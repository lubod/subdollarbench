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
    pub cost: f64,
    pub over_budget: bool,
    pub cap_reached: bool,
    pub over_wall: bool,
    pub updated: String,
    pub started: String,
    pub finished_at: String,
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
        });
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
    let finished = if leaf.phase.is_some() {
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
    let unique = unique_stage_records(&stage_records);
    let reached = unique.iter().filter(|r| r.passed).map(|r| r.id).max();
    let regression_free = unique
        .iter()
        .filter(|r| r.passed && r.regressed.is_empty())
        .count() as u32;
    let parsed = leaf.parsed.as_ref();
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
            per_dollar: (cost > 0.0).then(|| points / cost),
            cost,
        });
    let started = leaf.rollup["started_at"].as_str().unwrap_or("").to_string();
    let finished_at = leaf.rollup["finished_at"]
        .as_str()
        .or_else(|| leaf.rollup["started_at"].as_str())
        .unwrap_or("")
        .to_string();
    CareerCell {
        campaign_id: leaf.name.clone(),
        cells: cells.iter().map(|c| c.name.clone()).collect(),
        model: leaf.rollup["model"].as_str().unwrap_or("?").to_string(),
        harness: leaf.rollup["harness"].as_str().unwrap_or("?").to_string(),
        effort: leaf.rollup["effort"].as_str().unwrap_or("high").to_string(),
        phase: leaf.phase,
        phase_total,
        finished,
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
        cost,
        over_budget: leaf.rollup["over_budget"].as_bool().unwrap_or(false),
        cap_reached: leaf.rollup["cap_reached"].as_bool().unwrap_or(false),
        over_wall: leaf.rollup["over_wall"].as_bool().unwrap_or(false),
        updated: leaf.updated.clone().unwrap_or_else(|| finished_at.clone()),
        started,
        finished_at,
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
        g.median = g.careers[(g.careers.len() - 1) / 2];
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

fn rpd_of(c: &CareerCell) -> String {
    c.reg_free_per_dollar()
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

fn rules_of(c: &CareerCell) -> String {
    let base = if c.rules == crate::report::rules::RULES_VERSION {
        format!("<span class=\"mut\">{}</span>", esc(&c.rules))
    } else if c.rules.is_empty() {
        "<span class=\"b warn\">rules —</span>".to_string()
    } else {
        format!(
            "<span class=\"b warn\" title=\"graded under a different rule set\">stale {}</span>",
            esc(&c.rules)
        )
    };
    let mut text = format!(
        "{base} · SQLite {}",
        esc(crate::report::rules::recorded_sqlite_version(
            c.sqlite_version.as_deref()
        ))
    );
    if crate::report::rules::recorded_cap_basis(&c.rules, c.cap_price_basis.as_deref()).is_some() {
        text.push_str(" · cap at list price");
    }
    text
}

fn career_row(c: &CareerCell, phase_label: Option<&str>, extra_class: &str) -> String {
    let label = phase_label
        .map(|p| format!("<div class=\"mut\">{p}</div>"))
        .unwrap_or_default();
    let class = if extra_class.is_empty() {
        "career-row".to_string()
    } else {
        format!("career-row {extra_class}")
    };
    format!(
        "<tr class=\"{class}\"><td class=\"num\"></td><td>{label}<b>{}</b><div class=\"mut\">{} · effort {}{}</div>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}/{} · {}</td><td class=\"num\">${:.4}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}{}</td><td class=\"mut\">{}</td></tr>",
        esc(&c.campaign_id),
        esc(&c.model),
        esc(&c.effort),
        flags_of(c),
        strip_html(&c.ladder),
        reached_of(c),
        c.regression_free,
        c.points,
        c.max_points as u32,
        rules_of(c),
        c.cost,
        ppd_of(c),
        rpd_of(c),
        state_of(c),
        "",
        esc(c.updated.get(..16).unwrap_or(&c.updated)),
    )
}

fn model_row(cells: &[CareerCell], g: &ModelGroup, html: &mut String) {
    let med = &cells[g.median];
    let best = &cells[g.best];
    let worst = &cells[g.worst];
    let mut more = String::new();
    if g.careers.len() > 1 {
        let mut inner = String::new();
        for i in &g.careers {
            inner.push_str(&career_row(&cells[*i], None, ""));
        }
        more = format!(
            "<details class=\"more\"><summary>×{} careers ▸</summary><table class=\"career-table\"><tbody>{inner}</tbody></table></details>",
            g.careers.len()
        );
    }
    html.push_str(&format!(
        "<tr class=\"model-row\"><td class=\"num\">{}</td><td><b>{}</b>{}<div class=\"mut\">{} · effort {} · best {:.1} · median {:.1} · worst {:.1}</div>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}/{} · {}</td><td class=\"num\">${:.4}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}{}</td><td class=\"mut\">{}</td></tr>",
        g.rank_label(),
        esc(&g.model),
        more,
        esc(&g.harness),
        esc(&g.effort),
        best.points,
        med.points,
        worst.points,
        strip_html(&med.ladder),
        reached_of(med),
        med.regression_free,
        med.points,
        med.max_points as u32,
        rules_of(med),
        med.cost,
        ppd_of(med),
        rpd_of(med),
        state_of(med),
        "",
        esc(med.updated.get(..16).unwrap_or(&med.updated)),
    ));
}

fn render_board(cells: &[CareerCell]) -> String {
    let groups = model_groups(cells);
    let unfinished: Vec<&CareerCell> = cells.iter().filter(|c| !c.finished).collect();
    let finished_count = cells.iter().filter(|c| c.finished).count();
    let best_points = cells.iter().map(|c| c.points).fold(0.0f64, f64::max);
    let spend: f64 = cells.iter().map(|c| c.cost).sum();
    let mut rows = String::new();
    let mut old_divider = false;
    for g in &groups {
        if !g.current && !old_divider {
            rows.push_str(
                "<tr class=\"divider\"><td colspan=\"10\">older rule sets (incomparable)</td></tr>",
            );
            old_divider = true;
        }
        model_row(cells, g, &mut rows);
    }
    if !unfinished.is_empty() {
        rows.push_str(
            "<tr class=\"divider\"><td colspan=\"10\">unfinished careers (not ranked)</td></tr>",
        );
        for c in unfinished {
            let label = match (c.phase, c.phase_total) {
                (Some(p), t) if t > 0 => format!("phase {p} of {t}"),
                (Some(p), _) => format!("phase {p}"),
                _ => "in progress".to_string(),
            };
            rows.push_str(&career_row(c, Some(&label), "unfinished"));
        }
    }
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let mut h = String::new();
    h.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>SubDollarBench — sdb-sql campaign careers</title><style>");
    h.push_str(include_str!("portal_board.css"));
    h.push_str("</style></head><body><main>");
    h.push_str("<header><div><h1>💰 SubDollarBench — sdb-sql campaign careers</h1><div class=\"sub\">One ticket ladder, one task: a differential SQL database checked statement by statement against SQLite. A model ranks by its median career; careers rank by difficulty-weighted points, then points per dollar.</div></div><div class=\"right\"><a href=\"results.html\">📈 Results</a>");
    h.push_str(&format!(
        "<div class=\"mut\">Updated {}</div></div></header>",
        esc(&now)
    ));
    h.push_str(&format!(
        "<div class=\"sub\">Current binary: SQLite {} · cap policy: at list price (model_prices.json) for runs from .39; each career records its own.</div>",
        esc(crate::report::rules::sqlite_version())
    ));
    h.push_str(&format!(
        "<div class=\"cards\"><div class=\"card\"><div class=\"k\">Careers</div><div class=\"v\">{finished_count}</div></div><div class=\"card\"><div class=\"k\">Models</div><div class=\"v\">{}</div></div><div class=\"card\"><div class=\"k\">Best points</div><div class=\"v\">{best_points:.1}</div></div><div class=\"card\"><div class=\"k\">Spend</div><div class=\"v\">${spend:.4}</div></div></div>",
        groups.iter().filter(|g| g.current).count()
    ));
    h.push_str("<div class=\"wrap\"><table><thead><tr><th>#</th><th>Model / career</th><th>Reached</th><th>Reg-free</th><th>Points</th><th>Spend</th><th>Pts / $</th><th>Reg-free / $</th><th>State</th><th>Updated</th></tr></thead><tbody>");
    if rows.is_empty() {
        h.push_str("<tr><td colspan=\"10\" class=\"mut\">No sdb-sql careers yet (run one with <code>scripts/sql-cell.sh &lt;model&gt;</code>).</td></tr>");
    } else {
        h.push_str(&rows);
    }
    h.push_str("</tbody></table>");
    h.push_str(legend_html());
    h.push_str("</div>");
    h.push_str("<section class=\"methodology\"><h2>How this board is ranked</h2><ul><li><b>Careers</b>: one row per leaf phase cell of a chain. A model's row is its <b>median career</b> by the one shared board order; the best and worst careers are named next to it and the full set sits in <b>×N careers ▸</b>.</li><li><b>Points</b>: difficulty-weighted score; <b>Pts / $</b> divides by effective spend. Model rows use the median career, not a new formula.</li><li><b>Ties</b>: adjacent models are a rank group (<code>2=</code>) when the worse model's best career is at least as good as the better model's worst, by the shared board order. Ties never block publishing.</li><li>Unfinished careers (fewer than four phases, or a phase still running) are listed under the divider and are not ranked. Older rule sets are incomparable and sit below the board.</li><li>Hidden, poisoned and harness-fault cells are never published. The ladder squares merge each phase cell's own tickets; the legend sits under the table.</li></ul></section>");
    h.push_str("</main></body></html>");
    h
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
        scatter_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"num\">{:.1}</td><td class=\"num\">${:.4}</td></tr>",
            esc(&c.model),
            esc(&c.campaign_id),
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
        "<section class=\"methodology\"><h2>What it measured</h2><div class=\"wrap\"><table><tbody><tr><th>Rules</th><td>{}</td></tr><tr><th>Fingerprint (recorded)</th><td><code>{}</code>{}</td></tr><tr><th>SQLite</th><td>{}</td></tr><tr><th>Sandbox image</th><td><code>{}</code></td></tr><tr><th>Harness / effort</th><td>{} / {}</td></tr><tr><th>Cap</th><td>$0.99 per phase × 4 phases = ${:.2}</td></tr><tr><th>Grid dates</th><td>{} → {}</td></tr><tr><th>Careers</th><td>{}</td></tr><tr><th>Methodology</th><td><a href=\"https://github.com/lubod/subdollarbench/blob/master/docs/PROPOSAL-sdb-sql.md\">docs/PROPOSAL-sdb-sql.md</a></td></tr></tbody></table></div></section>",
        esc(rules),
        esc(&recorded_text),
        fingerprint_note,
        esc(crate::report::rules::sqlite_version()),
        esc(image),
        esc(&harness),
        esc(&effort),
        CAREER_CAP_USD,
        esc(first),
        esc(last),
        caps
    ));
    h.push_str(&format!(
        "<section><h2>Points against dollars</h2><div class=\"mut\">One dot per career; the line is the non-dominated frontier. Career cap ${CAREER_CAP_USD:.2}.</div>"
    ));
    h.push_str(&format!("<div class=\"wrap\">{scatter}</div>"));
    h.push_str(&format!("<div class=\"wrap\"><table><thead><tr><th>Model</th><th>Career</th><th>Points</th><th>Spend</th></tr></thead><tbody>{scatter_rows}</tbody></table></div></section>"));
    h.push_str("<section><h2>Survival: P(ticket ≥ i)</h2><div class=\"wrap\">");
    h.push_str(&survival);
    h.push_str(&format!("</div><div class=\"wrap\"><table><thead><tr><th>Model</th><th>0</th><th>1</th><th>2</th><th>3</th><th>4</th><th>5</th><th>6</th><th>7</th><th>8</th><th>9</th><th>10</th><th>11</th><th>12</th><th>13</th><th>14</th><th>15</th><th>16</th><th>17</th><th>18</th><th>19</th><th>20</th></tr></thead><tbody>{survival_rows}</tbody></table></div></section>"));
    h.push_str(&format!("<section><h2>Cost and robustness</h2><div class=\"wrap\"><table><thead><tr><th>Model</th><th>$ / ticket (min / median / max)</th><th>Regression-free rate</th></tr></thead><tbody>{metrics}</tbody></table></div></section>"));
    h.push_str("<section><h2>Model ranks</h2><div class=\"wrap\"><table><thead><tr><th>#</th><th>Model</th><th>Best / median / worst points</th></tr></thead><tbody>");
    for g in model_groups(cells).iter().filter(|g| g.current) {
        h.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"num\">{:.1} / {:.1} / {:.1}</td></tr>",
            g.rank_label(),
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
    let out_dir = Path::new(out);
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create portal dir {}", out_dir.display()))?;
    fs::write(out_dir.join("index.html"), render_board(&cells))?;
    fs::write(out_dir.join("results.html"), render_results(&cells))?;
    let ranked: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules)
        .collect();
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
                "{} vs {}: ranked ({} worst {:.1} < {} best {:.1})\n",
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
        assert!(
            html.contains("best 12.0 · median 8.0 · worst 4.0"),
            "{html}"
        );
        assert!(html.contains("median 8.0"), "{html}");
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
        assert!(
            html.contains("<td class=\"num\">2=</td>"),
            "the tied row has no 2= rank cell: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
