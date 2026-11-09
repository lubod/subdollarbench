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
    /// R67 F2: the provider bill summed over the career's phases; None on
    /// careers that recorded no bill (the v2 grid ran unpinned).
    pub billed: Option<f64>,
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
    let billed: Option<f64> = {
        let vals: Vec<f64> = cells
            .iter()
            .filter_map(|c| c.rollup["billed_usd"].as_f64())
            .collect();
        (!vals.is_empty()).then(|| vals.iter().sum())
    };
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
        billed,
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
            // R71 F1 (R70-2): one-letter status classes and a terse title;
            // the legend explains the letters, the square keeps its number.
            let cls = match e.status.as_str() {
                "pass" => "p",
                "lite" => "l",
                "fail" => "f",
                "fault" => "x",
                "skipped" => "s",
                "running" => "r",
                _ => "n",
            };
            format!(
                "<span class=\"sq {cls}\" data-t=\"{}\">{}</span>",
                e.id, e.id
            )
        })
        .collect();
    format!("<div class=\"strip\">{squares}</div>")
}

fn legend_html() -> &'static str {
    "<div class=\"legend\"><span class=\"sq p\">p</span>pass<span class=\"sq l\">l</span>lite<span class=\"sq f\">f</span>fail<span class=\"sq x\">x</span>fault<span class=\"sq s\">s</span>skipped<span class=\"sq r\">r</span>running<span class=\"sq n\">n</span>not reached<span class=\"mut\">tN p = ticket N pass · tN k/m = k of m careers passed</span></div>"
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
        "<span title=\"{}\" data-short=\"out {}\">out {} · in {} (+{} cached)</span>",
        esc(&title),
        fmt_tokens(t.completion),
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
    // R70 F5 (R69-7): badges, never a text node that starts with "· ".
    let mut f: Vec<&str> = Vec::new();
    if c.over_budget {
        f.push("<span class=\"b warn\">over budget</span>");
    }
    if c.cap_reached {
        f.push("<span class=\"b\">budget cap</span>");
    }
    if c.over_wall {
        f.push("<span class=\"b warn\">over wall</span>");
    }
    f.join(" ")
}

/// R70 F1 (R69-2): the $0 rehearsals share one footnote with their count
/// and score range; a smoke cell keeps its own line.
fn unranked_note(cells: &[&CareerCell]) -> Option<String> {
    let rehearsals: Vec<&&CareerCell> = cells.iter().filter(|c| c.rehearsal).collect();
    let mut parts: Vec<String> = Vec::new();
    if !rehearsals.is_empty() {
        let mut scores: Vec<f64> = rehearsals.iter().map(|c| c.points).collect();
        scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        let lo = scores[0];
        let hi = *scores.last().unwrap();
        let max = rehearsals[0].max_points as u32;
        let score = if (hi - lo).abs() < 1e-9 {
            format!("{} / {max} each", fmt_pts(lo))
        } else {
            format!("{} - {} / {max}", fmt_pts(lo), fmt_pts(hi))
        };
        let n = rehearsals.len();
        parts.push(format!(
            "{n} $0 {} (the reference server; {score}) {} not ranked",
            if n == 1 { "rehearsal" } else { "rehearsals" },
            if n == 1 { "is" } else { "are" }
        ));
    }
    for c in cells.iter().filter(|c| c.smoke) {
        parts.push(format!(
            "Not ranked: a smoke cell scored {} / {}",
            fmt_pts(c.points),
            c.max_points as u32
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(". ") + ".")
    }
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
        format!("<span class=\"b effort\">effort {}</span>", esc(effort))
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
/// index is the ranked career's start-time position within its
/// model/harness/effort/rules group (R64-3/R65 F7); an unranked cell
/// follows the ranked ones.
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
            // R67 F3 (R64-3): only a paid career (ranked or unfinished) is
            // numbered; a rehearsal or smoke cell is not.
            let numbered_a = ca.key.current_rules && !ca.rehearsal && !ca.smoke;
            let numbered_b = cb.key.current_rules && !cb.rehearsal && !cb.smoke;
            numbered_b
                .cmp(&numbered_a)
                .then_with(|| ca.started.cmp(&cb.started))
                .then_with(|| ca.campaign_id.cmp(&cb.campaign_id))
        });
        let mut n = 0usize;
        for idx in g {
            let c = &cells[*idx];
            let date = c.started.get(..10).filter(|d| !d.is_empty()).unwrap_or("—");
            let label = if c.key.current_rules && !c.rehearsal && !c.smoke {
                n += 1;
                format!("{} · career {} · {}", short_model(&c.model), n, date)
            } else if c.rehearsal {
                format!("{} · rehearsal · {}", short_model(&c.model), date)
            } else if c.smoke {
                format!("{} · smoke · {}", short_model(&c.model), date)
            } else {
                format!("{} · unranked · {}", short_model(&c.model), date)
            };
            out.insert(c.campaign_id.clone(), label);
        }
    }
    out
}

/// Portal polish (operator, 2026-11-06): the provider prefix is noise on
/// the page; the full model id stays in the row's title.
fn short_model(model: &str) -> &str {
    model.strip_prefix("openrouter/").unwrap_or(model)
}

/// R70 F2: a display name breaks only after `/` or `-`, never mid-word.
fn break_name_html(s: &str) -> String {
    esc(s).replace('/', "/<wbr>").replace('-', "-<wbr>")
}

/// R65-1 / R70 F2: the short model name gets the same break points.
fn model_name_html(model: &str) -> String {
    break_name_html(short_model(model))
}

/// R69 F1: the head both pages share: description, Open Graph title and
/// description, an inline SVG favicon, then the stylesheet. The favicon's
/// SVG namespace is percent-encoded so the page never names an external
/// `http(s)://` URL.
fn head_html(title: &str, description: &str, css: &str) -> String {
    let icon = "data:image/svg+xml,%3Csvg xmlns='http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%230d1117'/%3E%3Ctext x='16' y='23' font-size='20' font-weight='700' text-anchor='middle' fill='%2356B4E9'%3E%24%3C/text%3E%3C/svg%3E";
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><meta name=\"description\" content=\"{}\"><meta property=\"og:title\" content=\"{}\"><meta property=\"og:description\" content=\"{}\"><link rel=\"icon\" href=\"{}\"><style>{}</style></head><body><main>",
        esc(title),
        esc(description),
        esc(title),
        esc(description),
        icon,
        css
    )
}

/// R69 F1: the title, one plain sentence and the updated time. The rules,
/// fingerprints, cap basis and price list live on the results page.
fn page_header(
    title: &str,
    lede: &str,
    link_href: &str,
    link_label: &str,
    updated: &str,
) -> String {
    format!(
        "<header class=\"page-head\"><div class=\"head-main\"><h1>{title}</h1><p class=\"lede\">{lede}</p></div><div class=\"head-side\"><a class=\"button\" href=\"{link_href}\">{link_label}</a><div class=\"mut updated\">Updated {updated}</div></div></header>"
    )
}

/// R69 F1: the footer both pages share.
fn footer_html(updated: &str) -> String {
    format!(
        "<footer class=\"page-foot\"><a href=\"https://github.com/lubod/subdollarbench\">GitHub</a><a href=\"https://github.com/lubod/subdollarbench/blob/master/docs/PROPOSAL-sdb-sql.md\">Methodology</a><span>Rules {}</span><span>Updated {}</span></footer>",
        esc(crate::report::rules::RULES_VERSION),
        esc(updated)
    )
}

/// R69 F2: the rank is large, the share is a small badge, and the badge's
/// title carries the full distribution.
fn rank_badge(g: &ModelGroup, stability: Option<&RankStability>) -> String {
    if !g.current {
        return "<div class=\"rank\">—</div>".to_string();
    }
    match stability {
        Some(s) => {
            let distribution: Vec<String> = s
                .counts
                .iter()
                .enumerate()
                .map(|(r, n)| format!("#{} {:.0}%", r + 1, *n as f64 / s.total as f64 * 100.0))
                .collect();
            format!(
                "<div class=\"rank\">#{}{}</div><span class=\"share\" title=\"{}\">{:.0}%</span>",
                s.most_likely + 1,
                if s.tie_with_above { "=" } else { "" },
                esc(&distribution.join(" · ")),
                s.share * 100.0
            )
        }
        None => format!("<div class=\"rank\">#{}</div>", g.rank_label()),
    }
}

/// R69 F2: the points hero bar: every career a dot, the best-worst range
/// shaded, the median marked, on the fixed 0-76 scale.
fn points_bar(cells: &[CareerCell], g: &ModelGroup, colour: &str) -> String {
    let w = 220.0;
    let h = 26.0;
    let pad = 8.0;
    let x = |points: f64| pad + (points / CAREER_MAX_POINTS).clamp(0.0, 1.0) * (w - 2.0 * pad);
    let values: Vec<String> = g
        .careers
        .iter()
        .map(|i| fmt_pts1(cells[*i].points))
        .collect();
    let mut s = format!(
        "<svg class=\"points-bar\" data-metric=\"points\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" role=\"img\">"
    );
    s.push_str(&format!(
        "<title>points: {} · median {}</title>",
        esc(&values.join(", ")),
        esc(&fmt_pts1(cells[g.median].points))
    ));
    s.push_str(&format!(
        "<line class=\"track\" x1=\"{pad:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>",
        h / 2.0,
        w - pad,
        h / 2.0
    ));
    let lo = cells[g.worst].points.min(cells[g.best].points);
    let hi = cells[g.best].points.max(cells[g.worst].points);
    s.push_str(&format!(
        "<rect class=\"range\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"10\" rx=\"5\" fill=\"{colour}\"/>",
        x(lo),
        h / 2.0 - 5.0,
        (x(hi) - x(lo)).max(2.0)
    ));
    for i in &g.careers {
        let c = &cells[*i];
        s.push_str(&format!(
            "<circle class=\"dot\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"3.6\" fill=\"{colour}\"><title>{} · {} pts</title></circle>",
            x(c.points),
            h / 2.0,
            esc(short_model(&c.model)),
            fmt_pts1(c.points)
        ));
    }
    let cx = x(cells[g.median].points);
    s.push_str(&format!(
        "<polygon class=\"med\" points=\"{cx:.1},{:.1} {:.1},{:.1} {cx:.1},{:.1} {:.1},{:.1}\"/>",
        h / 2.0 - 4.5,
        cx + 5.0,
        h / 2.0,
        h / 2.0 + 4.5,
        cx - 5.0,
        h / 2.0
    ));
    s.push_str("</svg>");
    s
}

/// R69 F2: a compact perf badge; the colour says above or below 1.0x and
/// the tooltip keeps every workload's numbers.
fn perf_badge(c: &CareerCell) -> String {
    let Some(perf) = &c.perf else {
        return "<span class=\"badge\">—</span>".to_string();
    };
    let title = esc(&perf_tooltip(perf));
    match perf.score {
        Some(score) => {
            let class = if score >= 1.0 {
                "badge good"
            } else {
                "badge warn"
            };
            format!("<span class=\"{class}\" title=\"{title}\">{score:.2}×</span>")
        }
        None => format!("<span class=\"badge\" title=\"{title}\">—</span>"),
    }
}

/// R69 F2: the secondary numbers, one muted line under the model name.
/// Their dot strips stay in the expanded careers.
fn secondary_line(c: &CareerCell) -> String {
    format!(
        "<div class=\"mut secondary\"><span class=\"col-highest\" title=\"the highest ladder ticket whose checks passed\">highest {}</span><span class=\"col-green\" title=\"tickets that passed with no regression in their acceptance suite\">kept {} green</span><span class=\"col-ppd\">{} pts/$</span><span class=\"col-duration\">{}</span><span class=\"col-turns\">{} turns</span><span class=\"col-tokens\">{}</span><span class=\"col-state\">{}</span></div>",
        reached_of(c),
        c.regression_free,
        ppd_of(c),
        duration_cell(c),
        turns_cell(c),
        token_cell(c),
        state_of(c)
    )
}

/// R69 F2: the four summary cards answer who wins, what it costs and what
/// is cheapest per point.
fn summary_cards(
    cells: &[CareerCell],
    groups: &[&ModelGroup],
    ranked: &[&CareerCell],
    colours: &HashMap<String, &'static str>,
) -> String {
    // R70 F3 (R69-5): the top model is the board's #1, not a fresh
    // max_by on points; the best points per dollar is each model's
    // median pts/$ across its careers.
    let top = groups.first();
    let best_ppd = groups
        .iter()
        .filter_map(|g| {
            let values: Vec<f64> = g
                .careers
                .iter()
                .filter_map(|i| cells[*i].points_per_dollar())
                .collect();
            median(&sorted_unique(values.into_iter())).map(|v| (*g, v))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    let cheapest = ranked
        .iter()
        .min_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(Ordering::Equal));
    let spend: f64 = ranked.iter().map(|c| c.cost).sum();
    let top_card = match top {
        Some(g) => format!(
            "<div class=\"card\" data-card=\"top-model\"><div class=\"k\">Top model</div><div class=\"v\">{}<b title=\"{}\">{}</b></div><div class=\"s\">{} / {} median</div></div>",
            swatch(&g.model, colours),
            esc(&g.model),
            model_name_html(&g.model),
            fmt_pts1(cells[g.median].points),
            cells[g.median].max_points as u32
        ),
        None => "<div class=\"card\" data-card=\"top-model\"><div class=\"k\">Top model</div><div class=\"v\">—</div><div class=\"s\">no ranked career yet</div></div>".to_string(),
    };
    let ppd_card = match best_ppd {
        Some((g, value)) => format!(
            "<div class=\"card\" data-card=\"best-ppd\"><div class=\"k\">Best median points per dollar</div><div class=\"v\">{}<b title=\"{}\">{}</b></div><div class=\"s\">{value:.1} pts/$</div></div>",
            swatch(&g.model, colours),
            esc(&g.model),
            model_name_html(&g.model)
        ),
        None => "<div class=\"card\" data-card=\"best-ppd\"><div class=\"k\">Best points per dollar</div><div class=\"v\">—</div><div class=\"s\">no priced career yet</div></div>".to_string(),
    };
    let cheap_card = match cheapest {
        Some(c) => format!(
            "<div class=\"card\" data-card=\"cheapest-career\"><div class=\"k\">Cheapest career</div><div class=\"v\">{}<b title=\"{}\">{}</b></div><div class=\"s\">${:.2}</div></div>",
            swatch(&c.model, colours),
            esc(&c.model),
            model_name_html(&c.model),
            c.cost
        ),
        None => "<div class=\"card\" data-card=\"cheapest-career\"><div class=\"k\">Cheapest career</div><div class=\"v\">—</div><div class=\"s\">no ranked career yet</div></div>".to_string(),
    };
    let careers = format!(
        "<div class=\"card\" data-card=\"careers\"><div class=\"k\">Careers</div><div class=\"v\">{} across {} {}</div><div class=\"s\">${spend:.2} spent</div></div>",
        ranked.len(),
        groups.len(),
        if groups.len() == 1 { "model" } else { "models" }
    );
    format!("<div class=\"cards\">{top_card}{ppd_card}{cheap_card}{careers}</div>")
}

/// R69 F1/F3: a model name with its shared colour square, for a table cell.
fn model_cell(model: &str, colours: &HashMap<String, &'static str>) -> String {
    format!(
        "<td title=\"{}\">{}<span class=\"mname\">{}</span></td>",
        esc(model),
        swatch(model, colours),
        model_name_html(model)
    )
}

/// R69 F3: the sticky section nav; every target id exists below.
fn section_nav() -> &'static str {
    "<nav class=\"section-nav\" aria-label=\"Sections\"><a href=\"#measured\">What it measured</a><a href=\"#points\">Points vs $</a><a href=\"#survival\">Survival</a><a href=\"#tickets\">Per-ticket</a><a href=\"#head-to-head\">Head-to-head</a><a href=\"#variance\">Variance</a><a href=\"#ranks\">Ranks</a></nav>"
}

/// R70 F5 (R69-7): the expanded career table's 12 columns are named.
fn career_table_head() -> &'static str {
    "<thead><tr><th></th><th>Career</th><th>Tickets</th><th>Highest ticket</th><th>Tickets kept green</th><th class=\"num\">Points</th><th class=\"num\">Perf</th><th class=\"num\">Spend</th><th class=\"num\">Pts / $</th><th class=\"num\">Duration</th><th class=\"num\">Turns</th><th>Tokens out</th><th>State</th></tr></thead><tbody>"
}

fn career_row(
    c: &CareerCell,
    name: &str,
    phase_label: Option<&str>,
    extra_class: &str,
    model_effort: Option<&str>,
) -> String {
    let label = phase_label
        .map(|p| format!("<div class=\"mut\">{p}</div>"))
        .unwrap_or_default();
    let class = if extra_class.is_empty() {
        "career-row".to_string()
    } else {
        format!("career-row {extra_class}")
    };
    let id = esc(&c.campaign_id);
    let fp = esc(c.fingerprints.first().map(String::as_str).unwrap_or("—"));
    // R72 F2: the model row already names the model and its effort; the
    // career label is only `career N · date` on one line. The title keeps
    // the full model id.
    let compact = name.split_once(" · ").map(|(_, rest)| rest).unwrap_or(name);
    let label_html = esc(compact);
    let title = format!("{id} · {fp} · {}", esc(&c.model));
    let show_effort = model_effort != Some(c.effort.as_str());
    let notes = [
        if show_effort {
            effort_note(&c.effort)
        } else {
            String::new()
        },
        flags_of(c),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    format!(
        "<tr class=\"{class}\"><td class=\"num\"></td><td class=\"col-career\">{label}<a href=\"results.html#{id}\" title=\"{title}\"><b>{label_html}</b></a><div class=\"mut\">{notes}</div></td><td class=\"col-tickets\">{}</td><td class=\"col-highest\" data-label=\"highest\">{}</td><td class=\"num col-green\" data-label=\"kept\">{}</td><td class=\"num col-points\" data-label=\"points\">{} / {}</td><td class=\"num col-perf\" data-label=\"perf\">{}</td><td class=\"num col-spend\" data-label=\"spend\">${:.2}</td><td class=\"num col-ppd\" data-label=\"pts/$\">{}</td><td class=\"num col-duration\" data-label=\"duration\">{}</td><td class=\"num col-turns\" data-label=\"turns\">{}</td><td class=\"col-tokens\" data-label=\"tokens\">{}</td><td class=\"col-state\">{}</td></tr>",
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
        "<div class=\"val\">{}</div>{}",
        median.map(fmt).unwrap_or_else(|| "—".to_string()),
        dot_strip(values, median, lo, hi, log, label)
    )
}

fn model_row(
    cells: &[CareerCell],
    g: &ModelGroup,
    labels: &HashMap<String, String>,
    stability: Option<&RankStability>,
    scales: &DotScales,
    colours: &HashMap<String, &'static str>,
    html: &mut String,
) {
    let med = &cells[g.median];
    let best = &cells[g.best];
    let worst = &cells[g.worst];
    let colour = colours.get(&g.model).copied().unwrap_or(palette(0));
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
        inner.push_str(&career_row(c, name, None, "", Some(&g.effort)));
    }
    let word = if g.careers.len() == 1 {
        "career"
    } else {
        "careers"
    };
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
    // R69 F2: the points bar is the hero; the other metrics keep their dot
    // strips in the expanded view.
    let bar = points_bar(cells, g, colour);
    let strips = format!(
        "<div class=\"more-strips\"><div class=\"strip-metric\"><span class=\"mut\">perf</span>{}</div><div class=\"strip-metric\"><span class=\"mut\">$</span>{}</div><div class=\"strip-metric\"><span class=\"mut\">duration</span>{}</div><div class=\"strip-metric\"><span class=\"mut\">turns</span>{}</div><div class=\"strip-metric\"><span class=\"mut\">tokens out</span>{}</div></div>",
        strip_cell(&perfs, perf_med, 0.1, 10.0, true, "perf", fmt_perf),
        strip_cell(
            &costs,
            Some(med.cost),
            0.0,
            CAREER_CAP_USD,
            false,
            "$",
            fmt_dollars
        ),
        strip_cell(
            &walls,
            wall_med,
            0.0,
            scales.wall,
            false,
            "duration",
            fmt_duration_value
        ),
        strip_cell(
            &turn_vals,
            turn_med,
            0.0,
            scales.turns,
            false,
            "turns",
            fmt_turns_value
        ),
        strip_cell(
            &token_vals,
            token_med,
            0.0,
            scales.tokens,
            false,
            "tokens out",
            fmt_token_value
        )
    );
    let agreement = ticket_agreement(cells, g);
    let link = agreement_link(cells, g);
    let more = format!(
        "<details class=\"more\"><summary>×{} {word} ▸</summary><div class=\"agree\" id=\"agree-{}\">{agreement}</div>{strips}<div class=\"table-scroll\"><table class=\"career-table\">{}{inner}</table></div></details>",
        g.careers.len(),
        g.median,
        career_table_head()
    );
    html.push_str(&format!(
        "<tr class=\"model-row\"><td class=\"num col-rank\">{}</td><td class=\"col-model\"><div class=\"model-name\">{}<b title=\"{}\">{}</b>{}</div>{}<div class=\"mut range\">{} - {} · {} {word}</div></td><td class=\"num col-points\"><div class=\"hero\">{} / {}</div>{bar}</td><td class=\"num col-perf\">{}</td><td class=\"num col-spend\"><span class=\"badge spend\" title=\"effective spend at list price\">${:.2}</span></td><td class=\"col-agree\">{link}</td></tr>",
        rank_badge(g, stability),
        swatch(&g.model, colours),
        esc(&g.model),
        model_name_html(&g.model),
        [mark_badge(med), effort_note(&g.effort)]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        secondary_line(med),
        fmt_pts(worst.points),
        fmt_pts(best.points),
        g.careers.len(),
        fmt_pts1(med.points),
        med.max_points as u32,
        perf_badge(med),
        med.cost
    ));
    // R72 F1: the careers span the board's six columns in their own row.
    html.push_str(&format!(
        "<tr class=\"more-row\"><td colspan=\"6\">{more}</td></tr>"
    ));
}

fn render_board(cells: &[CareerCell]) -> String {
    let groups = model_groups(cells);
    let colours = model_colours(cells);
    // R64 F1: the header counts only careers that rank: finished, current
    // rules, never a $0 rehearsal or a smoke cell. An older rule set is
    // incomparable and counts nowhere.
    let ranked: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules && !c.rehearsal && !c.smoke)
        .collect();
    let current_groups: Vec<&ModelGroup> = groups.iter().filter(|g| g.current).collect();
    // Portal polish (operator, 2026-11-06: "remove old rule set results"):
    // the board shows the current rules only. A $0 rehearsal or a smoke
    // cell is a footnote under the table, not a row.
    let unfinished_current: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| !c.finished && c.key.current_rules && !c.rehearsal && !c.smoke)
        .collect();
    let marked: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.key.current_rules && (c.rehearsal || c.smoke))
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
        model_row(cells, g, &labels, stab, &scales, &colours, &mut rows);
    }
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let mut h = head_html(
        "SubDollarBench — sdb-sql campaign careers",
        "Budget LLMs build a SQL database, ticket by ticket, for at most $0.99 per phase. Graded against SQLite.",
        include_str!("portal_board.css"),
    );
    h.push_str(&page_header(
        "💰 SubDollarBench <span class=\"page-sub\">sdb-sql campaign careers</span>",
        "Budget LLMs build a SQL database, ticket by ticket, for at most $0.99 per phase. Graded against SQLite.",
        "results.html",
        "📈 Results",
        &now,
    ));
    // R64 F3 (M3): a grid with a career still running says so.
    let grid_total = ranked.len() + unfinished_current.len();
    if ranked.len() < grid_total {
        h.push_str(&format!(
            "<div class=\"banner\">Grid in progress: {} of {} careers finished; ranks are provisional.</div>",
            ranked.len(),
            grid_total
        ));
    }
    h.push_str(&summary_cards(cells, &current_groups, &ranked, &colours));
    if !current_groups.is_empty() && stability.is_none() {
        h.push_str("<div class=\"mut\">Rank stability: too many combinations to enumerate; ranks follow the board order.</div>");
    }
    h.push_str("<div class=\"wrap\"><table class=\"board-table\"><thead><tr><th class=\"col-rank\">#</th><th class=\"col-model\">Model</th><th class=\"num col-points\">Points <span class=\"mut\">(of 76)</span></th><th class=\"num col-perf\">Perf</th><th class=\"num col-spend\">Spend</th><th class=\"col-agree\">Tickets agreed</th></tr></thead><tbody>");
    if rows.is_empty() {
        h.push_str("<tr><td colspan=\"6\" class=\"mut\">No sdb-sql careers yet (run one with <code>scripts/sql-cell.sh &lt;model&gt;</code>).</td></tr>");
    } else {
        h.push_str(&rows);
    }
    h.push_str("</tbody></table>");
    h.push_str(legend_html());
    h.push_str("</div>");
    if let Some(note) = unranked_note(&marked) {
        h.push_str(&format!("<div class=\"mut note\">{note}</div>"));
    }
    h.push_str("<section class=\"methodology\"><h2>How this board is ranked</h2><ul><li><b>Careers</b>: a career is 4 phases and 21 tickets (0-20); one row per leaf phase cell of a chain. A model's row is its <b>median career</b> by the one shared board order; the best and worst careers are named next to it and the full set sits in <b>×N careers ▸</b>.</li><li><b>Points</b>: difficulty-weighted score. On equal points the <b>Perf</b> score decides (higher first; a career with no score ranks below one with a score), then <b>Pts / $</b> divides by effective spend. The perf contract is ticket 0's <a href=\"https://github.com/lubod/subdollarbench/blob/master/campaigns/sdb-sql/stages/0_genesis/prompt.md\">Performance section</a>; hover a Perf badge for each workload's reference, candidate, ratio and any floor.</li><li><b>Rank</b>: <code>#1</code> with a <code>70%</code> badge means the model holds rank 1 in 70% of all combinations of one career per model, ranked by the board order (points, then perf, then points per dollar); hover the badge for the full distribution. <code>=</code> marks two models that most likely share a rank.</li><li>Only the current rule set is shown. A career still running is counted in the in-progress banner, never shown as a row; a $0 rehearsal is a footnote.</li><li>Hidden, poisoned and harness-fault cells are never published. The ladder squares merge each phase cell's own tickets; the legend sits under the table.</li></ul></section>");
    h.push_str(&footer_html(&now));
    h.push_str("</main></body></html>");
    h
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
) -> String {
    let w = 84.0;
    let h = 16.0;
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
    // R71 F1 (R70-2): the career table carries the values; the strip keeps
    // only its metric name, so the repeated SVG stays small.
    let mut s = format!(
        "<svg class=\"dots\" data-metric=\"{}\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" role=\"img\" aria-label=\"{}\">",
        esc(label),
        esc(label)
    );
    s.push_str(&format!(
        "<line class=\"baseline\" x1=\"{pad:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>",
        h / 2.0,
        w - pad,
        h / 2.0
    ));
    for v in values {
        let vv = v.unwrap_or(lo);
        s.push_str(&format!(
            "<circle class=\"d{}\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"3.2\"/>",
            if v.is_none() { " m" } else { "" },
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
            cx + 4.5,
            h / 2.0,
            cx,
            h / 2.0 + 3.5,
            cx - 4.5,
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
    format!("${v:.2}")
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
        "$" => format!("${v:.2}"),
        "duration" => fmt_hours(v),
        "turns" => fmt_count(v.round() as u64),
        "tokens out" => fmt_tokens(v.round() as u64),
        _ => format!("{v:.1}"),
    }
}

/// R71 F1 (R70-2): the passed counts and the summary shared by the one
/// agreement strip and its row link.
fn agreement_passed(cells: &[CareerCell], g: &ModelGroup) -> [usize; 21] {
    let mut passed = [0usize; 21];
    for i in &g.careers {
        for rec in unique_stage_records(&cells[*i].stage_records) {
            if rec.passed && rec.id <= 20 {
                passed[rec.id as usize] += 1;
            }
        }
    }
    passed
}

fn agreement_summary(passed: &[usize; 21], n: usize) -> (usize, Vec<String>) {
    let mut agree = 0;
    let mut differs: Vec<String> = Vec::new();
    for (id, p) in passed.iter().enumerate().skip(1) {
        if *p == 0 || *p == n {
            agree += 1;
        } else {
            differs.push(id.to_string());
        }
    }
    (agree, differs)
}

/// R64 F6 / R71 F1: the one 21-square agreement strip, inside `<details>`.
/// The row's cell links to it instead of carrying a second copy.
fn ticket_agreement(cells: &[CareerCell], g: &ModelGroup) -> String {
    let n = g.careers.len();
    let passed = agreement_passed(cells, g);
    let (agree, differs) = agreement_summary(&passed, n);
    let mut squares = String::new();
    for (id, p) in passed.iter().enumerate() {
        let p = *p;
        let cls = if p == 0 {
            "a0"
        } else if p == n {
            "a3"
        } else if p * 2 > n {
            "a2"
        } else {
            "a1"
        };
        squares.push_str(&format!(
            "<span class=\"sq {cls}\" title=\"t{id} {p}/{n}\">{id}</span>"
        ));
    }
    let diff = if differs.is_empty() {
        String::new()
    } else {
        format!(" · differs: {}", differs.join(", "))
    };
    format!("<div class=\"strip\">{squares}</div><div class=\"mut\">agree {agree}/20{diff}</div>")
}

/// R71 F1: the row's compact link to the one strip.
fn agreement_link(cells: &[CareerCell], g: &ModelGroup) -> String {
    let n = g.careers.len();
    let passed = agreement_passed(cells, g);
    let (agree, differs) = agreement_summary(&passed, n);
    let diff = if differs.is_empty() {
        String::new()
    } else {
        format!(" · differs: {}", differs.join(", "))
    };
    format!(
        "<a class=\"agree-link\" href=\"#agree-{}\" title=\"open the 21-square strip\">agree {agree}/20{diff}</a>",
        g.median
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

/// R69 F1 / R71 F3: the model palette, assigned in board order and shared
/// by the board, both charts, their legends and every results table. Twelve
/// colours, all at least 3:1 against `--card`; the second channel (marker
/// shape and dash) keeps same-family colours apart.
pub const PALETTE_LEN: usize = 12;

fn palette(i: usize) -> &'static str {
    const COLORS: [&str; PALETTE_LEN] = [
        "#56B4E9", "#E69F00", "#009E73", "#F0E442", "#0072B2", "#D55E00", "#CC79A7", "#BBBBBB",
        "#8C6D31", "#CAB2D6", "#44AA99", "#AA4499",
    ];
    COLORS[i % PALETTE_LEN]
}

/// R71 F3: the hue family, for the "same family, different shape" rule.
fn palette_family(i: usize) -> &'static str {
    const FAMILIES: [&str; PALETTE_LEN] = [
        "blue", "orange", "green", "yellow", "blue", "orange", "pink", "grey", "brown", "purple",
        "green", "purple",
    ];
    FAMILIES[i % PALETTE_LEN]
}

/// R71 F3: one marker shape per model, assigned in board order.
fn model_shape(i: usize) -> &'static str {
    const SHAPES: [&str; PALETTE_LEN] = [
        "circle",
        "square",
        "triangle",
        "diamond",
        "star",
        "plus",
        "hexagon",
        "pentagon",
        "ring",
        "bar",
        "triangle-down",
        "cross",
    ];
    SHAPES[i % PALETTE_LEN]
}

/// R71 F3: one survival dash per model, assigned in board order.
fn model_dash(i: usize) -> &'static str {
    const DASHES: [&str; PALETTE_LEN] = [
        "", "6 3", "2 3", "8 3 2 3", "4 4", "1 3", "10 4", "6 3 2 3", "3 2 8 2", "12 4", "2 6",
        "5 2 1 2",
    ];
    DASHES[i % PALETTE_LEN]
}

/// R71 F3: the model order that assigns colour, shape and dash.
fn model_order(cells: &[CareerCell]) -> HashMap<String, usize> {
    let mut order: Vec<String> = Vec::new();
    for g in model_groups(cells) {
        if !order.iter().any(|m| m == &g.model) {
            order.push(g.model.clone());
        }
    }
    order.into_iter().enumerate().map(|(i, m)| (m, i)).collect()
}

/// R69 F1: one colour per model, assigned in board order.
fn model_colours(cells: &[CareerCell]) -> HashMap<String, &'static str> {
    model_order(cells)
        .into_iter()
        .map(|(m, i)| (m, palette(i)))
        .collect()
}

/// R72 F4 (R71-3): equalise the shapes' visual area so no model looks
/// smaller. The factor scales the nominal radius; a circle is the base.
fn shape_scale(shape: &str) -> f64 {
    match shape {
        "square" => 0.886,
        "triangle" | "triangle-down" => 1.55,
        "diamond" => 1.25,
        "star" => 1.25,
        "plus" | "cross" => 1.2,
        "hexagon" => 1.1,
        "pentagon" => 1.15,
        "ring" => 1.1,
        "bar" => 1.3,
        _ => 1.0,
    }
}

/// R71 F3: one SVG marker in the model's shape and colour.
fn mark_svg(shape: &str, colour: &str, cx: f64, cy: f64, r: f64) -> String {
    let r = r * shape_scale(shape);
    let poly = |n: usize, rot: f64| -> String {
        (0..n)
            .map(|i| {
                let a = rot + i as f64 * std::f64::consts::TAU / n as f64;
                format!("{:.1},{:.1}", cx + r * a.cos(), cy + r * a.sin())
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    match shape {
        "circle" => format!(
            "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{r:.1}\" fill=\"{colour}\"/>"
        ),
        "square" => format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"1\" fill=\"{colour}\"/>",
            cx - r,
            cy - r,
            2.0 * r,
            2.0 * r
        ),
        "triangle" => format!(
            "<polygon points=\"{}\" fill=\"{colour}\"/>",
            poly(3, -std::f64::consts::FRAC_PI_2)
        ),
        "triangle-down" => format!(
            "<polygon points=\"{}\" fill=\"{colour}\"/>",
            poly(3, std::f64::consts::FRAC_PI_2)
        ),
        "diamond" => format!(
            "<polygon points=\"{}\" fill=\"{colour}\"/>",
            poly(4, -std::f64::consts::FRAC_PI_2)
        ),
        "star" => {
            let points = (0..10)
                .map(|i| {
                    let a = -std::f64::consts::FRAC_PI_2
                        + i as f64 * std::f64::consts::PI / 5.0;
                    let rr = if i % 2 == 0 { r } else { r * 0.45 };
                    format!("{:.1},{:.1}", cx + rr * a.cos(), cy + rr * a.sin())
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("<polygon points=\"{points}\" fill=\"{colour}\"/>")
        }
        "plus" => format!(
            "<path d=\"M {:.1} {cy:.1} H {:.1} M {cx:.1} {:.1} V {:.1}\" stroke=\"{colour}\" stroke-width=\"2\" fill=\"none\"/>",
            cx - r,
            cx + r,
            cy - r,
            cy + r
        ),
        "cross" => format!(
            "<path d=\"M {:.1} {:.1} L {:.1} {:.1} M {:.1} {:.1} L {:.1} {:.1}\" stroke=\"{colour}\" stroke-width=\"2\" fill=\"none\"/>",
            cx - r,
            cy - r,
            cx + r,
            cy + r,
            cx + r,
            cy - r,
            cx - r,
            cy + r
        ),
        "hexagon" => format!(
            "<polygon points=\"{}\" fill=\"{colour}\"/>",
            poly(6, -std::f64::consts::FRAC_PI_2)
        ),
        "pentagon" => format!(
            "<polygon points=\"{}\" fill=\"{colour}\"/>",
            poly(5, -std::f64::consts::FRAC_PI_2)
        ),
        "ring" => format!(
            "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{r:.1}\" fill=\"none\" stroke=\"{colour}\" stroke-width=\"2\"/>"
        ),
        "bar" => format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"3\" rx=\"1\" fill=\"{colour}\"/>",
            cx - r,
            cy - 1.5,
            2.0 * r
        ),
        _ => format!(
            "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{r:.1}\" fill=\"{colour}\"/>"
        ),
    }
}

/// R69 F1: a model's colour square, used beside every name on both pages.
fn swatch(model: &str, colours: &HashMap<String, &'static str>) -> String {
    let colour = colours.get(model).copied().unwrap_or(palette(0));
    format!("<span class=\"swatch\" style=\"background:{colour}\"></span>")
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

/// R65 F6 / R69 F3 / R71 F3: one legend above each chart. The shape
/// legend shows the model's marker; the dash legend its survival line.
fn chart_legend(
    models: &[String],
    colours: &HashMap<String, &'static str>,
    indices: &HashMap<String, usize>,
    mode: &str,
) -> String {
    let mut s = String::from("<div class=\"legend chart-legend\">");
    for m in models {
        let idx = indices.get(m).copied().unwrap_or(0);
        let colour = colours.get(m).copied().unwrap_or(palette(idx));
        if mode == "shape" {
            let shape = model_shape(idx);
            s.push_str(&format!(
                "<span class=\"key\" data-model=\"{}\" data-shape=\"{}\" data-family=\"{}\" data-colour=\"{}\"><svg class=\"key-shape\" viewBox=\"0 0 14 14\" aria-hidden=\"true\">{}</svg><span title=\"{}\">{}</span></span>",
                esc(m),
                shape,
                palette_family(idx),
                colour,
                mark_svg(shape, colour, 7.0, 7.0, 4.0),
                esc(m),
                model_name_html(m)
            ));
        } else {
            let dash = model_dash(idx);
            let dash_attr = if dash.is_empty() {
                String::new()
            } else {
                format!(" stroke-dasharray=\"{dash}\"")
            };
            s.push_str(&format!(
                "<span class=\"key\" data-model=\"{}\" data-dash=\"{}\" data-colour=\"{}\"><svg class=\"key-dash\" viewBox=\"0 0 24 8\" aria-hidden=\"true\"><line x1=\"0\" y1=\"4\" x2=\"24\" y2=\"4\" stroke=\"{}\" stroke-width=\"2\"{dash_attr}/></svg><span title=\"{}\">{}</span></span>",
                esc(m),
                esc(dash),
                colour,
                colour,
                esc(m),
                model_name_html(m)
            ));
        }
    }
    s.push_str("</div>");
    s
}

/// R69 F3: the scatter has axis titles, gridlines at round values, the
/// shared model palette and a 760x560 viewBox (560 px tall on desktop).
fn svg_scatter(
    cars: &[&CareerCell],
    colours: &HashMap<String, &'static str>,
    indices: &HashMap<String, usize>,
) -> String {
    let w = 760.0;
    let h = 560.0;
    let l = 74.0;
    let r = 24.0;
    let t = 24.0;
    let b = 82.0;
    let pw = w - l - r;
    let ph = h - t - b;
    let x = |cost: f64| l + (cost / CAREER_CAP_USD).clamp(0.0, 1.0) * pw;
    let y = |points: f64| t + (1.0 - (points / CAREER_MAX_POINTS).clamp(0.0, 1.0)) * ph;
    let mut s = format!(
        "<svg class=\"chart\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"points against dollars\">"
    );
    s.push_str(&format!(
        "<rect class=\"chart-bg\" x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/>"
    ));
    // Horizontal gridlines at 0, 19, 38, 57 and 76 points.
    for (frac, label) in [
        (0.0, "0"),
        (0.25, "19"),
        (0.5, "38"),
        (0.75, "57"),
        (1.0, "76"),
    ] {
        let yy = t + (1.0 - frac) * ph;
        s.push_str(&format!(
            "<line class=\"grid\" x1=\"{l}\" y1=\"{yy:.1}\" x2=\"{:.1}\" y2=\"{yy:.1}\"/><text class=\"axis\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{label}</text>",
            l + pw,
            l - 10.0,
            yy + 4.0
        ));
    }
    // Vertical gridlines every $0.99 up to the $3.96 career cap.
    for (i, label) in ["$0", "$0.99", "$1.98", "$2.97", "$3.96"]
        .iter()
        .enumerate()
    {
        let frac = i as f64 / 4.0;
        let xx = l + frac * pw;
        let anchor = if i == 0 {
            "start"
        } else if i == 4 {
            "end"
        } else {
            "middle"
        };
        s.push_str(&format!(
            "<line class=\"grid\" x1=\"{xx:.1}\" y1=\"{t}\" x2=\"{xx:.1}\" y2=\"{:.1}\"/><text class=\"axis\" x=\"{xx:.1}\" y=\"{:.1}\" text-anchor=\"{anchor}\">{label}</text>",
            t + ph,
            t + ph + 18.0
        ));
    }
    s.push_str(&format!(
        "<line class=\"axis-line\" x1=\"{l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/><line class=\"axis-line\" x1=\"{l}\" y1=\"{t}\" x2=\"{l}\" y2=\"{}\"/>",
        t + ph,
        l + pw,
        t + ph,
        t + ph
    ));
    s.push_str(&format!(
        "<text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">$ per career</text>",
        l + pw / 2.0,
        h - 18.0
    ));
    s.push_str(&format!(
        "<text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" transform=\"rotate(-90 {:.1} {:.1})\">points (of 76)</text>",
        20.0,
        t + ph / 2.0,
        20.0,
        t + ph / 2.0
    ));
    let frontier = pareto(cars);
    let line: String = frontier
        .iter()
        .map(|c| format!("{:.1},{:.1}", x(c.cost), y(c.points)))
        .collect::<Vec<_>>()
        .join(" ");
    if !line.is_empty() {
        s.push_str(&format!(
            "<polyline class=\"pareto-frontier\" points=\"{line}\" fill=\"none\"/>"
        ));
    }
    for c in cars {
        let idx = indices.get(&c.model).copied().unwrap_or(0);
        let shape = model_shape(idx);
        let colour = colours.get(&c.model).copied().unwrap_or(palette(idx));
        let mark = mark_svg(shape, colour, x(c.cost), y(c.points), 5.0);
        s.push_str(&format!(
            "<g class=\"career-dot\" data-model=\"{}\" data-shape=\"{}\" data-family=\"{}\" data-points=\"{:.1}\" data-cost=\"{:.4}\">{mark}<title>{}: {:.1} pts · ${:.2}</title></g>",
            esc(&c.model),
            shape,
            palette_family(idx),
            c.points,
            c.cost,
            esc(short_model(&c.model)),
            c.points,
            c.cost
        ));
    }
    s.push_str("</svg>");
    s
}

/// R69 F3: the survival chart has axis titles, gridlines every 25% and
/// every five tickets, the shared palette and a 760x560 viewBox.
fn svg_survival(
    series: &[(String, Vec<f64>)],
    colours: &HashMap<String, &'static str>,
    indices: &HashMap<String, usize>,
) -> String {
    let w = 760.0;
    let h = 560.0;
    let l = 74.0;
    let r = 24.0;
    let t = 24.0;
    let b = 82.0;
    let pw = w - l - r;
    let ph = h - t - b;
    let mut s = format!(
        "<svg class=\"chart\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"ticket survival\">"
    );
    s.push_str(&format!(
        "<rect class=\"chart-bg\" x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/>"
    ));
    for (frac, label) in [
        (0.0, "0%"),
        (0.25, "25%"),
        (0.5, "50%"),
        (0.75, "75%"),
        (1.0, "100%"),
    ] {
        let yy = t + (1.0 - frac) * ph;
        s.push_str(&format!(
            "<line class=\"grid\" x1=\"{l}\" y1=\"{yy:.1}\" x2=\"{:.1}\" y2=\"{yy:.1}\"/><text class=\"axis\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{label}</text>",
            l + pw,
            l - 10.0,
            yy + 4.0
        ));
    }
    for i in 0..=4 {
        let ticket = i * 5;
        let xx = l + (ticket as f64 / 20.0) * pw;
        let anchor = if i == 0 {
            "start"
        } else if i == 4 {
            "end"
        } else {
            "middle"
        };
        s.push_str(&format!(
            "<line class=\"grid\" x1=\"{xx:.1}\" y1=\"{t}\" x2=\"{xx:.1}\" y2=\"{:.1}\"/><text class=\"axis\" x=\"{xx:.1}\" y=\"{:.1}\" text-anchor=\"{anchor}\">{ticket}</text>",
            t + ph,
            t + ph + 18.0
        ));
    }
    s.push_str(&format!(
        "<line class=\"axis-line\" x1=\"{l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/><line class=\"axis-line\" x1=\"{l}\" y1=\"{t}\" x2=\"{l}\" y2=\"{}\"/>",
        t + ph,
        l + pw,
        t + ph,
        t + ph
    ));
    s.push_str(&format!(
        "<text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">ticket</text>",
        l + pw / 2.0,
        h - 18.0
    ));
    s.push_str(&format!(
        "<text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" transform=\"rotate(-90 {:.1} {:.1})\">P(highest passed ≥ i)</text>",
        20.0,
        t + ph / 2.0,
        20.0,
        t + ph / 2.0
    ));
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
        let idx = indices.get(model).copied().unwrap_or(mi);
        let dash = model_dash(idx);
        let dash_attr = if dash.is_empty() {
            String::new()
        } else {
            format!(" stroke-dasharray=\"{dash}\"")
        };
        let colour = colours.get(model).copied().unwrap_or(palette(idx));
        s.push_str(&format!(
            "<polyline class=\"survival-line\" data-model=\"{}\" data-dash=\"{}\" points=\"{pts}\" fill=\"none\" stroke=\"{}\" stroke-width=\"2\"{dash_attr}><title>{} · P(highest passed ticket ≥ i)</title></polyline>",
            esc(model),
            esc(dash),
            colour,
            esc(short_model(model))
        ));
    }
    s.push_str("</svg>");
    s
}

fn render_results(cells: &[CareerCell]) -> String {
    let labels = career_labels(cells);
    let groups = model_groups(cells);
    let colours = model_colours(cells);
    let indices = model_order(cells);
    let current_groups: Vec<&ModelGroup> = groups.iter().filter(|g| g.current).collect();
    let current: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules)
        .collect();
    // R69 F1: the charts and tables follow the board's model order, so a
    // model keeps its colour and its place on both pages.
    let mut models: Vec<String> = Vec::new();
    for g in &current_groups {
        if !models.iter().any(|m| m == &g.model) {
            models.push(g.model.clone());
        }
    }
    for c in &current {
        if !models.iter().any(|m| m == &c.model) {
            models.push(c.model.clone());
        }
    }
    let caps = current.len();
    let binary_fingerprint = crate::report::rules::fingerprint();
    let mut recorded: Vec<String> = current
        .iter()
        .flat_map(|c| c.fingerprints.iter().cloned())
        .collect();
    recorded.sort();
    recorded.dedup();
    // R66 F2: one board per rules version. Every recorded fingerprint is
    // listed with its career count; a difference is normal (the harness or
    // the price list moved), not a warning.
    let mut fingerprint_counts: HashMap<&str, usize> = HashMap::new();
    for c in &current {
        for f in &c.fingerprints {
            *fingerprint_counts.entry(f.as_str()).or_default() += 1;
        }
    }
    let fingerprint_rows = if recorded.is_empty() {
        "<tr><th>Fingerprint (recorded)</th><td><span class=\"b warn\">no recorded fingerprint</span></td></tr>".to_string()
    } else {
        let mut rows = String::from("<tr><th>Graded under</th><td>");
        for (i, f) in recorded.iter().enumerate() {
            if i > 0 {
                rows.push_str("<br>");
            }
            let n = fingerprint_counts.get(f.as_str()).copied().unwrap_or(0);
            rows.push_str(&format!(
                "<code>{}</code> ({} {})",
                esc(f),
                n,
                if n == 1 { "career" } else { "careers" }
            ));
        }
        rows.push_str("</td></tr>");
        rows.push_str(&format!(
            "<tr><th>Page built with</th><td><code>{}</code></td></tr>",
            esc(&binary_fingerprint)
        ));
        rows
    };
    let fingerprint_note = if recorded.len() > 1 {
        "<div class=\"mut\">Fingerprints differ when the harness or the price list changed between careers; the rules version, and so the grading, is the same.</div>".to_string()
    } else if recorded.len() == 1 && recorded[0] != binary_fingerprint {
        "<div class=\"mut\">The harness changed after these careers ran; their grades are as recorded.</div>".to_string()
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
                format!(
                    "{}<span title=\"{}\">{}</span> — none",
                    swatch(m, &colours),
                    esc(m),
                    model_name_html(m)
                )
            } else {
                format!(
                    "{}<span title=\"{}\">{}</span> — {}",
                    swatch(m, &colours),
                    esc(m),
                    model_name_html(m),
                    esc(detail)
                )
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    // R69 F1: the price list and the cap basis moved here from the board.
    let prices = format!(
        "base rates per call from model_prices.json (snapshot {}); overrides: {}",
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
        let list: f64 = cs.iter().map(|c| c.cost).sum();
        let billed: Option<f64> = cs.iter().filter_map(|c| c.billed).reduce(|a, b| a + b);
        let spend = match billed {
            Some(b) => format!("${list:.2} / ${b:.2}"),
            None => format!("${list:.2} / —"),
        };
        metrics.push_str(&format!(
            "<tr>{}<td class=\"num\">${:.2} / ${:.2} / ${:.2}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            model_cell(model, &colours),
            costs.first().copied().unwrap_or(0.0),
            median(&costs).unwrap_or(0.0),
            costs.last().copied().unwrap_or(0.0),
            rate.map(|r| format!("{:.1}%", r * 100.0))
                .unwrap_or_else(|| "—".to_string()),
            spend
        ));
    }

    let scatter = svg_scatter(&current, &colours, &indices);
    let survival = svg_survival(&series, &colours, &indices);
    let model_order: HashMap<&str, usize> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.model.as_str(), i))
        .collect();
    let mut scatter_cells: Vec<&CareerCell> = current.clone();
    scatter_cells.sort_by(|a, b| {
        let oa = model_order
            .get(a.model.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        let ob = model_order
            .get(b.model.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        oa.cmp(&ob)
            .then_with(|| b.points.partial_cmp(&a.points).unwrap_or(Ordering::Equal))
            .then_with(|| a.campaign_id.cmp(&b.campaign_id))
    });
    let mut scatter_rows = String::new();
    for c in &scatter_cells {
        let id = esc(&c.campaign_id);
        let fp = esc(c.fingerprints.first().map(String::as_str).unwrap_or("—"));
        let name = labels
            .get(&c.campaign_id)
            .map(String::as_str)
            .unwrap_or(&c.campaign_id);
        scatter_rows.push_str(&format!(
            "<tr id=\"{id}\" title=\"{id} · {fp}\">{}<td>{}</td><td class=\"num\">{:.1}</td><td class=\"num\">${:.2}</td></tr>",
            model_cell(&c.model, &colours),
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
        survival_rows.push_str(&format!("<tr>{}{cells}</tr>", model_cell(model, &colours)));
    }
    let mut h = head_html(
        "SubDollarBench — results",
        "What the sdb-sql career grid measured: points against dollars, survival by ticket, per-ticket pass share, head-to-head results and rank stability.",
        include_str!("portal_board.css"),
    );
    h.push_str(&page_header(
        "📈 SubDollarBench <span class=\"page-sub\">results</span>",
        "Budget LLMs build a SQL database, ticket by ticket, for at most $0.99 per phase. Graded against SQLite.",
        "index.html",
        "← Board",
        &chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
    ));
    h.push_str(section_nav());
    h.push_str(&format!(
        "<section id=\"measured\" class=\"methodology measured\"><h2>What it measured</h2><div class=\"wrap\"><table class=\"measured-table\"><tbody><tr><th>Rules</th><td>{}</td></tr>{fingerprint_rows}<tr><th>SQLite</th><td>{}</td></tr><tr><th>Sandbox image</th><td><code>{}</code></td></tr><tr><th>Harness / effort</th><td>{}</td></tr><tr><th>Cap</th><td>cap policy: at list price for runs from .39; each career records its own; $0.99 per phase × 4 phases = ${:.2}; 21 tickets (0-20)</td></tr><tr><th>Prices</th><td>{}</td></tr><tr><th>Grid dates</th><td>{} → {}</td></tr><tr><th>Careers</th><td>{}</td></tr><tr><th>Methodology</th><td><a href=\"https://github.com/lubod/subdollarbench/blob/master/docs/PROPOSAL-sdb-sql.md\">docs/PROPOSAL-sdb-sql.md</a></td></tr></tbody></table></div>{fingerprint_note}</section>",
        esc(rules),
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
    if let Some(note) = unranked_note(&marked) {
        h.push_str(&format!("<div class=\"mut note\">{note}</div>"));
    }
    let shape_legend = chart_legend(&models, &colours, &indices, "shape");
    let dash_legend = chart_legend(&models, &colours, &indices, "dash");
    h.push_str(&format!(
        "<section id=\"points\"><h2>Points against dollars</h2><div class=\"mut\">One dot per career; the line is the non-dominated frontier. Career cap ${CAREER_CAP_USD:.2}.</div>{shape_legend}<div class=\"chart-wrap\">{scatter}</div><div class=\"wrap table-scroll-y\"><table><thead><tr><th>Model</th><th>Career</th><th class=\"num\">Points</th><th class=\"num\">Spend</th></tr></thead><tbody>{scatter_rows}</tbody></table></div></section>"
    ));
    h.push_str(&format!(
        "<section id=\"survival\"><h2>Survival: P(highest passed ticket ≥ i)</h2><div class=\"mut\">Each line follows the career's highest passed ticket; earlier tickets may have failed. The per-ticket table below shows each one.</div>{dash_legend}<div class=\"chart-wrap\">{survival}</div><div class=\"wrap table-scroll-y\"><table><thead><tr><th>Model</th><th>0</th><th>1</th><th>2</th><th>3</th><th>4</th><th>5</th><th>6</th><th>7</th><th>8</th><th>9</th><th>10</th><th>11</th><th>12</th><th>13</th><th>14</th><th>15</th><th>16</th><th>17</th><th>18</th><th>19</th><th>20</th></tr></thead><tbody>{survival_rows}</tbody></table></div></section>"
    ));
    // R65 F6: the per-ticket pass share, shaded like the board's agreement.
    let mut ticket_rows = String::new();
    for model in &models {
        let cs = by_model.get(model.as_str()).cloned().unwrap_or_default();
        let n = cs.len();
        let mut passed = [0usize; 21];
        for c in &cs {
            for rec in unique_stage_records(&c.stage_records) {
                if rec.passed && rec.id <= 20 {
                    passed[rec.id as usize] += 1;
                }
            }
        }
        let cells = (0..=20)
            .map(|id| {
                let p = passed[id];
                let pct = if n > 0 { p * 100 / n } else { 0 };
                let cls = if p == 0 {
                    "agree-none"
                } else if p == n {
                    "agree-full"
                } else if p * 2 > n {
                    "agree-most"
                } else {
                    "agree-some"
                };
                format!(
                    "<td class=\"num {cls}\" title=\"ticket {id} · passed by {p}/{n}\">{pct}%</td>"
                )
            })
            .collect::<String>();
        ticket_rows.push_str(&format!("<tr>{}{cells}</tr>", model_cell(model, &colours)));
    }
    h.push_str(&format!(
        "<section id=\"tickets\"><h2>Per-ticket pass share</h2><div class=\"wrap table-scroll-y\"><table><thead><tr><th>Model</th><th>0</th><th>1</th><th>2</th><th>3</th><th>4</th><th>5</th><th>6</th><th>7</th><th>8</th><th>9</th><th>10</th><th>11</th><th>12</th><th>13</th><th>14</th><th>15</th><th>16</th><th>17</th><th>18</th><th>19</th><th>20</th></tr></thead><tbody>{ticket_rows}</tbody></table></div><h3>Cost and robustness</h3><div class=\"wrap table-scroll-y\"><table><thead><tr><th>Model</th><th class=\"num\">$ / ticket (min / median / max)</th><th class=\"num\">Regression-free rate</th><th class=\"num\">Spend (list / billed)</th></tr></thead><tbody>{metrics}</tbody></table></div><div class=\"mut\">The cap counts list price. Billed is the provider's own charge, recorded from round 67 on; the v2 careers ran unpinned (+11% billed over list overall). No career recorded a regression: every passed ticket stayed green, so each model's rate is 100.0%.</div></section>"
    ));
    let stability = rank_stability(cells, &groups);
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
                "<tr>{}{}<td class=\"num\">{:.0}%</td><td class=\"num\">{:.0}%</td><td class=\"num\">{:.0}%</td></tr>",
                model_cell(&models[i], &colours),
                model_cell(&models[j], &colours),
                aw * 100.0,
                bw * 100.0,
                ties * 100.0
            ));
        }
    }
    h.push_str("<section id=\"head-to-head\"><h2>Head-to-head</h2><div class=\"mut\">The share of career pairs where model A's career outscores model B's, by points.</div><div class=\"wrap table-scroll-y\"><table><thead><tr><th>Model A</th><th>Model B</th><th class=\"num\">A wins</th><th class=\"num\">B wins</th><th class=\"num\">Ties</th></tr></thead><tbody>");
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
    h.push_str("<section id=\"variance\"><h2>Variance check</h2><div class=\"mut\">The publishing bar: the spread between models must exceed the spread within one model's careers.</div><div class=\"wrap table-scroll-y\"><table><thead><tr><th>Metric</th><th class=\"num\">Median within-model spread</th><th class=\"num\">Span of model medians</th><th class=\"num\">Between / within</th></tr></thead><tbody>");
    h.push_str(&var_rows);
    h.push_str("</tbody></table></div></section>");
    h.push_str("<section id=\"ranks\"><h2>Model ranks</h2><div class=\"wrap table-scroll-y\"><table><thead><tr><th>#</th><th>Model</th><th class=\"num\">Best / median / worst points</th></tr></thead><tbody>");
    for (i, g) in current_groups.iter().enumerate() {
        let stab = stability.as_ref().map(|s| &s[i]);
        h.push_str(&format!(
            "<tr><td>{}</td>{}<td class=\"num\">{:.1} / {:.1} / {:.1}</td></tr>",
            rank_badge(g, stab),
            model_cell(&g.model, &colours),
            cells[g.best].points,
            cells[g.median].points,
            cells[g.worst].points
        ));
    }
    h.push_str("</tbody></table></div></section>");
    h.push_str(&footer_html(
        &chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
    ));
    h.push_str("</main></body></html>");
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
    let mut fingerprint_careers: HashMap<String, usize> = HashMap::new();
    for c in &ranked {
        for f in &c.fingerprints {
            *fingerprint_careers.entry(f.clone()).or_default() += 1;
        }
    }
    let meta = serde_json::json!({
        "rules_version": ranked.iter().map(|c| c.rules.as_str()).find(|r| !r.is_empty()).unwrap_or(crate::report::rules::RULES_VERSION),
        "current_rules": crate::report::rules::RULES_VERSION,
        "ranked_rules": ranked_rules,
        "fingerprint": fingerprint,
        "ranked_fingerprints": ranked_fingerprints,
        "fingerprint_careers": fingerprint_careers,
        "careers": ranked.len(),
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

    /// R67 F2: record a phase's provider bill in both files.
    fn set_billed(runs: &Path, prefix: &str, phase: u32, value: f64) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            rollup["billed_usd"] = serde_json::json!(value);
            fs::write(path, v.to_string()).unwrap();
        }
    }

    /// R70 F5: mark a phase over budget in both files.
    fn set_over_budget(runs: &Path, prefix: &str, phase: u32) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            rollup["over_budget"] = serde_json::json!(true);
            fs::write(path, v.to_string()).unwrap();
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

    /// R65 F7: true when the visible text holds a dollar amount with more
    /// than two decimals.
    fn money_over_two(text: &str) -> bool {
        let mut rest = text;
        while let Some(i) = rest.find('$') {
            let tail = &rest[i + 1..];
            let digits: String = tail
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Some(dot) = digits.find('.') {
                let decimals = digits[dot + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                if decimals > 2 {
                    return true;
                }
            }
            rest = &tail[digits.len()..];
        }
        false
    }

    /// R64 F4: the text a reader sees, with every tag and attribute removed.
    fn visible_text(html: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for ch in html.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => {
                    in_tag = false;
                    // Keep a boundary between cells: `$0.04` followed by a
                    // bare `1750.0` must not read as one long decimal.
                    out.push(' ');
                }
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// R70 F4 (R69-6): the model row's markup, from its `<tr>` to the next
    /// model row. Used to check rank and share in the same row.
    fn model_row_html<'a>(html: &'a str, model: &str) -> &'a str {
        let marker = format!("<b title=\"{model}\">");
        let start_tag = "<tr class=\"model-row\">";
        let mut from = 0;
        while let Some(rel) = html[from..].find(start_tag) {
            let start = from + rel;
            let after = start + start_tag.len();
            let end = html[after..]
                .find(start_tag)
                .map(|i| after + i)
                .unwrap_or(html.len());
            let row = &html[start..end];
            if row.contains(&marker) {
                return row;
            }
            from = end;
        }
        panic!("no model row for {model}");
    }

    /// R71 F3: read one attribute from an opening tag.
    fn attr<'a>(tag: &'a str, name: &str) -> &'a str {
        let needle = format!("{name}=\"");
        let at = tag
            .find(&needle)
            .unwrap_or_else(|| panic!("missing {name}: {tag}"));
        tag[at + needle.len()..].split('"').next().unwrap()
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
        // R71 F4 (operator): the public board shows finished careers only.
        // The running career is a banner count, never a row or a link.
        assert_eq!(
            html.matches("class=\"career-row unfinished\"").count(),
            0,
            "the unfinished row is gone: {html}"
        );
        assert!(
            !html.contains("unfinished careers")
                && !html.contains("campaign_u1_p2")
                && !html.contains("phase 2 of 4")
                && !html.contains(">unfinished<"),
            "the running career is absent from the board: {html}"
        );
        assert!(
            html.contains("Grid in progress: 6 of 7 careers finished; ranks are provisional."),
            "the banner keeps the count and names no run: {html}"
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
                html.contains(&format!("data-t=\"{id}\"")),
                "ticket {id}: {html}"
            );
        }
        let t = |id: u32| {
            let marker = format!("data-t=\"{id}\"");
            let at = pos(&html, &marker);
            let start = html[..at].rfind("<span").unwrap();
            let end = html[at..].find("</span>").unwrap() + at;
            &html[start..end]
        };
        assert!(t(0).contains("class=\"sq p\"") && t(0).contains("data-t=\"0\""));
        assert!(t(7).contains("class=\"sq p\""));
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
        // R69 F2: the points are the hero (`median / 76`), with the
        // best-worst range and career count under the bar.
        assert!(
            html.contains("class=\"hero\">8.0 / 76"),
            "the model row leads with the median points: {html}"
        );
        assert!(
            html.contains("4 - 12 · 3 careers"),
            "the model row carries its range and career count: {html}"
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
        // R66 F2: two fingerprints under one rules version are listed with
        // their counts, not warned about.
        assert!(html.contains("Graded under"), "{html}");
        assert!(html.contains("<code>fp-a</code> (1 career)"), "{html}");
        assert!(html.contains("<code>fp-b</code> (1 career)"), "{html}");
        assert!(
            html.contains("Fingerprints differ when the harness or the price list changed"),
            "{html}"
        );
        assert!(
            !html.contains("more than one recorded fingerprint"),
            "{html}"
        );

        let bar = base.join("bar.txt");
        fs::write(&bar, "RESULT: PASS\n").unwrap();
        let status = std::process::Command::new("bash")
            .arg(env!("CARGO_MANIFEST_DIR").to_string() + "/scripts/publish.sh")
            .args(["--check", out.to_str().unwrap(), bar.to_str().unwrap()])
            .status()
            .expect("run publish check");
        assert!(
            status.success(),
            "two fingerprints under one rules version must publish"
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
        let row = model_row_html(&html, "b");
        assert!(
            row.contains("#2") && row.contains("89%"),
            "the model row has no exact rank and share: {row}"
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
        // R70 F1: one footnote with the count and score.
        assert!(
            board.contains("1 $0 rehearsal (the reference server; 4 / 76 each) is not ranked"),
            "{board}"
        );
        let results = read(&out.join("results.html"));
        // R65 F7 / R70 F1: the results page shows the same one-line footnote.
        assert!(
            results.contains("1 $0 rehearsal (the reference server; 4 / 76 each) is not ranked"),
            "{results}"
        );
        assert!(results.contains("$0 rehearsal"), "{results}");
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
        // Portal polish: a smoke cell is a footnote, not a row.
        assert!(board.contains("Not ranked: a smoke cell scored"), "{board}");
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
            html.contains("data-card=\"careers\"") && html.contains("2 across 2 models"),
            "only the two ranked careers count: {html}"
        );
        assert!(
            html.contains("data-card=\"top-model\"") && html.contains("70.0 / 76 median"),
            "the $0 rehearsal's 71 must not be the top model: {html}"
        );
        assert!(
            html.contains("$0.80 spent"),
            "spend is the ranked careers' $0.80: {html}"
        );
        assert!(
            html.contains("57.6") && !html.contains("57.599999999999994"),
            "points print with one decimal: {html}"
        );
        // Portal polish (operator, 2026-11-06): the .44 cell is not on the
        // board at all, neither as unfinished nor as an older rule set.
        assert!(
            !html.contains("old-model") && !html.contains("older rule sets"),
            "the .44 cell must not show: {html}"
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
            html.contains("class=\"hero\">65.0 / 76") && html.contains("63 - 70 · 3 careers"),
            "the model row is median / 76 with its worst-best range: {html}"
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
            text.contains("career 1 · 2026-10-27"),
            "the career label is career i · date: {text}"
        );
        assert!(
            html.contains("title=\"campaign_c_p4 · ")
                && html.contains("results.html#campaign_c_p4"),
            "the campaign id lives in the tooltip and the link: {html}"
        );
        // R69 F1: the rules version moved to the footer; the header is a
        // plain sentence and the updated time.
        assert_eq!(
            text.matches("Rules ").count(),
            1,
            "the rules version shows once, in the footer: {text}"
        );
        assert!(
            text.contains(&format!("Rules {}", crate::report::rules::RULES_VERSION)),
            "the footer names the rules version: {text}"
        );
        assert_eq!(
            text.matches("Updated").count(),
            2,
            "the header and the footer each carry Updated: {text}"
        );
        assert!(
            html.contains("70 / 76"),
            "the Points cell is `70 / 76`: {html}"
        );
        assert!(
            text.contains("highest ticket 20") && text.contains("kept 21 green"),
            "the secondary line carries the highest ticket and kept-green count: {text}"
        );
        assert!(
            !html.contains(">Reached</th>")
                && !html.contains(">Reg-free</th>")
                && !html.contains("Reg-free / $")
                && !html.contains(">Updated</th>"),
            "the retired columns are gone: {html}"
        );
        assert!(
            text.contains(
                "Budget LLMs build a SQL database, ticket by ticket, for at most $0.99 per phase. Graded against SQLite."
            ),
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
        let a_row = model_row_html(&html, "A");
        assert!(
            a_row.contains("#1") && a_row.contains("56%"),
            "A's most likely rank and share are in one row: {a_row}"
        );
        let c_row = model_row_html(&html, "C");
        assert!(
            c_row.contains("#3") && c_row.contains("100%"),
            "C's rank and share are in one row: {c_row}"
        );
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
            html.contains("title=\"t5 2/3\""),
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
        let h2h = &html[pos(&html, "<section id=\"head-to-head\">")..];
        let at = pos(
            h2h,
            "title=\"openrouter/deepseek/deepseek-v4.1-flash-1017\"",
        );
        let row = &h2h[at..at + h2h[at..].find("</tr>").unwrap()];
        assert!(
            row.contains("title=\"openrouter/xiaomi/mimo-v2.6-flash\""),
            "deepseek vs mimo row: {row}"
        );
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

    /// R65 F7 (R5-R8): the results page matches the board: short names,
    /// two-decimal money, date-ordered career numbers and the rehearsal as
    /// a footnote.
    #[test]
    fn r65_f7_results_matches_board() {
        let (base, runs, out) = fixture("r65f7");
        let model = "openrouter/deepseek/deepseek-v4.1-flash-1017";
        for (prefix, date) in [
            ("d1", "2026-11-04"),
            ("d2", "2026-11-05"),
            ("d3", "2026-11-06"),
        ] {
            career(&runs, prefix, model, [17.5; 4], [0.01; 4]);
            for phase in 1..=4 {
                set_phase_times(
                    &runs,
                    prefix,
                    phase,
                    &format!("{date}T0{phase}:00:00Z"),
                    &format!("{date}T0{phase}:30:00Z"),
                );
            }
        }
        career(&runs, "reh", model, [17.75; 4], [0.0; 4]);
        set_mark(&runs, "reh", 4, "rehearsal");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let board = read(&out.join("index.html"));
        let results = read(&out.join("results.html"));
        for (page, html) in [("board", &board), ("results", &results)] {
            let text = visible_text(html);
            assert!(
                !text.contains("openrouter/"),
                "{page} shows the provider prefix: {text}"
            );
            assert!(
                !money_over_two(&text),
                "{page} holds a dollar amount with more than two decimals: {text}"
            );
        }
        let text = visible_text(&results.replace("<wbr>", ""));
        for (i, date) in ["2026-11-04", "2026-11-05", "2026-11-06"]
            .iter()
            .enumerate()
        {
            assert!(
                text.contains(&format!(
                    "deepseek/deepseek-v4.1-flash-1017 · career {} · {date}",
                    i + 1
                )),
                "career {} is dated {date}: {text}",
                i + 1
            );
        }
        assert!(
            text.contains("1 $0 rehearsal (the reference server; 71 / 76 each) is not ranked."),
            "the rehearsal is a footnote: {text}"
        );
        assert!(
            !results.contains("Rehearsals and smoke cells"),
            "the rehearsal section is gone: {results}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R65 F6 (R1, R2): both charts carry one legend swatch per model in
    /// the series colour, with the short name; the broken Dots: line is gone.
    #[test]
    fn r65_f6_chart_legends() {
        let (base, runs, out) = fixture("r65f6legends");
        for (prefix, model) in [
            ("a", "openrouter/vendor/alpha"),
            ("b", "openrouter/vendor/beta"),
            ("c", "openrouter/vendor/gamma"),
        ] {
            career(&runs, prefix, model, [1.0; 4], [0.01; 4]);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert_eq!(
            html.matches("class=\"legend chart-legend\"").count(),
            2,
            "one legend for each chart: {html}"
        );
        assert!(!html.contains("Dots:"), "the broken Dots line is gone");
        let legends: Vec<&str> = html
            .split("class=\"legend chart-legend\"")
            .skip(1)
            .collect();
        assert_eq!(legends.len(), 2, "one legend for each chart");
        for (li, legend) in legends.iter().enumerate() {
            let legend = &legend[..legend.find("</div>").unwrap()];
            let keys: Vec<&str> = legend.split("class=\"key\"").skip(1).collect();
            assert_eq!(keys.len(), 3, "one key per model: {legend}");
            for (i, key) in keys.iter().enumerate() {
                assert!(
                    key[..160].contains(&format!("data-colour=\"{}\"", palette(i))),
                    "series colour {i}: {key}"
                );
                if li == 0 {
                    assert!(
                        key[..200].contains(&format!("data-shape=\"{}\"", model_shape(i))),
                        "shape {i}: {key}"
                    );
                    assert!(key.contains("<svg class=\"key-shape\""), "{key}");
                } else {
                    assert!(
                        key[..200].contains(&format!("data-dash=\"{}\"", model_dash(i))),
                        "dash {i}: {key}"
                    );
                    assert!(key.contains("<svg class=\"key-dash\""), "{key}");
                }
                let model = key
                    .split("data-model=\"")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap();
                let name_html = model_name_html(model);
                assert!(
                    key.contains(&format!(">{name_html}</span>")),
                    "short name {model}: {key}"
                );
                assert!(
                    !name_html.contains("openrouter/"),
                    "the legend shows no provider prefix: {key}"
                );
            }
        }
        assert!(
            html.contains("<title>vendor/alpha · P(highest passed ticket ≥ i)</title>"),
            "each survival line names its model: {html}"
        );
        assert!(
            html.contains("<title>vendor/alpha: 4.0 pts · $0.04</title>"),
            "each dot names its model and career: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R65 F6 (R3, R4): survival is the highest passed ticket, and the
    /// per-ticket table shows an earlier failure.
    #[test]
    fn r65_f6_per_ticket_pass_table() {
        let (base, runs, out) = fixture("r65f6tickets");
        career(&runs, "m", "m", [1.0; 4], [0.01; 4]);
        fail_ticket(&runs, "m", 5);
        for id in 16..=20 {
            fail_ticket(&runs, "m", id);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(
            html.contains("Survival: P(highest passed ticket ≥ i)"),
            "the heading says highest passed: {html}"
        );
        assert!(
            html.contains("Per-ticket pass share"),
            "the per-ticket table exists: {html}"
        );
        assert!(
            html.contains("title=\"ticket 5 · passed by 0/1\""),
            "ticket 5 is below 100%: {html}"
        );
        assert!(
            html.contains("title=\"ticket 15 · passed by 1/1\""),
            "ticket 15 passed: {html}"
        );
        let surv = &html[pos(&html, "Survival: P(highest")..];
        let at = pos(surv, "<td title=\"m\">");
        let row = &surv[at..at + surv[at..].find("</tr>").unwrap()];
        let cells: Vec<&str> = row.split("<td class=\"num\">").skip(1).collect();
        assert_eq!(cells.len(), 21, "one cell per ticket: {row}");
        assert!(cells[15].starts_with("100%"), "survival at 15: {row}");
        assert!(cells[16].starts_with("0%"), "survival at 16: {row}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R65 F8 / R70 F4 (R69-6): the phone layout keeps rank, model,
    /// points, perf and spend; the secondary line and the row agreement
    /// give way to the expanded details, whose agreement copy stays.
    #[test]
    fn r65_f8_mobile_board() {
        let css = include_str!("portal_board.css");
        let at = css
            .find("@media (max-width: 700px)")
            .expect("the board has a phone media query");
        let media = &css[at..];
        for rule in [
            ".col-agree { display: none; }",
            ".secondary { display: none; }",
        ] {
            assert!(media.contains(rule), "the phone rule {rule:?} is missing");
        }
        for hidden in [
            ".more { display: none; }",
            "details { display: none; }",
            ".agree { display: none; }",
        ] {
            assert!(
                !media.contains(hidden),
                "the phone rule must not hide the details strip: {hidden}"
            );
        }
        for class in [
            "col-rank",
            "col-model",
            "col-points",
            "col-perf",
            "col-spend",
        ] {
            assert!(
                !media.contains(&format!(".{class} {{ display: none; }}")),
                "the phone layout must keep {class}"
            );
        }
        let (base, runs, out) = fixture("r65f8");
        career(&runs, "m", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let row = model_row_html(&html, "m");
        for cell in [
            "class=\"num col-rank\"",
            "class=\"col-model\"",
            "class=\"num col-points\"",
            "class=\"num col-perf\"",
            "class=\"num col-spend\"",
        ] {
            assert!(row.contains(cell), "the phone row keeps {cell}: {row}");
        }
        assert!(
            row.contains("class=\"mut secondary\"") && row.contains("class=\"col-agree\""),
            "the secondary line and row agreement are the parts the phone rule gives way on: {row}"
        );
        assert!(
            html.contains("<details class=\"more\">")
                && html.contains("<div class=\"agree\" id=\"agree-"),
            "the expanded details keep the one agreement strip: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R65-2: one recorded fingerprint gets neutral "Graded under" and
    /// "Page built with" rows, even when the binary moved on; only the
    /// mixed case keeps the amber warning.
    #[test]
    fn r65_r2_fingerprint_rows() {
        let (base, runs, out) = fixture("r65r2");
        career_with_stamp(&runs, "a", "m", [1.0; 4], [0.01; 4], "fp-old");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(html.contains("<th>Graded under</th>"), "{html}");
        assert!(html.contains("<th>Page built with</th>"), "{html}");
        assert!(html.contains("fp-old"), "{html}");
        assert!(
            html.contains(&crate::report::rules::fingerprint()),
            "{html}"
        );
        assert!(
            html.contains(
                "The harness changed after these careers ran; their grades are as recorded."
            ),
            "{html}"
        );
        assert!(!html.contains("recorded fingerprint differs"), "{html}");
        let section = &html[pos(&html, "What it measured")..];
        let section = &section[..section.find("</section>").unwrap()];
        assert!(
            !section.contains("class=\"b warn\""),
            "the single-fingerprint case has no warning: {section}"
        );
        let _ = fs::remove_dir_all(&base);

        let (base2, runs2, out2) = fixture("r65r2mixed");
        career_with_stamp(&runs2, "a", "m1", [1.0; 4], [0.01; 4], "fp-a");
        career_with_stamp(&runs2, "b", "m2", [2.0; 4], [0.01; 4], "fp-b");
        export(runs2.to_str().unwrap(), out2.to_str().unwrap()).unwrap();
        let html2 = read(&out2.join("results.html"));
        // R66 F2: a mixed recorded set is listed with its counts and no
        // amber warning.
        assert!(html2.contains("<code>fp-a</code> (1 career)"), "{html2}");
        assert!(html2.contains("<code>fp-b</code> (1 career)"), "{html2}");
        assert!(
            html2.contains("Fingerprints differ when the harness or the price list changed"),
            "{html2}"
        );
        assert!(
            !html2.contains("more than one recorded fingerprint"),
            "{html2}"
        );
        let section2 = &html2[pos(&html2, "What it measured")..];
        let section2 = &section2[..section2.find("</section>").unwrap()];
        assert!(
            !section2.contains("class=\"b warn\""),
            "the mixed case is neutral: {section2}"
        );
        let _ = fs::remove_dir_all(&base2);
    }

    /// R66 F2: every fingerprint of the ranked careers is listed with its
    /// career count, the page binary is separate, and each career's title
    /// carries its fingerprint.
    #[test]
    fn r66_f2_fingerprints_listed() {
        let (base, runs, out) = fixture("r66f2");
        for (prefix, stamp) in [
            ("a1", "fp-a"),
            ("a2", "fp-a"),
            ("a3", "fp-a"),
            ("b1", "fp-b"),
            ("b2", "fp-b"),
        ] {
            career_with_stamp(&runs, prefix, "m", [1.0; 4], [0.01; 4], stamp);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let results = read(&out.join("results.html"));
        assert!(results.contains("Graded under"), "{results}");
        assert!(
            results.contains("<code>fp-a</code> (3 careers)"),
            "{results}"
        );
        assert!(
            results.contains("<code>fp-b</code> (2 careers)"),
            "{results}"
        );
        assert!(results.contains("Page built with"), "{results}");
        assert!(
            results.contains(
                "Fingerprints differ when the harness or the price list changed between careers; the rules version, and so the grading, is the same."
            ),
            "{results}"
        );
        assert!(
            results.contains("title=\"campaign_a1_p4 · fp-a\""),
            "the results row carries its fingerprint: {results}"
        );
        let board = read(&out.join("index.html"));
        assert!(
            board.contains("title=\"campaign_a1_p4 · fp-a · "),
            "the board row carries its fingerprint and model: {board}"
        );
        let meta: Value = serde_json::from_str(&read(&out.join("grid.json"))).unwrap();
        assert_eq!(meta["fingerprint_careers"]["fp-a"], 3, "{meta}");
        assert_eq!(meta["fingerprint_careers"]["fp-b"], 2, "{meta}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R67 F2: the results page shows list and billed spend per model,
    /// with the cap note; a career with no recorded bill shows `—`.
    #[test]
    fn r67_f2_results_billed() {
        let (base, runs, out) = fixture("r67billed");
        career(&runs, "m", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(html.contains("$0.04 / —"), "{html}");
        assert!(html.contains("The cap counts list price."), "{html}");
        for phase in 1..=4 {
            set_billed(&runs, "m", phase, phase as f64 * 0.1);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(html.contains("$0.04 / $1.00"), "{html}");
        assert!(html.contains("The cap counts list price."), "{html}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R67 F3 (R64-3): the paid careers are numbered 1-3 by start time;
    /// a rehearsal is never numbered.
    #[test]
    fn r67_f3_rehearsal_never_numbered() {
        let (base, runs, _out) = fixture("r67f3num");
        for (prefix, date) in [
            ("p1", "2026-11-04"),
            ("p2", "2026-11-05"),
            ("p3", "2026-11-06"),
        ] {
            career(&runs, prefix, "m", [1.0; 4], [0.01; 4]);
            for phase in 1..=4 {
                set_phase_times(
                    &runs,
                    prefix,
                    phase,
                    &format!("{date}T0{phase}:00:00Z"),
                    &format!("{date}T0{phase}:30:00Z"),
                );
            }
        }
        career(&runs, "reh", "m", [1.0; 4], [0.0; 4]);
        set_mark(&runs, "reh", 4, "rehearsal");
        let cells = careers(runs.to_str().unwrap());
        let labels = career_labels(&cells);
        for (prefix, n, date) in [
            ("p1", 1, "2026-11-04"),
            ("p2", 2, "2026-11-05"),
            ("p3", 3, "2026-11-06"),
        ] {
            let id = format!("campaign_{prefix}_p4");
            let label = labels.get(&id).unwrap_or_else(|| panic!("{id} label"));
            assert!(
                label.contains(&format!("career {n} · {date}")),
                "{id}: {label}"
            );
        }
        let reh = labels.get("campaign_reh_p4").expect("rehearsal label");
        assert!(
            !reh.contains("career"),
            "the rehearsal is not numbered: {reh}"
        );
        assert!(reh.contains("rehearsal"), "{reh}");
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
        assert!(html.contains("2026-11-08"), "{html}");
        assert!(html.contains("base rates per call"), "{html}");
        let mimo = format!(
            "{}</span> — none",
            model_name_html("xiaomi/mimo-v2.6-flash")
        );
        let qwen = format!("{}</span> — none", model_name_html("qwen/qwen3.8-flash"));
        for needle in [
            "5× the prompt rate above 100k prompt tokens".to_string(),
            "2× above 272k prompt tokens".to_string(),
            "2× in weekday peak hours".to_string(),
            mimo,
            qwen,
        ] {
            assert!(html.contains(&needle), "missing {needle:?}: {html}");
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
        assert!(html.contains(">Perf</th>"), "{html}");
        assert!(html.contains("1.32×"), "{html}");
        assert!(html.contains("W1: ref 1.000"), "{html}");
        assert!(html.contains("candidate 0.760"), "{html}");
        assert!(
            html.contains("0.10× · floored · not durable"),
            "the W4 floor and its recorded note: {html}"
        );
        let at = html.rfind("<b title=\"plain-model\">").unwrap();
        let start = html[..at].rfind("<tr").unwrap();
        let row = &html[start..start + html[start..].find("</tr>").unwrap()];
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
        // Portal polish (operator, 2026-11-06): an old rule set renders
        // without error and is not on the board.
        assert!(!html.contains("old-model"), "{html}");
        assert!(!html.contains("older rule sets"), "{html}");
        let _ = fs::remove_dir_all(&base);
    }
    // R69 F1: WCAG relative luminance and contrast, for the CSS palette
    // test. The values are computed, not asserted as constants.
    fn srgb(v: u8) -> f64 {
        let c = v as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(hex: &str) -> f64 {
        let h = hex.trim().trim_start_matches('#');
        assert_eq!(h.len(), 6, "a six-digit hex colour: {hex}");
        let r = u8::from_str_radix(&h[0..2], 16).unwrap();
        let g = u8::from_str_radix(&h[2..4], 16).unwrap();
        let b = u8::from_str_radix(&h[4..6], 16).unwrap();
        0.2126 * srgb(r) + 0.7152 * srgb(g) + 0.0722 * srgb(b)
    }

    fn contrast_ratio(a: &str, b: &str) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn css_token(css: &str, name: &str) -> String {
        let needle = format!("--{name}:");
        let at = css
            .find(&needle)
            .unwrap_or_else(|| panic!("the stylesheet has {needle}"));
        let rest = &css[at + needle.len()..];
        rest[..rest.find(';').unwrap()].trim().to_string()
    }

    /// R69 F1: one model has the same colour on the board row, both chart
    /// legends and every results table.
    #[test]
    fn r69_f1_one_colour_per_model() {
        let (base, runs, out) = fixture("r69colour");
        for (prefix, model) in [
            ("a1", "vendor/alpha"),
            ("a2", "vendor/alpha"),
            ("b1", "vendor/beta"),
            ("b2", "vendor/beta"),
            ("c1", "vendor/gamma"),
        ] {
            career(&runs, prefix, model, [1.0; 4], [0.01; 4]);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let board = read(&out.join("index.html"));
        let results = read(&out.join("results.html"));
        let mut colours: HashMap<String, String> = HashMap::new();
        for row in board.split("<tr class=\"model-row\">").skip(1) {
            let row = &row[..row.find("</tr>").unwrap()];
            let model = row
                .split("<b title=\"")
                .nth(1)
                .expect("a model name")
                .split('"')
                .next()
                .unwrap()
                .to_string();
            let at = row.find("background:").expect("a swatch");
            let colour = row[at + "background:".len()..]
                .split('"')
                .next()
                .unwrap()
                .to_string();
            colours.insert(model, colour);
        }
        assert_eq!(colours.len(), 3, "one colour per model: {colours:?}");
        for (model, colour) in &colours {
            // Both chart legends carry the model in the same colour.
            let key = format!("class=\"key\" data-model=\"{model}\"");
            let mut keys = results.split(&key).skip(1);
            let first = keys
                .next()
                .unwrap_or_else(|| panic!("{model} shape legend"));
            assert!(
                first[..160].contains(&format!("data-colour=\"{colour}\"")),
                "{model} must keep {colour} in the shape legend: {first}"
            );
            let second = keys.next().unwrap_or_else(|| panic!("{model} dash legend"));
            assert!(
                second[..160].contains(&format!("data-colour=\"{colour}\"")),
                "{model} must keep {colour} in the dash legend: {second}"
            );
            // Every results table names the model beside the same colour.
            assert!(
                results.contains(&format!(
                    "<td title=\"{model}\"><span class=\"swatch\" style=\"background:{colour}\"></span>"
                )),
                "{model} must keep {colour} in the results tables: {results}"
            );
        }
        // Twelve distinct colours, so the palette cannot silently collapse.
        let mut distinct: Vec<&str> = (0..PALETTE_LEN).map(palette).collect();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), PALETTE_LEN, "twelve distinct model colours");
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F1: the board speaks plainly; the rules, cap basis and price list
    /// live on the results page. Both pages carry the footer, the meta
    /// description, the Open Graph tags and the inline favicon.
    #[test]
    fn r69_f1_header_plain() {
        let (base, runs, out) = fixture("r69plain");
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let board = read(&out.join("index.html"));
        let results = read(&out.join("results.html"));
        let board_text = visible_text(&board);
        let results_text = visible_text(&results);
        assert!(
            !board_text.contains("model_prices.json") && !board_text.contains("cap policy"),
            "the board header speaks plainly: {board_text}"
        );
        assert!(
            board_text.contains(
                "Budget LLMs build a SQL database, ticket by ticket, for at most $0.99 per phase. Graded against SQLite."
            ),
            "the board leads with the plain sentence: {board_text}"
        );
        assert!(
            results_text.contains("model_prices.json") && results_text.contains("cap policy"),
            "the results page names the price list and the cap basis: {results_text}"
        );
        for (page, html) in [("board", &board), ("results", &results)] {
            assert!(
                html.contains("<meta name=\"description\" content=\""),
                "{page} has a meta description"
            );
            assert!(
                html.contains("<meta property=\"og:title\" content=\"")
                    && html.contains("<meta property=\"og:description\" content=\""),
                "{page} has the Open Graph tags"
            );
            assert!(
                html.contains("<link rel=\"icon\" href=\"data:image/svg+xml,"),
                "{page} has the inline favicon"
            );
            assert!(
                html.contains("<footer class=\"page-foot\">")
                    && html.contains("https://github.com/lubod/subdollarbench")
                    && html.contains("docs/PROPOSAL-sdb-sql.md")
                    && html.contains(&format!("Rules {}", crate::report::rules::RULES_VERSION))
                    && html.contains("Updated "),
                "{page} has the footer"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F2: the four cards answer who wins, what costs least per point,
    /// the cheapest career and how many careers/models the board holds.
    #[test]
    fn r69_f2_summary_cards() {
        let (base, runs, out) = fixture("r69cards");
        // The 15-career fixture: five models, three careers each.
        for (prefix, points) in [("a1", 50.0), ("a2", 60.0), ("a3", 70.0)] {
            career(&runs, prefix, "vendor/a", [points / 4.0; 4], [0.01; 4]);
        }
        for (prefix, points) in [("b1", 30.0), ("b2", 20.0), ("b3", 10.0)] {
            career(&runs, prefix, "vendor/b", [points / 4.0; 4], [0.005; 4]);
        }
        for (prefix, points) in [("c1", 40.0), ("c2", 45.0), ("c3", 55.0)] {
            career(&runs, prefix, "vendor/c", [points / 4.0; 4], [0.0125; 4]);
        }
        for (prefix, points) in [("d1", 25.0), ("d2", 35.0), ("d3", 65.0)] {
            career(&runs, prefix, "vendor/d", [points / 4.0; 4], [0.025; 4]);
        }
        for (prefix, points) in [("e1", 5.0), ("e2", 15.0), ("e3", 75.0)] {
            career(&runs, prefix, "vendor/e", [points / 4.0; 4], [0.05; 4]);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let card = |name: &str| {
            let at = pos(&html, &format!("data-card=\"{name}\""));
            let end = html[at..]
                .find("</div></div>")
                .map(|i| at + i + 12)
                .unwrap();
            &html[at..end]
        };
        let top = card("top-model");
        assert!(
            top.contains("title=\"vendor/a\"") && top.contains("60.0 / 76 median"),
            "top model is A at its 60.0 median: {top}"
        );
        let ppd = card("best-ppd");
        assert!(
            ppd.contains("title=\"vendor/a\"") && ppd.contains("1500.0 pts/$"),
            "A's median is 60.0 / $0.04 = 1500.0 pts/$: {ppd}"
        );
        let cheap = card("cheapest-career");
        assert!(
            cheap.contains("title=\"vendor/b\"") && cheap.contains("$0.02"),
            "B's career is the cheapest at $0.02: {cheap}"
        );
        let careers = card("careers");
        assert!(
            careers.contains("15 across 5 models") && careers.contains("$1.23 spent"),
            "fifteen careers across five models: {careers}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F2: the 21-square agreement strip is on the model row itself,
    /// outside the expanded careers.
    #[test]
    fn r69_f2_agreement_on_row() {
        let (base, runs, out) = fixture("r69agree");
        career(&runs, "a", "m", [1.0; 4], [0.01; 4]);
        career(&runs, "b", "m", [1.0; 4], [0.01; 4]);
        fail_ticket(&runs, "b", 5);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let start_tag = "<tr class=\"model-row\">";
        let at = pos(&html, start_tag);
        let rest = &html[at..];
        let details_start = rest
            .find("<details class=\"more\">")
            .expect("the expanded careers");
        let details_end = details_start
            + rest[details_start..]
                .find("</details>")
                .expect("the expanded careers")
            + "</details>".len();
        let cell_at = rest
            .find("<td class=\"col-agree\">")
            .expect("the row agreement cell");
        assert!(
            cell_at < details_start || cell_at > details_end,
            "the row agreement cell is outside the expanded careers: {rest}"
        );
        let cell = &rest[cell_at..];
        let cell = &cell[..cell.find("</td>").unwrap()];
        assert!(
            cell.contains("<a class=\"agree-link\" href=\"#agree-")
                && cell.contains("agree 19/20")
                && cell.contains("differs: 5"),
            "the row links to the one agreement strip: {cell}"
        );
        let details = &rest[pos(rest, "<details class=\"more\">")..];
        let details = &details[..details.find("</details>").unwrap()];
        let agree = &details[pos(details, "<div class=\"agree\" id=\"agree-")..];
        let agree = &agree[..agree.find("</div></div>").unwrap() + 12];
        assert!(
            agree.matches("title=\"t").count() == 21,
            "the details carry the one 21-square strip: {agree}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F3: every nav anchor points at a section that exists, in order.
    #[test]
    fn r69_f3_section_nav() {
        let (base, runs, out) = fixture("r69nav");
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        let at = pos(&html, "<nav class=\"section-nav\"");
        let nav = &html[at..at + html[at..].find("</nav>").unwrap()];
        let labels = [
            "What it measured",
            "Points vs $",
            "Survival",
            "Per-ticket",
            "Head-to-head",
            "Variance",
            "Ranks",
        ];
        let mut cursor = 0;
        for label in labels {
            let label_at = nav[cursor..]
                .find(label)
                .unwrap_or_else(|| panic!("nav label {label}: {nav}"));
            cursor += label_at + label.len();
            let href = &nav[..cursor];
            let href_at = href.rfind("href=\"#").unwrap();
            let id = href[href_at + 7..].split('"').next().unwrap();
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "the nav target #{id} exists: {html}"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F3: both charts carry axis titles and gridlines at round values.
    #[test]
    fn r69_f3_axis_titles() {
        let (base, runs, out) = fixture("r69axes");
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert_eq!(
            html.matches("<svg class=\"chart\"").count(),
            2,
            "two charts: {html}"
        );
        for (start, titles) in [
            (
                "aria-label=\"points against dollars\"",
                ["$ per career", "points (of 76)"],
            ),
            (
                "aria-label=\"ticket survival\"",
                ["ticket", "P(highest passed ≥ i)"],
            ),
        ] {
            let at = pos(&html, start);
            let chart = &html[at..at + html[at..].find("</svg>").unwrap()];
            for title in titles {
                assert!(
                    chart.contains("class=\"axis-title\""),
                    "the chart has axis titles: {chart}"
                );
                assert!(
                    chart.contains(&format!(">{title}</text>")),
                    "the chart names {title:?}: {chart}"
                );
            }
            assert!(
                chart.matches("class=\"grid\"").count() >= 5,
                "the chart has gridlines: {chart}"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F4: the text and chart colours clear WCAG AA on the dark theme.
    #[test]
    fn r69_f4_contrast() {
        let css = include_str!("portal_board.css");
        let bg = css_token(css, "bg");
        let card = css_token(css, "card");
        for name in ["text", "mut", "pri", "ok", "warn", "bad"] {
            let colour = css_token(css, name);
            for background in [&bg, &card] {
                let ratio = contrast_ratio(&colour, background);
                assert!(
                    ratio >= 4.5,
                    "--{name} {colour} on {background} is {ratio:.2}:1, below AA 4.5"
                );
            }
        }
        for i in 0..PALETTE_LEN {
            let colour = palette(i);
            for background in [&bg, &card] {
                let ratio = contrast_ratio(colour, background);
                assert!(
                    ratio >= 3.0,
                    "chart colour {i} {colour} on {background} is {ratio:.2}:1, below 3.0"
                );
            }
        }
    }

    /// R69 F4: the 15-career board and results stay at or under 150 KB.
    #[test]
    fn r69_f4_page_size() {
        let (base, runs, out) = fixture("r69size");
        for model in 0..5 {
            for c in 0..3 {
                let prefix = format!("m{model}c{c}");
                career(
                    &runs,
                    &prefix,
                    &format!("vendor/model-{model}"),
                    [4.0 + model as f64, 5.0, 6.0, 7.0],
                    [0.01; 4],
                );
                set_tokens(&runs, &prefix, 1, 0, 10_000, 8_000, 500, 10_500);
                set_career_perf(
                    &runs,
                    &prefix,
                    1.1,
                    serde_json::json!([
                        {"id": "W1", "metric": "latency", "reference": 1.0, "candidate": 0.9, "ratio": 1.11, "correct": true},
                        {"id": "W4", "metric": "throughput", "reference": 100.0, "candidate": 90.0, "ratio": 0.9, "correct": false, "note": "not durable"}
                    ]),
                );
            }
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        for page in ["index.html", "results.html"] {
            let bytes = fs::metadata(out.join(page)).unwrap().len();
            assert!(
                bytes <= 150 * 1024,
                "{page} is {bytes} bytes, above the 150 KB limit"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R69 F4: the pages are self-contained: no external src, link href or
    /// CSS url(). Anchor links to the code and the method are allowed.
    #[test]
    fn r69_f4_self_contained() {
        let (base, runs, out) = fixture("r69self");
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let css = include_str!("portal_board.css");
        assert!(!css.contains("url("), "the stylesheet has no CSS url()");
        for page in ["index.html", "results.html"] {
            let html = read(&out.join(page));
            assert!(!html.contains("<script"), "{page} has no script");
            for raw in html.split('<').skip(1) {
                let Some(end) = raw.find('>') else { continue };
                let tag = &raw[..end];
                let name = tag
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if name == "a" {
                    continue;
                }
                for attr in ["src", "href"] {
                    let needle = format!("{attr}=");
                    if let Some(i) = tag.find(&needle) {
                        let rest = &tag[i + needle.len()..];
                        let quote = rest.chars().next().unwrap_or('"');
                        let value = rest[1..].split(quote).next().unwrap_or("");
                        assert!(
                            !value.starts_with("http://")
                                && !value.starts_with("https://")
                                && !value.starts_with("//"),
                            "{page} has an external {attr}: {value}"
                        );
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&base);
    }
    /// R72 R1: the phone column widths leave the model column at least
    /// 110 px of the 349 px board table, so a name cannot spill into the
    /// points cell.
    #[test]
    fn r72_r1_phone_columns() {
        let css = include_str!("portal_board.css");
        let media = &css[css.find("@media (max-width: 700px)").expect("phone rule")..];
        let width = |class: &str| -> f64 {
            let needle = format!(".board-table th.{class}, .board-table td.{class} {{ width: ");
            let at = media
                .find(&needle)
                .unwrap_or_else(|| panic!("missing {class} width"));
            let rest = &media[at + needle.len()..];
            rest[..rest.find("px").unwrap()].parse().unwrap()
        };
        let sum = width("col-rank") + width("col-points") + width("col-perf") + width("col-spend");
        assert_eq!(sum, 238.0, "the phone columns are 40+88+50+60");
        // The 390 px board table measures 349 px (the 390 viewport less
        // the page padding and scrollbar); the model column is the rest.
        assert!(
            349.0 - sum >= 110.0,
            "the model column keeps at least 110 px: {}",
            349.0 - sum
        );
        assert!(
            media.contains(".hero { font-size: 1.05rem; }"),
            "the points hero shrinks to fit its 88 px cell"
        );
    }

    /// R72 F1: the careers sit in a full-width row under the model row,
    /// with the toggle under the model's name, not inside td.col-model.
    #[test]
    fn r72_f1_more_row_full_width() {
        let (base, runs, out) = fixture("r72more");
        for (prefix, model) in [("a", "vendor/a"), ("b", "vendor/b")] {
            for c in 0..2 {
                career(&runs, &format!("{prefix}{c}"), model, [1.0; 4], [0.01; 4]);
            }
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let head = &html[pos(&html, "<thead>")..pos(&html, "</thead>")];
        let columns = head.matches("</th>").count();
        assert_eq!(columns, 6, "the board has six columns: {head}");
        for model in ["vendor/a", "vendor/b"] {
            let row = model_row_html(&html, model);
            let model_end = pos(row, "</tr>");
            let model_cell = &row[..model_end];
            assert!(
                !model_cell.contains("<details"),
                "{model}'s details is not inside the model cell: {model_cell}"
            );
            let more = &row[pos(row, "<tr class=\"more-row\">")..];
            assert!(
                more.contains(&format!("<td colspan=\"{columns}\">")),
                "{model}'s more row spans every column: {more}"
            );
            assert!(
                more.contains("<table class=\"career-table\">")
                    && more.contains("<summary>×2 careers ▸</summary>"),
                "{model}'s career table and toggle are in the more row: {more}"
            );
        }
        let css = include_str!("portal_board.css");
        assert!(
            css.contains(".more-row summary { padding: 4px 10px 4px 111px; }")
                && css.contains(".more-row summary { padding-left: 33px; }"),
            "the toggle sits under the model name at 1440 and 390 px"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R72 F2: the career label drops the model prefix and the matching
    /// effort badge; the full model id stays in the title.
    #[test]
    fn r72_f2_compact_career_label() {
        let (base, runs, out) = fixture("r72label");
        let model = "openrouter/vendor/model-a";
        for c in 0..2 {
            career(&runs, &format!("c{c}"), model, [1.0; 4], [0.01; 4]);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let details = &html[pos(&html, "<details class=\"more\">")..];
        let career = &details[pos(details, "<tr class=\"career-row\">")..];
        let cell = &career[pos(career, "<td class=\"col-career\">")..];
        let cell = &cell[..cell.find("</td>").unwrap()];
        let text = visible_text(cell);
        assert!(
            text.contains("career ") && text.contains("2026-10-27"),
            "the label is career N · date: {text}"
        );
        assert!(!text.contains('/'), "the label has no model prefix: {text}");
        assert!(
            !text.contains("effort high"),
            "the matching effort badge is dropped: {text}"
        );
        assert!(
            cell.contains(model),
            "the full model id is in the title: {cell}"
        );
        let css = include_str!("portal_board.css");
        assert!(
            css.contains(".career-table .col-career { white-space: nowrap; }"),
            "the compact label is one line"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R72 F3: under the phone media query the expanded careers are block
    /// cards with no fixed min-width above 390 px.
    #[test]
    fn r72_f3_phone_cards() {
        let css = include_str!("portal_board.css");
        let media = &css[css.find("@media (max-width: 700px)").expect("phone rule")..];
        for rule in [
            ".career-table { display: block;",
            ".career-table tr.career-row {",
            "display: grid;",
            "min-width: 0;",
        ] {
            assert!(
                media.contains(rule),
                "the phone cards rule {rule:?} is missing"
            );
        }
        assert!(
            !media.contains("min-width: 900px") && !media.contains("min-width: 1000px"),
            "no expansion min-width above 390 px"
        );
        assert!(
            media.contains(
                ".career-table td.col-tickets .strip { max-width: 190px; flex-wrap: wrap; }"
            ),
            "the phone ticket strip wraps in the card"
        );
        assert!(
            media.contains(".career-table .col-tokens > span[data-short] { font-size: 0; }")
                && media.contains("content: attr(data-short);"),
            "the phone token cell shows only its short value"
        );
        let css = include_str!("portal_board.css");
        assert!(
            css.contains(".more-strips { display: flex; flex-wrap: wrap;"),
            "the dot strips wrap instead of clipping"
        );
        assert!(
            css.contains(
                ".strip-metric { display: flex; align-items: center; gap: 5px; white-space: nowrap; }"
            ),
            "each dot-strip label and value stay on one line"
        );
    }

    /// R71 F4 (operator): the public pages and grid.json show finished
    /// careers only; the banner keeps the count and names no run.
    #[test]
    fn r71_f4_finished_only() {
        let (base, runs, out) = fixture("r71finished");
        career(&runs, "a", "model-a", [1.0; 4], [0.01; 4]);
        career(&runs, "b", "model-b", [2.0; 4], [0.01; 4]);
        // One running career: phase 2 of 4, the model's only career.
        write_phase(
            &runs,
            "run",
            "model-run",
            1,
            None,
            &["campaign_run_p1".into()],
            &[0, 1, 2, 3, 4, 5, 6],
            1.0,
            0.01,
            "done",
            1.0,
            0.01,
        );
        write_phase(
            &runs,
            "run",
            "model-run",
            2,
            Some("campaign_run_p1"),
            &["campaign_run_p1".into(), "campaign_run_p2".into()],
            &[7, 8, 9, 10],
            2.0,
            0.01,
            "running",
            3.0,
            0.02,
        );
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        for page in ["index.html", "results.html"] {
            let html = read(&out.join(page));
            assert!(
                !html.contains("campaign_run_p2")
                    && !html.to_lowercase().contains("unfinished careers")
                    && !html.contains("model-run"),
                "{page} shows the running career: {html}"
            );
        }
        let board = read(&out.join("index.html"));
        assert_eq!(
            board.matches("class=\"model-row\"").count(),
            2,
            "only the two finished models: {board}"
        );
        assert!(
            board.contains("Grid in progress: 2 of 3 careers finished; ranks are provisional."),
            "the banner keeps the count and names no run: {board}"
        );
        let grid_raw = read(&out.join("grid.json"));
        assert!(
            !grid_raw.contains("campaign_run_p2")
                && !grid_raw.to_lowercase().contains("unfinished"),
            "grid.json shows the running career: {grid_raw}"
        );
        let meta: Value = serde_json::from_str(&grid_raw).unwrap();
        assert_eq!(meta["careers"], 2, "{meta}");
        assert_eq!(meta["ranked"], 2, "{meta}");
        assert!(
            !meta["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m.as_str() == Some("model-run")),
            "grid.json has no running model: {meta}"
        );
        // The acceptance's literal "1 of 3" fixture: one finished career
        // and two running careers.
        let (base2, runs2, out2) = fixture("r71finished1of3");
        career(&runs2, "a", "model-a", [1.0; 4], [0.01; 4]);
        for (prefix, model) in [("r1", "model-run1"), ("r2", "model-run2")] {
            write_phase(
                &runs2,
                prefix,
                model,
                1,
                None,
                &[format!("campaign_{prefix}_p1")],
                &[0, 1, 2, 3, 4, 5, 6],
                1.0,
                0.01,
                "done",
                1.0,
                0.01,
            );
            write_phase(
                &runs2,
                prefix,
                model,
                2,
                Some(&format!("campaign_{prefix}_p1")),
                &[
                    format!("campaign_{prefix}_p1"),
                    format!("campaign_{prefix}_p2"),
                ],
                &[7, 8, 9, 10],
                2.0,
                0.01,
                "running",
                3.0,
                0.02,
            );
        }
        export(runs2.to_str().unwrap(), out2.to_str().unwrap()).unwrap();
        let board2 = read(&out2.join("index.html"));
        assert!(
            board2.contains("Grid in progress: 1 of 3 careers finished; ranks are provisional."),
            "the 1-of-3 banner: {board2}"
        );
        assert!(
            !board2.contains("campaign_r1_p2")
                && !board2.contains("campaign_r2_p2")
                && !board2.contains("model-run1")
                && !board2.contains("model-run2"),
            "both running careers are absent: {board2}"
        );
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&base2);
    }

    /// R71 F1 (R70-2): each model's agreement strip appears once, inside
    /// its details, and the phone rule does not hide that ancestor.
    #[test]
    fn r71_f1_one_agreement_strip() {
        let (base, runs, out) = fixture("r71agreeone");
        for (prefix, model) in [("a", "vendor/a"), ("b", "vendor/b"), ("c", "vendor/c")] {
            for c in 0..2 {
                career(&runs, &format!("{prefix}{c}"), model, [1.0; 4], [0.01; 4]);
            }
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let board = read(&out.join("index.html"));
        assert_eq!(
            board.matches("<div class=\"agree\"").count(),
            3,
            "one agreement strip per model: {board}"
        );
        let mut ids: Vec<&str> = board
            .split("id=\"agree-")
            .skip(1)
            .map(|seg| seg.split('"').next().unwrap())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3, "the three strip ids are unique: {ids:?}");
        for model in ["vendor/a", "vendor/b", "vendor/c"] {
            let row = model_row_html(&board, model);
            assert_eq!(
                row.matches("<div class=\"agree\"").count(),
                1,
                "{model} has one strip: {row}"
            );
            let details = &row[pos(row, "<details ")..];
            let details = &details[..details.find("</details>").unwrap()];
            assert!(
                !row.contains("<details id=\"agree-"),
                "the id is on the strip, not the details: {row}"
            );
            let agree = &details[pos(details, "<div class=\"agree\"")..];
            let agree = &agree[..agree.find("</div></div>").unwrap() + 12];
            assert_eq!(
                agree.matches("title=\"t").count(),
                21,
                "{model}'s one strip has 21 squares: {agree}"
            );
            let id = agree
                .split("id=\"agree-")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            let link = row
                .find("<a class=\"agree-link\" href=\"#agree-")
                .expect("the row link");
            let href = row[link..]
                .split("href=\"#agree-")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert_eq!(href, id, "{model}'s link targets the strip inside details");
        }
        let css = include_str!("portal_board.css");
        let media = &css[css.find("@media (max-width: 700px)").expect("phone rule")..];
        for hidden in [
            ".more { display: none; }",
            "details { display: none; }",
            ".agree { display: none; }",
        ] {
            assert!(
                !media.contains(hidden),
                "the phone rule must not hide the strip's ancestor: {hidden}"
            );
        }
        let results = read(&out.join("results.html"));
        assert_eq!(
            results.matches("<div class=\"agree\"").count(),
            0,
            "the results page has no duplicate agreement strip: {results}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R71 F3 (R70-4): ten models get ten different colours, each at least
    /// 3:1 against the card.
    #[test]
    fn r71_f3_ten_distinct() {
        let css = include_str!("portal_board.css");
        let card = css_token(css, "card");
        let colours: Vec<&str> = (0..10).map(palette).collect();
        let mut distinct = colours.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 10, "ten different colours: {colours:?}");
        for (i, colour) in colours.iter().enumerate() {
            let ratio = contrast_ratio(colour, &card);
            assert!(
                ratio >= 3.0,
                "colour {i} {colour} on {card} is {ratio:.2}:1, below 3.0"
            );
        }
    }

    /// R71 F3 (R70-4): every model's scatter marks share one shape and its
    /// survival line one dash; no same-family pair shares a shape.
    #[test]
    fn r71_f3_second_channel() {
        let (base, runs, out) = fixture("r71second");
        for model in 0..10 {
            for c in 0..2 {
                let prefix = format!("m{model}c{c}");
                career(
                    &runs,
                    &prefix,
                    &format!("vendor/model-{model}"),
                    [4.0 + model as f64, 5.0, 6.0, 7.0],
                    [0.01; 4],
                );
            }
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        let mut shapes: HashMap<String, String> = HashMap::new();
        let mut families: HashMap<String, String> = HashMap::new();
        for seg in html.split("class=\"career-dot\"").skip(1) {
            let seg = &seg[..seg.find('>').expect("the opening tag")];
            let model = attr(seg, "data-model").to_string();
            let shape = attr(seg, "data-shape").to_string();
            let family = attr(seg, "data-family").to_string();
            if let Some(previous) = shapes.get(&model) {
                assert_eq!(previous, &shape, "{model} has two shapes");
            } else {
                shapes.insert(model.clone(), shape);
                families.insert(model, family);
            }
        }
        assert_eq!(shapes.len(), 10, "ten models in the scatter: {shapes:?}");
        let mut dashes: HashMap<String, String> = HashMap::new();
        for seg in html.split("class=\"survival-line\"").skip(1) {
            let seg = &seg[..seg.find('>').expect("the opening tag")];
            let model = attr(seg, "data-model").to_string();
            let dash = attr(seg, "data-dash").to_string();
            if let Some(previous) = dashes.get(&model) {
                assert_eq!(previous, &dash, "{model} has two dashes");
            } else {
                dashes.insert(model, dash);
            }
        }
        assert_eq!(dashes.len(), 10, "ten models in survival: {dashes:?}");
        let mut distinct_shapes: Vec<&String> = shapes.values().collect();
        distinct_shapes.sort();
        distinct_shapes.dedup();
        assert_eq!(distinct_shapes.len(), 10, "ten shapes: {shapes:?}");
        let mut distinct_dashes: Vec<&String> = dashes.values().collect();
        distinct_dashes.sort();
        distinct_dashes.dedup();
        assert_eq!(distinct_dashes.len(), 10, "ten dashes: {dashes:?}");
        for (a, family) in &families {
            for (b, other) in &families {
                if a < b && family == other {
                    assert_ne!(shapes[a], shapes[b], "{a} and {b} share a family and shape");
                }
            }
        }
        // The legends carry the same shape and dash per model.
        for seg in html.split("class=\"key\" data-model=\"").skip(1) {
            let model = seg.split('"').next().unwrap();
            let head = &seg[..seg.find('>').expect("the key tag")];
            if head.contains("data-shape=") {
                assert_eq!(
                    attr(head, "data-shape"),
                    shapes.get(model).expect("a scatter model"),
                    "{model} legend shape"
                );
            } else {
                assert_eq!(
                    attr(head, "data-dash"),
                    dashes.get(model).expect("a survival model"),
                    "{model} legend dash"
                );
            }
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R71 F1 (R70-2): the board and results stay at or under 150 KB with
    /// 30 careers across 10 models, every model expanded.
    #[test]
    fn r71_f1_page_size_30_careers() {
        let (base, runs, out) = fixture("r71size30");
        let models = [
            "openrouter/anthropic/claude-haiku-5.5",
            "openrouter/deepseek/deepseek-v4.1-flash-1017",
            "openrouter/openai/gpt-6-luna",
            "openrouter/xiaomi/mimo-v2.6-flash",
            "openrouter/qwen/qwen3.8-flash",
            "openrouter/minimax/minimax-m3.1-flash",
            "openrouter/openai/gpt-6-luna-pro",
            "openrouter/xiaomi/mimo-v2.6-pro",
            "openrouter/tencent/hy4",
            "openrouter/meta/muse-spark-1.3-contributor",
        ];
        for (model, id) in models.iter().enumerate() {
            for c in 0..3 {
                let prefix = format!("m{model}c{c}");
                career(
                    &runs,
                    &prefix,
                    id,
                    [4.0 + model as f64, 5.0, 6.0, 7.0],
                    [0.01; 4],
                );
                set_tokens(&runs, &prefix, 1, 0, 10_000, 8_000, 500, 10_500);
                set_career_perf(
                    &runs,
                    &prefix,
                    1.1,
                    serde_json::json!([
                        {"id": "W1", "metric": "latency", "reference": 1.0, "candidate": 0.9, "ratio": 1.11, "correct": true},
                        {"id": "W4", "metric": "throughput", "reference": 100.0, "candidate": 90.0, "ratio": 0.9, "correct": false, "note": "not durable"}
                    ]),
                );
            }
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert_eq!(
            html.matches("<details class=\"more\">").count(),
            10,
            "ten expanded model blocks"
        );
        assert_eq!(
            html.matches("<tr class=\"career-row\">").count(),
            30,
            "thirty expanded careers"
        );
        let board = fs::metadata(out.join("index.html")).unwrap().len();
        let results = fs::metadata(out.join("results.html")).unwrap().len();
        assert!(
            board <= 150 * 1024,
            "index.html is {board} bytes with 30 careers, over 150 KB"
        );
        assert!(
            results <= 150 * 1024,
            "results.html is {results} bytes with 30 careers, over 150 KB"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R70 F4/F5: a text node that starts with "· " is a dangling
    /// separator; the stylesheet's generated `content` is not a text node.
    fn dangling_separator(html: &str) -> Option<String> {
        let mut rest = html;
        let mut in_style = false;
        loop {
            let Some(lt) = rest.find('<') else {
                if !in_style && rest.trim_start().starts_with("· ") {
                    return Some(rest.to_string());
                }
                return None;
            };
            let text = &rest[..lt];
            if !in_style && text.trim_start().starts_with("· ") {
                return Some(text.to_string());
            }
            let gt = rest[lt..].find('>')?;
            let tag = &rest[lt + 1..lt + gt];
            let name = tag
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('/')
                .to_ascii_lowercase();
            if name == "style" {
                in_style = !tag.starts_with('/');
            }
            rest = &rest[lt + gt + 1..];
        }
    }

    /// R70 F1 (R69-2): three rehearsals share one footnote with the count
    /// and score, on both pages; a smoke cell keeps its own line.
    #[test]
    fn r70_f1_rehearsals_one_line() {
        let (base, runs, out) = fixture("r70rehearsals");
        for prefix in ["r1", "r2", "r3"] {
            career(&runs, prefix, "m", [17.75; 4], [0.0; 4]); // 71
            set_mark(&runs, prefix, 4, "rehearsal");
        }
        career(&runs, "s1", "m", [10.0; 4], [0.01; 4]); // 40
        set_mark(&runs, "s1", 1, "smoke");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        for page in ["index.html", "results.html"] {
            let html = read(&out.join(page));
            assert!(
                html.contains(
                    "3 $0 rehearsals (the reference server; 71 / 76 each) are not ranked."
                ),
                "{page} must collapse the rehearsals to one line: {html}"
            );
            assert_eq!(
                html.matches("(the reference server;").count(),
                1,
                "{page} repeats the rehearsal footnote: {html}"
            );
            assert!(
                html.contains("Not ranked: a smoke cell scored 40 / 76."),
                "{page} must keep the smoke line: {html}"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R70 F2 (R69-3): model names break only after `/` or `-`, and the
    /// name CSS has no break-all or anywhere.
    #[test]
    fn r70_f2_card_name_breaks() {
        let name = "openrouter/deepseek/deepseek-v4.1-flash-1017";
        assert!(
            model_name_html(name).contains("deepseek/<wbr>deepseek-<wbr>v4.1-<wbr>flash-<wbr>1017"),
            "the model name carries a <wbr> after / and -: {}",
            model_name_html(name)
        );
        let css = include_str!("portal_board.css");
        assert!(!css.contains("break-all"), "no break-all on names");
        assert!(!css.contains("anywhere"), "no anywhere on names");
        let (base, runs, out) = fixture("r70namebreak");
        career(&runs, "c", name, [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let at = pos(&html, "data-card=\"top-model\"");
        let card = &html[at..at + html[at..].find("</div></div>").unwrap()];
        assert!(
            card.contains("<wbr>"),
            "the summary card name carries break points: {card}"
        );
        let row = model_row_html(&html, name);
        assert!(
            row.contains("deepseek/<wbr>deepseek-<wbr>v4.1-<wbr>flash-<wbr>1017"),
            "the board row name carries break points: {row}"
        );
        let details = &html[pos(&html, "<details class=\"more\">")..];
        let career = &details[pos(details, "<tr class=\"career-row\">")..];
        assert!(
            career.contains("title=\"campaign_c_p4 · ")
                && career.contains("openrouter/deepseek/deepseek-v4.1-flash-1017"),
            "the compact label keeps the full model in its title: {career}"
        );
        assert!(
            !career.contains("deepseek/<wbr>"),
            "the compact label has no split model name: {career}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R70 F2 (R69-4): on a phone the row agreement gives way to the copy
    /// inside the expanded details, which is not display: none.
    #[test]
    fn r70_f2_agreement_on_phone() {
        let css = include_str!("portal_board.css");
        let at = css
            .find("@media (max-width: 700px)")
            .expect("the phone media query");
        let media = &css[at..];
        assert!(
            media.contains(".col-agree { display: none; }"),
            "the row link is hidden on a phone"
        );
        for hidden in [
            ".more { display: none; }",
            "details { display: none; }",
            ".agree { display: none; }",
        ] {
            assert!(
                !media.contains(hidden),
                "the phone rule must not hide the details strip: {hidden}"
            );
        }
        let (base, runs, out) = fixture("r70agreephone");
        career(&runs, "a", "m", [1.0; 4], [0.01; 4]);
        career(&runs, "b", "m", [1.0; 4], [0.01; 4]);
        fail_ticket(&runs, "b", 5);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let details = pos(&html, "<details class=\"more\">");
        let details = &html[details..details + html[details..].find("</details>").unwrap()];
        let agree = pos(details, "<div class=\"agree\" id=\"agree-");
        let agree = &details[agree..];
        let agree = &agree[..agree.find("</div></div>").unwrap() + 12];
        assert!(
            agree.matches("title=\"t").count() == 21,
            "the details carry the 21-square strip: {agree}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R70 F3 (R69-5): the cards follow the board's #1 and the median
    /// pts/$ across each model's careers.
    #[test]
    fn r70_f3_cards_follow_board() {
        let (base, runs, out) = fixture("r70cards");
        // A and B tie on 60 points; A's median career has the higher perf.
        career(&runs, "a1", "vendor/a", [15.0; 4], [0.1; 4]);
        career(&runs, "a2", "vendor/a", [15.0; 4], [0.1; 4]);
        set_career_perf(&runs, "a1", 1.0, serde_json::json!([]));
        set_career_perf(&runs, "a2", 0.9, serde_json::json!([]));
        career(&runs, "b1", "vendor/b", [15.0; 4], [0.1; 4]);
        set_career_perf(&runs, "b1", 0.8, serde_json::json!([]));
        // C's median-points career is not its median pts/$ career:
        // 50/$0.02 = 2500, 40/$0.10 = 400, 38/$0.05 = 760.
        career(&runs, "c1", "vendor/c", [12.5; 4], [0.005; 4]);
        career(&runs, "c2", "vendor/c", [10.0; 4], [0.025; 4]);
        career(&runs, "c3", "vendor/c", [9.5; 4], [0.0125; 4]);
        // D ties two careers at 750 pts/$; deduplicating before the median
        // would read 825 and wrongly beat C.
        career(&runs, "d1", "vendor/d", [7.5; 4], [0.01; 4]);
        career(&runs, "d2", "vendor/d", [7.5; 4], [0.01; 4]);
        career(&runs, "d3", "vendor/d", [11.25; 4], [0.0125; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let top = &html[pos(&html, "data-card=\"top-model\"")..];
        let top = &top[..top.find("</div></div>").unwrap()];
        assert!(
            top.contains("title=\"vendor/a\"") && top.contains("60.0 / 76 median"),
            "the top card is the board's #1, not the last points tie: {top}"
        );
        let ppd = &html[pos(&html, "data-card=\"best-ppd\"")..];
        let ppd = &ppd[..ppd.find("</div></div>").unwrap()];
        assert!(
            ppd.contains("Best median points per dollar"),
            "the card names the metric: {ppd}"
        );
        assert!(
            ppd.contains("title=\"vendor/c\"") && ppd.contains("760.0 pts/$"),
            "C's median pts/$ is 760.0, not its median-points career's 400 or D's deduped 825: {ppd}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R70 F5 (R69-7): the expanded career table names its 12 columns.
    #[test]
    fn r70_f5_career_table_header() {
        let (base, runs, out) = fixture("r70tablehead");
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let details = &html[pos(&html, "<details class=\"more\">")..];
        let table = &details[pos(details, "<table class=\"career-table\">")..];
        let table = &table[..table.find("</table>").unwrap()];
        let head = &table[..table.find("</thead>").expect("a career-table head")];
        for column in [
            "Career",
            "Tickets",
            "Highest ticket",
            "Tickets kept green",
            "Points",
            "Perf",
            "Spend",
            "Pts / $",
            "Duration",
            "Turns",
            "Tokens out",
            "State",
        ] {
            assert!(head.contains(column), "the head names {column}: {head}");
        }
        assert_eq!(
            head.matches("</th>").count(),
            13,
            "thirteen columns: {head}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R70 F5 (R69-7): no text node starts with "· ".
    #[test]
    fn r70_f5_no_dangling_separator() {
        let (base, runs, out) = fixture("r70dangling");
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        set_over_budget(&runs, "c", 4);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        for page in ["index.html", "results.html"] {
            let html = read(&out.join(page));
            assert_eq!(
                dangling_separator(&html),
                None,
                "{page} has a text node that starts with a separator"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }
}
