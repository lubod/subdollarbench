use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::report::archive::RunManifest;
use crate::report::leaderboard::{
    format_duration, is_rankable_status, wilson_interval_95, LeaderboardManager,
};

pub const PORTAL_HIDDEN_FILE: &str = ".portal_hidden";
/// Infra-poison marker: trials died on 429/404/delisting, not model verdicts.
/// Poisoned rows are excluded from EVERY board (internal + public) but kept for audit.
pub const POISONED_FILE: &str = ".poisoned";

fn clean_id(run_id: &str) -> Option<String> {
    let name = Path::new(run_id).file_name()?.to_str()?;
    if name.is_empty() || name == "." {
        return None;
    }
    Some(name.to_string())
}

pub fn is_hidden(runs_dir: &Path, run_id: &str) -> bool {
    match clean_id(run_id) {
        Some(clean) => runs_dir.join(clean).join(PORTAL_HIDDEN_FILE).exists(),
        None => true,
    }
}

/// True when a run was killed by infrastructure (rate-limit, delisting),
/// never a statement about the model. Poisoned rows feed no aggregate.
pub fn is_poisoned(runs_dir: &Path, run_id: &str) -> bool {
    match clean_id(run_id) {
        Some(clean) => runs_dir.join(clean).join(POISONED_FILE).exists(),
        None => false,
    }
}

/// True when a run's status is a *non-result* rather than a model verdict.
///
/// `infra_error` is a harness fault and `agent_*` means the agent died or was
/// killed. Neither is a statement about the model's code, so neither may be
/// rendered as a failure - not by the status pill, and not by the pass-rate
/// pill either, since the stages of a degraded run were never meaningfully
/// measured.
pub fn is_non_result(status: &str) -> bool {
    !matches!(status, "completed" | "failed_tests" | "disqualified_cheat")
}

/// Pill class for a run's archived status.
///
/// Only a real model verdict may render red. Rendering a harness fault as
/// `fail` published an infrastructure outage on the portal as though the model
/// had failed - the exact confusion the `infra_error` status exists to
/// prevent, surviving in the display layer.
pub fn status_pill_class(status: &str) -> &'static str {
    match status {
        "completed" => "pass",
        "failed_tests" | "disqualified_cheat" => "fail",
        _ => "info",
    }
}

pub fn set_hidden(runs_dir: &Path, run_id: &str, hidden: bool) -> anyhow::Result<bool> {
    let clean = clean_id(run_id).ok_or_else(|| anyhow::anyhow!("invalid run id"))?;
    let dir = runs_dir.join(&clean);
    if !dir.join("manifest.json").exists() {
        anyhow::bail!("run not found");
    }
    let flag = dir.join(PORTAL_HIDDEN_FILE);
    if hidden {
        std::fs::write(&flag, "hidden\n")?;
    } else {
        let _ = std::fs::remove_file(&flag);
    }
    Ok(hidden)
}

pub fn set_poisoned(runs_dir: &Path, run_id: &str, poisoned: bool) -> anyhow::Result<bool> {
    let clean = clean_id(run_id).ok_or_else(|| anyhow::anyhow!("invalid run id"))?;
    let dir = runs_dir.join(&clean);
    if !dir.join("manifest.json").exists() {
        anyhow::bail!("run not found");
    }
    let flag = dir.join(POISONED_FILE);
    if poisoned {
        std::fs::write(&flag, "poisoned\n")?;
    } else {
        let _ = std::fs::remove_file(&flag);
    }
    Ok(poisoned)
}

/// Round-14 N1: poison or hide a whole campaign cell. The flag lives in the
/// campaign folder (`runs/<campaign_id>/`), where `export_sql_board` and the
/// console's `get_campaigns` look; the per-run setters above need a run
/// `manifest.json`, which a campaign folder never has. The file carries the
/// reason, so a quarantined cell always says why it is out.
pub fn set_campaign_flag(
    runs_dir: &Path,
    campaign_id: &str,
    flag_file: &str,
    on: bool,
    reason: Option<&str>,
) -> anyhow::Result<bool> {
    let clean = clean_id(campaign_id).ok_or_else(|| anyhow::anyhow!("invalid campaign id"))?;
    if !clean.starts_with("campaign_") {
        anyhow::bail!("not a campaign id");
    }
    if flag_file != POISONED_FILE && flag_file != PORTAL_HIDDEN_FILE {
        anyhow::bail!("unknown flag");
    }
    let dir = runs_dir.join(&clean);
    if !dir.join("campaign.json").exists() && !dir.join("status.json").exists() {
        anyhow::bail!("campaign not found");
    }
    let flag = dir.join(flag_file);
    if on {
        let what = if flag_file == POISONED_FILE {
            "poisoned"
        } else {
            "hidden"
        };
        let reason = reason
            .map(|r| r.trim().chars().take(500).collect::<String>())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "(no reason given)".to_string());
        std::fs::write(
            &flag,
            format!(
                "{what} {}\nreason: {reason}\n",
                Utc::now().format("%Y-%m-%d %H:%M UTC")
            ),
        )?;
    } else {
        let _ = std::fs::remove_file(&flag);
    }
    Ok(on)
}

pub struct PortalSummary {
    pub run_count: usize,
    pub out_dir: PathBuf,
}

/// One aggregated row of the public model rollup: a model across all tasks.
#[derive(Debug, Clone)]
pub struct ModelRollupRow {
    pub model: String,
    pub harness: String,
    /// Comparability class: rules regime stamp (`rules_of`) and evaluation
    /// protocol. Statistics never pool across classes (Q4).
    pub regime: String,
    pub protocol: String,
    pub n_tasks: usize,
    pub tasks: Vec<String>,
    pub n_parents: usize,
    pub n_trials: usize,
    pub n_pass: usize,
    pub avg_pass_rate: f64,
    pub p1: f64,
    pub p1_lo: f64,
    pub p1_hi: f64,
    pub total_cost_usd: f64,
    pub avg_cost_usd: f64,
    pub total_api_cost_usd: f64,
    pub avg_api_cost_usd: f64,
    pub stability: Option<String>,
}

/// Static publish-hygiene note rendered into both portal indexes (full +
/// rollup). Short, factual, no per-run data.
const PORTAL_METHODOLOGY_HTML: &str = r#"<section class="methodology" style="margin-top:2rem;padding:1rem 1.2rem;border:1px solid var(--line);border-radius:8px;"><h2>Methodology &amp; limitations</h2><ul style="font-size:0.82rem;color:var(--mut);line-height:1.5;margin:0.4rem 0 0 1.1rem;"><li><strong>Ranking:</strong> runs rank by the Wilson 95% lower bound on the pass fraction (P@1), then the P@1 point estimate, then score — an n=3 1.00 never outranks an n=9 0.89 on thin evidence. The rollup sorts by average pass rate, ties broken by Wilson lower bound, then cost.</li><li><strong>Stability flags:</strong> any pass-rate spread across trials marks <em>unstable</em>; throughput CV &gt;25% marks <em>unstable</em> (&gt;50% flags a suspect measurement); CV 10–25% marks <em>watch</em>; otherwise <em>stable</em>. Throughput-free tasks (pdf/wal/proxy/pubsub/binproto) are judged on pass spread alone; single-trial runs default to <em>stable</em> (a 9x100% row can still read <em>unstable</em> on throughput CV alone).</li><li><strong>Non-results excluded at trial level:</strong> harness/agent faults (<code>infra_error</code>, <code>agent_failed</code>, <code>agent_killed</code>) are neutral &mdash; their stages were never meaningfully measured, so they never enter pass aggregates. Parents rank on measured trials with fault counts badged; a cell with no measurement stays visible but unranked. Repair attempts are recorded per trial and merged (<code>repair_attempted</code>); <code>repaired</code> reports a converted repair only.</li><li><strong>Cheats excluded at trial level:</strong> trials stamped <code>disqualified_cheat</code> never enter pass aggregates (a cheat is not a measurement); parents rank on the remaining valid trials with a DQ count badge. Cheat trials stay visible in run pages with their stamps and consoles. A parent whose every trial cheated stays fully excluded. False positives cured by repair re-enter.</li><li><strong>Harness:</strong> trials record <code>harness</code> (<code>omp</code> | <code>opencode</code>) + version; ranks never mix harnesses — compare within one harness, like effort.</li><li><strong>Costs:</strong> total cost is the effective cost (API spend plus compute for run duration and tokens). Rows on free/contrib keys record $0.00 API and rank on the compute floor only &mdash; do not compare $0 rows against paid rows on cost.</li><li><strong>Comparability:</strong> runs are only comparable within one <code>rules_version</code> fingerprint (<code>legacy-pre-cheat</code> for 2026-09-17 and earlier, <code>legacy-cheat</code> from 2026-09-18); cross-fingerprint ranks mix rule sets.</li><li><strong>DNS removed 2026-10-05:</strong> retired task; five archived rows (09-27&ndash;09-28) preserved offline, no live verifier. Only <strong>redis/http</strong> carry a live reference denominator and a Throughput Score; pdf/wal/proxy/pubsub/binproto report rates unscored (--).</li><li><strong>Budget and keys:</strong> <code>budget_usd</code> caps live agent spend only and never gates cost accounting; the API key travels as env/flag into the runner and is never persisted in manifests, results, or the database.</li><li><strong>Publish freeze:</strong> only runs graded at or after the 2026-09-26 anti-cheat fix are published (timestamp &ge; 2026-09-26T15:34:10Z); earlier rows were graded under a stricter rule that has since changed. Fingerprint allowlists alone are insufficient because graded content changed without a version bump.</li><li><strong>Integrity limits:</strong> egress/static anti-cheat blocklists are self-admitted; a DNS candidate forwarding to the reference is indistinguishable from a real resolver by design; binproto STREAM, proxy X-Forwarded-For, and pubsub 1,000-subscriber requirements are published as specified but verified at reduced scale; the LLM judge may review its own model family rows; the API key travels in the agent container env.</li></ul></section>"#;

/// Rollup page appends the coverage-floor note to the shared methodology.
const ROLLUP_FLOOR_NOTE_HTML: &str = r#"<p style="font-size:0.82rem;color:var(--mut);">Rollup coverage floor: a model appears only with at least 6 of 7 active tasks and at least 18 total trials (avg 3/task); thinner coverage is excluded and logged at export.</p>"#;

/// Publish floor for the public rollup: one row per model only with >= 6 of
/// 7 tasks AND >= 18 total trials (avg 3/task). Thin-coverage rows (e.g. a
/// single-task probe) would otherwise top average-pass sorting on noise.
pub const ROLLUP_MIN_TASKS: usize = 6;
pub const ROLLUP_MIN_TRIALS: usize = 18;

/// Publish freeze: only rows at or after the last grading-content change are
/// published. Rationale: `fingerprint()` hashes graded inputs
/// (tasks/*/prompt.md + src/verifier/*.rs incl. compliance.rs), but the
/// 2026-09-24.2 line has THREE hashes (91939c3e/e3d2bf2b/b371d2d4) because
/// graded content changed without a RULES_VERSION bump (notably the
/// 2026-09-26 Go-mime apache2 whitelist in compliance.rs, commit 2bf3a60 —
/// an anti-cheat grading change that cleared 21 DQ trials). A fingerprint
/// allowlist alone would still publish rows graded under the old rule, so the
/// gate is an as-of timestamp cutoff instead: rows with timestamp BEFORE this
/// instant are excluded from both portal exports with a stderr count.
/// Update this when grading changes (it marks the new publish baseline).
/// Timestamps here are RFC3339; row timestamps are naive local ISO
/// (YYYY-MM-DDTHH:MM:SS); string compare works within one format, so the
/// freeze is stored naive to match row format.
pub const PUBLISH_FREEZE_TS: &str = "2026-09-26T15:34:10";

/// True when a naive ISO timestamp is new enough to publish.
pub fn publishable(timestamp: &str) -> bool {
    timestamp >= PUBLISH_FREEZE_TS
}

const PORTAL_CSS: &str = r#":root{color-scheme:dark;--bg:#0b0e14;
  --panel: #121724;
  --panel-card: #0d1117;
  --line: #232c3d;
  --border: #232c3d;
  --txt: #e8edf4;
  --mut: #8b94a3;
  --acc: #58a5ff;
  --primary: #58a5ff;
  --grn: #3fb950;
  --success: #3fb950;
  --red: #f85149;
  --danger: #f85149;
  --warning: #d29922;
  --gold: #f1e05a;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: radial-gradient(1200px 400px at 50% -10%, #16233d 0%, var(--bg) 60%);
  color: var(--txt);
  font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
  line-height: 1.55;
}
main {
  max-width: 1240px;
  margin: 0 auto;
  padding: 2.2rem 1.25rem 4rem;
}
h1 {
  font-size: 2rem;
  margin: 0;
  letter-spacing: -.02em;
  background: linear-gradient(90deg, #fff, #9ecbff);
  -webkit-background-clip: text;
  background-clip: text;
  color: transparent;
}
.sub { color: var(--mut); margin: .3rem 0 1.2rem; font-size: 0.88rem; }
a { color: var(--acc); text-decoration: none; }
a:hover { text-decoration: underline; }

.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
  gap: 10px;
  margin: 1rem 0 1.5rem;
}
.card {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 10px;
  padding: 12px 14px;
}
.card.wide { grid-column: 1 / -1; }
.card .k {
  font-size: .7rem;
  text-transform: uppercase;
  letter-spacing: .06em;
  color: var(--mut);
}
.card .v {
  font-size: 1.2rem;
  font-weight: 700;
  margin-top: 2px;
}
.card small { font-size: .75rem; font-weight: 400; color: var(--mut); }

/* Controls Bar */
.controls-bar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  flex-wrap: wrap;
  gap: 12px;
  background: var(--panel-card);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 8px 12px;
  margin-bottom: 14px;
}
.pills-group {
  display: inline-flex;
  border: 1px solid var(--line);
  border-radius: 6px;
  overflow: hidden;
  background: #090d13;
  flex-wrap: wrap;
}
.btn-toggle {
  background: #161b22;
  border: none;
  border-right: 1px solid var(--line);
  color: var(--mut);
  padding: 6px 13px;
  font-size: 0.76rem;
  cursor: pointer;
  transition: all 0.15s ease;
  font-weight: 500;
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.pills-group .btn-toggle:last-child { border-right: none; }
.btn-toggle:hover { color: #fff; background: #1c2333; }
.btn-toggle.active {
  background: var(--acc);
  color: #000;
  font-weight: 700;
}
.search-input {
  padding: 6px 12px;
  font-size: 0.78rem;
  background: #090d13;
  border: 1px solid var(--line);
  border-radius: 5px;
  color: #fff;
  width: 220px;
}
.search-input:focus { outline: none; border-color: var(--acc); }

/* Badges */
.badge {
  display: inline-block;
  padding: 2px 7px;
  border-radius: 4px;
  font-size: 0.72rem;
  font-weight: 600;
  white-space: nowrap;
}
.badge-pass { background: rgba(63, 185, 80, 0.18); color: var(--grn); border: 1px solid rgba(63, 185, 80, 0.4); }
.badge-fail { background: rgba(248, 81, 73, 0.16); color: var(--red); border: 1px solid rgba(248, 81, 73, 0.4); }
.badge-protocol { background: rgba(188, 140, 255, 0.14); color: #bc8cff; border: 1px solid rgba(188, 140, 255, 0.4); }
.badge-info { background: rgba(88, 166, 255, 0.16); color: var(--acc); border: 1px solid rgba(88, 166, 255, 0.4); }
.badge-effort { background: rgba(210, 153, 34, 0.18); color: var(--warning); border: 1px solid rgba(210, 153, 34, 0.4); }
.badge-count { background: rgba(88, 166, 255, 0.2); color: var(--acc); border-radius: 999px; font-size: 0.65rem; padding: 1px 6px; }

/* Table Wrap */
.wrap {
  overflow-x: auto;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 12px;
  box-shadow: 0 12px 40px rgba(0,0,0,.45);
  margin-bottom: 24px;
}
table {
  border-collapse: collapse;
  width: 100%;
  font-size: .83rem;
}
thead th {
  position: sticky;
  top: 0;
  background: #161d2c;
  cursor: pointer;
  user-select: none;
  white-space: nowrap;
  font-size: .73rem;
  text-transform: uppercase;
  letter-spacing: .06em;
  color: var(--mut);
  padding: 10px 12px;
  text-align: left;
  border-bottom: 1px solid var(--line);
}
thead th:hover { color: #fff; }
tbody td {
  padding: 10px 12px;
  border-bottom: 1px solid var(--line);
  vertical-align: middle;
}
tbody tr:last-child td { border-bottom: none; }
table.roll tbody tr { cursor: default; }
table.roll tbody tr.mrow:hover { background: rgba(88,165,255,.05); }
.bignum { font-size: 1.7rem; font-weight: 800; text-align: center; }
.bignum .pct { font-size: .95rem; color: var(--mut); font-weight: 600; }
.mname { font-weight: 700; font-size: 1.02rem; }
.mut { color: var(--mut); font-size: .75rem; }
.rank { display: inline-flex; align-items: center; justify-content: center; width: 1.7rem; height: 1.7rem; border-radius: 50%; background: #212b3d; color: var(--mut); font-weight: 700; }
.rank-1 { background: rgba(241,224,90,.18); color: var(--gold); }
.rank-2 { background: rgba(232,237,244,.14); color: #e8edf4; }
.rank-3 { background: rgba(242,114,68,.16); color: #f27244; }
.stab { display: inline-flex; align-items: center; gap: .4rem; font-weight: 600; text-transform: capitalize; }
.stab i { width: .6rem; height: .6rem; border-radius: 50%; display: inline-block; }
.stab-ok { background: var(--grn); } .stab-watch { background: var(--warning); } .stab-bad { background: var(--danger); } .stab-na { background: #484f58; }
.cibar { display: block; position: relative; height: .45rem; background: #212b3d; border-radius: 99px; min-width: 7rem; margin-bottom: .3rem; }
.cifill { position: absolute; top: 0; bottom: 0; background: rgba(88,165,255,.35); border-radius: 99px; }
.cipoint { position: absolute; top: -2px; width: 2px; height: calc(.45rem + 4px); background: var(--acc); }
.tdots { letter-spacing: .1em; }
.tdot { width: .55rem; height: .55rem; border-radius: 50%; display: inline-block; background: #212b3d; margin-right: .28rem; }
.tdot.on { background: var(--grn); }
details.method { margin-top: 1.4rem; border: 1px solid var(--line); border-radius: 8px; padding: .7rem 1rem; }
details.method summary { cursor: pointer; font-weight: 700; }
tbody tr {
  cursor: pointer;
  transition: background .12s;
}
tbody tr:hover { background: #182032; }
tbody tr:nth-child(even) { background: rgba(255,255,255,.015); }
tbody tr.leaderboard-row {
  cursor: pointer;
  transition: all .15s ease;
}
tbody tr.leaderboard-row:hover {
  filter: brightness(1.1);
  box-shadow: inset 0 0 16px rgba(255,255,255,0.04);
}
.model-pill {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 2px 8px;
  border-radius: 6px;
  font-family: inherit;
  transition: transform 0.15s ease, box-shadow 0.15s ease;
  max-width: fit-content;
}
.model-pill:hover {
  transform: translateY(-1px);
  box-shadow: 0 2px 8px rgba(0,0,0,0.3);
}
.model-icon {
  font-size: 0.95rem;
  line-height: 1;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}
code {
  font-size: .78rem;
  word-break: break-all;
  background: #0b0f18;
  border: 1px solid var(--line);
  padding: 2px 7px;
  border-radius: 6px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.pill {
  display: inline-block;
  padding: 2px 10px;
  border-radius: 999px;
  font-weight: 650;
  font-size: .75rem;
  white-space: nowrap;
}
.pill.pass { background: rgba(63, 185, 80, 0.14); color: var(--grn); border: 1px solid rgba(63, 185, 80, 0.4); }
.pill.fail { background: rgba(248, 81, 73, 0.12); color: var(--red); border: 1px solid rgba(248, 81, 73, 0.4); }
.pill.info { background: rgba(88, 165, 255, 0.12); color: var(--acc); border: 1px solid rgba(88, 165, 255, 0.4); }
.score {
  color: #79c0ff;
  font-weight: 750;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
pre {
  background: #0b0f18;
  padding: 1rem;
  overflow-x: auto;
  font-size: .78rem;
  border: 1px solid var(--line);
  border-radius: 10px;
}
details { margin: 1.2rem 0; }
summary { cursor: pointer; color: var(--acc); }
.hero { margin: 0 0 1.4rem; }
.hero h1 { margin: .2rem 0 .6rem; word-break: break-word; }
.pills { display: flex; gap: 8px; flex-wrap: wrap; margin: .4rem 0; }
.stages { display: flex; flex-direction: column; gap: 8px; }
.stage { display: flex; gap: 10px; align-items: flex-start; background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 10px 12px; }
.bar { height: 8px; background: #0b0f18; border: 1px solid var(--line); border-radius: 99px; margin-top: 8px; overflow: hidden; }
.fill { height: 100%; background: linear-gradient(90deg, #1f6feb, #3fb950); }
h2 { margin-top: 2rem; font-size: 1.3rem; letter-spacing: -0.01em; }
.review { display: grid; gap: 12px; }
@media(min-width:800px) { .review { grid-template-columns: 1fr 1fr; } }
.review ul { margin: .4rem 0; padding-left: 1.2rem; }
.review li { margin: .35rem 0; font-size: .86rem; }
.review p { grid-column: 1 / -1; }
.muted { color: var(--mut); }
footer { margin-top: 2.5rem; color: var(--mut); font-size: .8rem; }

/* Banner */
.banner {
  background: var(--panel-card);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 14px 18px;
  margin-bottom: 14px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.banner-kpi {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: 10px;
  margin-top: 6px;
}
.kpi-card {
  background: #090d13;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

"#;

const PORTAL_JS: &str = concat!(
    include_str!("../web/shared.js"),
    r#"
(function() {



  const RUNS_DATA = window.__SUBDOLLAR_RUNS__ || [];
  
  const TASK_SPECS = {
    http: {
      name: 'HTTP/1.1 Web Server',
      icon: '🌐',
      desc: 'RFC 7230/7231 web server handling GET /, 404s, /echo/{str}, header reflection, POST/GET /files/{name} persistence, keep-alive socket reuse, and path traversal security guards.',
      stagesCount: 5,
      refInfo: 'nginx:alpine reference container on port 8081 (live-measured per batch; no fixed baseline)',
      benchTool: 'wrk -t2 -c20 -d3s http://127.0.0.1:8080/'
    },
    redis: {
      name: 'In-Memory Concurrent Redis Clone',
      icon: '⚡',
      desc: 'RESP2 protocol TCP server handling pipelined streams, simple strings, bulk strings, integers, errors, PING, ECHO, SET (with PX expiry), GET, DEL, EXISTS, and INCR non-integer guards.',
      stagesCount: 4,
      refInfo: 'redis:alpine reference container on port 6380 (live-measured per batch; no fixed baseline)',
      benchTool: 'redis-benchmark -p 6379 -n 100000 -c 10 -t get -q (scored, read path; write path reported separately, fsync-bound)'
    },
    wal: {
      name: 'TCP Write-Ahead Log',
      icon: '📓',
      desc: 'TCP WAL broker handling PUB append with fsync durability, SUB-from-offset replay, topic isolation, SIGKILL crash recovery, and PUB burst throughput.',
      stagesCount: 5,
      refInfo: 'No external reference: verifier-timed msgs/s, reported but unscored',
      benchTool: '2000-msg PUB burst + SIGKILL recovery probe'
    },
    proxy: {
      name: 'HTTP Reverse Proxy',
      icon: '🔀',
      desc: 'HTTP reverse proxy on port 8091 over three upstream servers: round-robin routing, upstream keep-alive pooling, X-Forwarded-For injection, and failover with zero dropped requests.',
      stagesCount: 5,
      refInfo: 'No valid throughput denominator (direct-nginx comparison is structurally unreachable): rate reported, not ranked',
      benchTool: '200-req routing + failover + concurrent GET burst'
    },
    pubsub: {
      name: 'WebSocket Pub/Sub Broker',
      icon: '📢',
      desc: 'WebSocket fan-out broker on port 8092: RFC 6455 handshake, multi-subscriber broadcast, channel isolation, ping/pong, and 200-subscriber soak.',
      stagesCount: 4,
      refInfo: 'No external reference: verifier soak in deliveries/s, reported but unscored',
      benchTool: 'Soak: 200 subscribers x 20 msgs'
    },
    pdf: {
      name: 'PDF-to-Text Microservice',
      icon: '📄',
      desc: 'RFC-compliant HTTP microservice on port 8090 converting binary PDF documents to plaintext using a custom PDF parser and DEFLATE engine (no external PDF libraries).',
      stagesCount: 5,
      refInfo: 'Parity oracle only (own slow extractor, expected slower than candidates): pages/s reported, not ranked',
      benchTool: 'Synthetic multi-page PDF extraction burst (20 requests, 60 pages)'
    }
  };

  // Server-side ranking is authoritative (Rust finalize_results):
  // P@1-lo, then P@1, then throughput/efficiency score, then cost.
  // The old client-side formula customizer (weights/presets/sliders) was
  // deleted: it re-derived ranking behind UI slider state and zeroed every
  // sub-100% cell behind strictPass (defect #1).
  let currentTab = 'models';
  let searchQuery = '';
  let passOnly = false;
  // Data-derived pricing tier (shared.js: tierOf). 'all' by default so
  // nothing hides until the reader asks for a split.
  let boardTier = 'all';
  // Default sort mirrors the server rank: P@1 lower bound, descending.
  let sortCol = 'p1lo';
  let sortDir = -1;
  // Models-tab ordering. Default: pooled P@1 lower bound, descending.
  let modelsSortCol = 'p1lo';
  let modelsSortDir = -1;



  function formatDuration(sec) {
    if (sec === null || sec === undefined) return '-';
    if (sec < 60) return `${Math.round(sec)}s`;
    const m = Math.floor(sec / 60);
    const s = Math.round(sec % 60);
    return `${m}m ${s}s`;
  }

  // Units are not interchangeable: pubsub reports fan-out *deliveries*
  // (200 subs x 20 msgs = 4000 copies from 20 requests), wal reports
  // fsync-bound messages, binproto reports binary ops. Any of those labelled
  // "req/s" invites comparing them against a real request rate, which the
  // numbers do not support. Mirrors throughput_unit() in leaderboard.rs.


  function formatTimestamp(ts) {
    if (!ts) return '';
    try {
      const d = new Date(ts);
      if (isNaN(d.getTime())) return ts;
      return d.toISOString().slice(0, 16).replace('T', ' ') + ' UTC';
    } catch (e) {
      return ts;
    }
  }

  // NOTE: there is deliberately no fallback-baseline function. A hardcoded
  // constant (the old getBaseline: 71000/19000/4600/...) is unmeasured on
  // this testbed and makes refless tasks look comparable to anchored ones.
  // Ratios render only from a row's measured reference_throughput_req_sec;
  // tasks without one show the raw rate, unscored (--).
  function regimeOf(r) {
    if (r.ref_image) {
      const hex = (r.ref_image.split('@')[1] || r.ref_image).replace(/^sha256:/, '');
      return r.ref_drift ? hex.slice(0, 12) + '+drift' : hex.slice(0, 12);
    }
    return 'legacy';
  }

  // Server-side score (Rust-computed, proportional since 2026-09-24.1):
  // throughput-adjusted where a live reference anchors it, efficiency
  // otherwise. The old client-side re-derivation (strictPass cliff +
  // slider weights) is deleted; this reads the shipped fields.
  function serverScore(r) {
    if (r.throughput_score !== undefined && r.throughput_score !== null) return r.throughput_score;
    return r.efficiency_score || 0;
  }

  function computeDefaultRanks(runs, tab) {
    let list = runs.slice();
    if (tab && tab !== 'all') {
      list = list.filter(r => (r.task || '').toLowerCase() === tab.toLowerCase());
    }
    const rankLo = r => (r.pass_at_1_lo !== undefined && r.pass_at_1_lo !== null) ? r.pass_at_1_lo : ((r.pass_at_1 !== undefined && r.pass_at_1 !== null) ? r.pass_at_1 : (r.pass_rate || 0));
    const rankP1 = r => (r.pass_at_1 !== undefined && r.pass_at_1 !== null) ? r.pass_at_1 : (r.pass_rate || 0);
    // Mirrors Rust finalize_results: P@1-lo, then P@1, then score, then cost.
    list.sort((a, b) => (rankLo(b) - rankLo(a)) || (rankP1(b) - rankP1(a)) || (serverScore(b) - serverScore(a)));
    const map = {};
    list.forEach((r, idx) => {
      map[r.id] = idx + 1;
    });
    return map;
  }

  function getSortedFilteredRuns(runs, tab, query, onlyPassed) {
    let filtered = runs.slice();
    if (tab && tab !== 'all') {
      filtered = filtered.filter(r => (r.task || '').toLowerCase() === tab.toLowerCase());
    }
    if (query) {
      const q = query.toLowerCase();
      filtered = filtered.filter(r => 
        (r.model || '').toLowerCase().includes(q) ||
        (r.language || '').toLowerCase().includes(q) ||
        (r.task || '').toLowerCase().includes(q)
      );
    }
    if (onlyPassed) {
      filtered = filtered.filter(r => ((r.avg_pass_rate !== undefined && r.avg_pass_rate !== null) ? r.avg_pass_rate : (r.pass_rate || 0)) >= 100);
    }
    // Free rows rank on the compute floor only (zero API spend); paid rows
    // pay per token. Cross-tier cost comparisons rank nothing, so the split
    // is explicit and data-derived.
    if (boardTier !== 'all') {
      filtered = filtered.filter(r => tierOf(r) === boardTier);
    }

    // 'score' sorts on the server-side score (proportional, no cliff);
    // 'p1lo' mirrors the Rust rank key (P@1 lower bound, descending).
    const srvScore = (r) => {
      if (r.throughput_score !== undefined && r.throughput_score !== null) return r.throughput_score;
      return r.efficiency_score || 0;
    };
    const srvLo = (r) => (r.pass_at_1_lo !== undefined && r.pass_at_1_lo !== null) ? r.pass_at_1_lo : 0;
    filtered.sort((a, b) => {
      let vA, vB;
      if (sortCol === 'score') {
        vA = srvScore(a);
        vB = srvScore(b);
        if (vA === vB) {
          const loA = srvLo(a);
          const loB = srvLo(b);
          return sortDir === 1 ? (loA - loB) : (loB - loA);
        }
      } else if (sortCol === 'p1lo') {
        vA = srvLo(a);
        vB = srvLo(b);
      } else if (sortCol === 'model') {
        vA = (a.model || '').toLowerCase();
        vB = (b.model || '').toLowerCase();
        return sortDir === 1 ? vA.localeCompare(vB) : vB.localeCompare(vA);
      } else if (sortCol === 'pass') {
        vA = (a.avg_pass_rate !== undefined && a.avg_pass_rate !== null) ? a.avg_pass_rate : (a.pass_rate || 0);
        vB = (b.avg_pass_rate !== undefined && b.avg_pass_rate !== null) ? b.avg_pass_rate : (b.pass_rate || 0);
      } else if (sortCol === 'dur') {
        vA = a.duration_seconds || 0;
        vB = b.duration_seconds || 0;
      } else if (sortCol === 'turns') {
        vA = a.turns || 0;
        vB = b.turns || 0;
      } else if (sortCol === 'tp') {
        vA = a.throughput_req_sec || 0;
        vB = b.throughput_req_sec || 0;
      } else if (sortCol === 'api_cost') {
        vA = a.total_cost_usd || 0;
        vB = b.total_cost_usd || 0;
      } else if (sortCol === 'cost') {
        vA = (a.effective_cost_usd !== undefined && a.effective_cost_usd !== null) ? a.effective_cost_usd : a.total_cost_usd;
        vB = (b.effective_cost_usd !== undefined && b.effective_cost_usd !== null) ? b.effective_cost_usd : b.total_cost_usd;
      } else {
        vA = 0; vB = 0;
      }
      return sortDir === 1 ? (vA - vB) : (vB - vA);
    });

    return filtered;
  }

  function renderRowsHtml(runs, showTaskCol, defaultRanks) {
    if (!runs || runs.length === 0) {
      const colSpan = showTaskCol ? 10 : 9;
      return `<tr><td colspan="${colSpan}" style="text-align: center; color: var(--mut); padding: 28px;">No matching runs recorded.</td></tr>`;
    }

    return runs.map((r, idx) => {
      const curRank = idx + 1;
      let rankBadge = `<span style="font-weight: 700; color: var(--mut);">#${curRank}</span>`;
      if (curRank === 1) rankBadge = `<span style="font-weight: 800; font-size: 1rem; color: var(--gold);">🥇 #1</span>`;
      else if (curRank === 2) rankBadge = `<span style="font-weight: 800; font-size: 1rem; color: #c0c0c0;">🥈 #2</span>`;
      else if (curRank === 3) rankBadge = `<span style="font-weight: 800; font-size: 1rem; color: #cd7f32;">🥉 #3</span>`;

      const taskBadge = showTaskCol
        ? `<span class="badge" style="background: rgba(88, 166, 255, 0.15); color: var(--acc); font-weight: 600;">${(r.task || 'http').toUpperCase()}</span>`
        : '';
      const hasSubruns = Array.isArray(r.subruns) && r.subruns.length > 0;
      const subrunsBadge = hasSubruns
        ? `<span class="badge" style="background: rgba(163, 113, 247, 0.15); color: #d2a8ff; font-weight: 600;" title="Fair Expected Score: Median TP & Passing Mean Cost scaled by Pass@1">${r.subruns.length} trials (Fair)</span>`
        : '';
      const passBadge = (r.pass_rate >= 100)
        ? `<span class="badge badge-pass">✔ 100% (${r.passed_stages}/${r.total_stages})</span>`
        : `<span class="badge badge-fail">✕ ${Math.round(r.pass_rate)}% (${r.passed_stages}/${r.total_stages})</span>`;
      const passAtK = (hasSubruns && r.pass_at_1 !== undefined && r.pass_at_k !== undefined)
        ? `<div style="font-size: 0.72rem; color: var(--grn); font-weight: 600;">Pass@1: ${Math.round(r.pass_at_1)}% · Pass@${r.total_trials || r.subruns.length}: ${Math.round(r.pass_at_k)}%</div>`
        : '';
      const effBadge = `<span class="badge badge-effort">effort: ${escapeHtml(r.effort || 'auto')}</span>`;

      let subrunsHtml = '';
      if (hasSubruns) {
        const totalSpent = r.subruns.reduce((acc, sr) => acc + (sr.effective_cost_usd || sr.total_cost_usd || 0), 0);
        const passingCount = r.subruns.filter(sr => (sr.pass_rate || 0) >= 100).length;
        subrunsHtml = `
          <details style="margin-top: 5px; font-size: 0.72rem; color: var(--mut);">
            <summary style="cursor: pointer; color: var(--acc); font-weight: 600;">View ${r.subruns.length} trials breakdown ▾ (${passingCount}/${r.subruns.length} passed · Total spend: $${totalSpent.toFixed(4)})</summary>
            <div style="margin-top: 4px; padding: 6px 8px; background: rgba(0,0,0,0.25); border-radius: 4px; border: 1px solid rgba(255,255,255,0.06); display: flex; flex-direction: column; gap: 4px;">
              ${(() => {
                const tpIqr = iqr(r.subruns.map(sr => sr.throughput_req_sec));
                const tokIqr = iqr(r.subruns.map(sr => sr.total_tokens || ((sr.prompt_tokens || 0) + (sr.cached_tokens || 0) + (sr.completion_tokens || 0))));
                return `<div style="font-size: 0.72rem; color: var(--mut);" title="Inter-quartile range across trials (display only; ranking uses Wilson P@1 bounds)">spread — throughput: ${fmtIqr(tpIqr, fmtTp)} · tokens: ${fmtIqr(tokIqr)}</div>`;
              })()}
              ${r.subruns.map((sr, sIdx) => {
                const sPass = (sr.pass_rate || 0) >= 100;
                const isDq = (sr.status || '') === 'disqualified_cheat';
      const statusTag = isDq
        ? `<span style="color:var(--warning);font-weight:600;" title="Excluded from parent score, retained in record">⊘ DQ · ${Math.round(sr.pass_rate || 0)}%</span>`
        : (sPass ? '<span style="color:var(--grn);font-weight:600;">✔ 100%</span>' : `<span style="color:var(--red);font-weight:600;">✕ ${Math.round(sr.pass_rate || 0)}%</span>`);
                return `
                <div style="display: flex; justify-content: space-between; align-items: center; gap: 8px; border-bottom: 1px dashed rgba(255,255,255,0.05); padding-bottom: 2px;">
                  <span><strong>Trial ${sIdx + 1}:</strong> ${statusTag} · ${formatThroughput(sr.throughput_req_sec, sr.task)} · $${(sr.effective_cost_usd || sr.total_cost_usd).toFixed(4)} · ${formatDuration(sr.duration_seconds)} · ${sr.turns || '-'}t</span>
                  <a href="runs/${escapeHtml(sr.id)}/" style="color: var(--primary); text-decoration: underline;">Inspect</a>
                </div>
              `;}).join('')}
            </div>
          </details>
        `;
      }
      const durStr = formatDuration(r.duration_seconds);
      const turnsStr = (r.turns !== undefined && r.turns !== null) ? `${r.turns}` : '-';
      const tpStr = formatThroughput(r.throughput_req_sec, r.task);
      const refRatio = (r.reference_throughput_req_sec && r.throughput_req_sec)
        ? `<div style="font-size: 0.72rem; color: var(--grn); font-weight: 600;">${Math.round((r.throughput_req_sec / r.reference_throughput_req_sec) * 100)}% of Ref · <span title="Measurement regime: same task + same regime = comparable">${regimeOf(r)}</span></div>`
        : '';
      // Frontier fraction: share of the best anchored same-task throughput
      // in this comparison set. Secondary to the external reference above.
      const frontierLine = (r.frontier_fraction !== undefined && r.frontier_fraction !== null)
        ? `<div style="font-size: 0.72rem; color: var(--acc); font-weight: 600;" title="Frontier: this run vs the fastest anchored same-task run in this result set (secondary to the external reference)">🏁 ${Math.round(r.frontier_fraction * 100)}% of best</div>`
        : '';
      
      const effCost = (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null) ? r.effective_cost_usd : r.total_cost_usd;
      const apiCost = r.total_cost_usd;
      const apiDiff = (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null && Math.abs(r.effective_cost_usd - r.total_cost_usd) > 0.0001)
        ? `<span style="font-size: 0.7rem; color: var(--mut);">(API: $${r.total_cost_usd.toFixed(4)})</span>`
        : '';
      const totalTokens = r.total_tokens || ((r.prompt_tokens || 0) + (r.cached_tokens || 0) + (r.completion_tokens || 0));
      const tokStr = `${(totalTokens / 1000).toFixed(1)}k tokens`;

      // Server-side score cell: throughput-adjusted where anchored,
      // efficiency otherwise. No client re-derivation, no cliff.
      const srvScore = (r.throughput_score !== undefined && r.throughput_score !== null)
        ? r.throughput_score : (r.efficiency_score || 0);
      const scoreMain = `${srvScore.toFixed(1)} <small style="font-size: 0.72rem; color: var(--mut);">pts/¢</small>`;

      let scoreSub = '';
      {
        const bonusBase = (r.throughput_score !== undefined && Math.abs(r.throughput_score - r.efficiency_score) > 0.1 && r.pass_rate >= 100)
          ? `<div style="font-size: 0.72rem; color: var(--mut);">base: ${r.efficiency_score.toFixed(1)}</div>`
          : '';
        scoreSub = bonusBase;
      }

      const modelMeta = getModelMeta(r.model);
      return `<tr data-href="runs/${escapeHtml(r.id)}/" class="leaderboard-row" style="border-left: 4px solid ${modelMeta.color}; background: linear-gradient(90deg, ${modelMeta.bg} 0%, rgba(18, 23, 36, 0.6) 12%, transparent 100%);">
        <td style="text-align: center;">${rankBadge}</td>
        <td>
          <div style="display: flex; flex-direction: column; gap: 4px;">
            <div style="display: flex; align-items: center; gap: 6px; flex-wrap: wrap;">
              <div class="model-pill" style="background: ${modelMeta.bg}; border: 1px solid ${modelMeta.border};">
                <span class="model-icon" title="${modelMeta.family}">${modelMeta.icon}</span>
                <a href="runs/${escapeHtml(r.id)}/" title="${escapeHtml(r.model)}" style="font-weight: 600; color: ${modelMeta.color}; text-decoration: none;">${escapeHtml(shortModelName(r.model))}</a>
              </div>
              ${taskBadge}
              ${subrunsBadge}
              <span class="badge" style="background: rgba(63, 185, 80, 0.15); color: var(--grn);">${escapeHtml(r.language || 'Unknown')}</span>
            </div>
            <div style="font-size: 0.72rem; color: var(--mut);">${formatTimestamp(r.timestamp)}</div>
            ${subrunsHtml}
          </div>
        </td>
        <td>
          <div style="display: flex; flex-direction: column; gap: 3px;">
            <div>${passBadge}</div>
            ${passAtK}
            <div>${effBadge}</div>
          </div>
        </td>
        <td><span style="font-weight: 500;">${durStr}</span></td>
        <td style="text-align: center;"><span class="badge" style="background: rgba(88, 166, 255, 0.15); color: var(--acc); font-weight: 600;">${turnsStr}</span></td>
        <td>
          <span style="font-weight: 600;">${tpStr}</span>
          ${refRatio}
          ${frontierLine}
        </td>
        <td>
          <div style="display: flex; flex-direction: column; gap: 2px;">
            <div>
              <div><span style="font-size: 0.7rem; color: var(--mut);">API</span> <span style="font-weight: 600;">$${apiCost.toFixed(4)}</span></div>
              <div><span style="font-size: 0.7rem; color: var(--mut);">Effective</span> <span style="font-weight: 600; color: var(--gold);">$${effCost.toFixed(4)}</span></div>
              ${apiDiff}
            </div>
            <span style="font-size: 0.72rem; color: var(--mut);">${tokStr}</span>
          </div>
        </td>
        <td class="score">
          <div style="font-size: 0.95rem; font-weight: 700;">${scoreMain}</div>
          ${scoreSub}
        </td>
        <td><a href="runs/${escapeHtml(r.id)}/" class="badge badge-info" style="text-decoration: none; padding: 4px 8px;">Inspect &rarr;</a></td>
      </tr>`;
    }).join('\n');
  }

  function renderTaskBanner(taskKey, runs) {
    const spec = TASK_SPECS[taskKey];
    if (!spec) return '';
    const taskRuns = runs.filter(r => (r.task || '').toLowerCase() === taskKey.toLowerCase());
    // Featured run: full-pass filter, ordered by P@1-lo (server rank key).
    const passRuns = taskRuns.filter(r => (r.pass_rate || 0) >= 100);
    const srvLo = (r) => (r.pass_at_1_lo !== undefined && r.pass_at_1_lo !== null) ? r.pass_at_1_lo : 0;
    const srvScore = (r) => (r.throughput_score !== undefined && r.throughput_score !== null) ? r.throughput_score : (r.efficiency_score || 0);
    const topRun = passRuns.slice().sort((a, b) => (srvLo(b) - srvLo(a)) || (srvScore(b) - srvScore(a)))[0];

    const peakTpRun = passRuns.slice().sort((a, b) => (b.throughput_req_sec || 0) - (a.throughput_req_sec || 0))[0];
    const lowestCostRun = passRuns.slice().sort((a, b) => {
      const cA = a.effective_cost_usd !== undefined && a.effective_cost_usd !== null ? a.effective_cost_usd : a.total_cost_usd;
      const cB = b.effective_cost_usd !== undefined && b.effective_cost_usd !== null ? b.effective_cost_usd : b.total_cost_usd;
      return cA - cB;
    })[0];

    const topModel = topRun ? shortModelName(topRun.model) : 'None yet';
    const topScoreVal = topRun ? ((topRun.throughput_score !== undefined && topRun.throughput_score !== null) ? topRun.throughput_score : (topRun.efficiency_score || 0)) : null;
    const topScore = topRun ? (topScoreVal.toFixed(1) + ' pts/¢') : '-';
    const peakTpStr = peakTpRun ? formatThroughput(peakTpRun.throughput_req_sec, taskKey) : 'N/A';
    const peakRatioStr = (peakTpRun && peakTpRun.reference_throughput_req_sec && peakTpRun.throughput_req_sec)
      ? `${Math.round((peakTpRun.throughput_req_sec / peakTpRun.reference_throughput_req_sec) * 100)}% of reference · regime ${regimeOf(peakTpRun)}`
      : '';
    const lowestCostStr = lowestCostRun
      ? `$${((lowestCostRun.effective_cost_usd !== undefined && lowestCostRun.effective_cost_usd !== null) ? lowestCostRun.effective_cost_usd : lowestCostRun.total_cost_usd).toFixed(4)}`
      : 'N/A';

    return `
      <div class="banner">
        <div style="display: flex; justify-content: space-between; align-items: flex-start; flex-wrap: wrap; gap: 10px;">
          <div>
            <div style="display: flex; align-items: center; gap: 8px;">
              <span style="font-size: 1.3rem;">${spec.icon}</span>
              <span style="font-size: 1.05rem; font-weight: 700; color: #fff;">${spec.name}</span>
              <span class="badge badge-info">${spec.stagesCount} Stages</span>
            </div>
            <div style="font-size: 0.78rem; color: var(--mut); margin-top: 4px; max-width: 820px;">
              ${spec.desc}
            </div>
          </div>
          <div style="text-align: right; font-size: 0.74rem; color: var(--mut);">
            <div><strong>Baseline:</strong> ${spec.refInfo}</div>
            <div style="margin-top: 2px;"><strong>Benchmark:</strong> <code>${spec.benchTool}</code></div>
          </div>
        </div>
        <div class="banner-kpi">
          <div class="kpi-card">
            <span class="k">🏆 Current Leader</span>
            <span style="font-weight: 700; color: var(--gold); font-size: 0.88rem; word-break: break-all;">${escapeHtml(topModel)}</span>
            <span style="font-size: 0.72rem; color: var(--acc); font-weight: 600;">${topScore}</span>
          </div>
          <div class="kpi-card">
            <span class="k">⚡ Peak Throughput</span>
            <span style="font-weight: 700; color: #fff; font-size: 0.95rem;">${peakTpStr}</span>
            <span style="font-size: 0.72rem; color: var(--grn); font-weight: 600;">${peakRatioStr}</span>
          </div>
          <div class="kpi-card">
            <span class="k">💰 Lowest Cost (100% Pass)</span>
            <span style="font-weight: 700; color: var(--gold); font-size: 0.95rem;">${lowestCostStr}</span>
            <span style="font-size: 0.72rem; color: var(--mut);">${passRuns.length} of ${taskRuns.length} passed</span>
          </div>
        </div>
      </div>
    `;
  }

  function renderTableContainer(runs, showTaskCol, title, subtitle, defaultRanks) {
    const headerHtml = title ? `
      <div style="display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 8px;">
        <span style="font-weight: 700; color: #fff; font-size: 0.95rem;">${title}</span>
        ${subtitle ? `<span style="font-size: 0.75rem; color: var(--mut);">${subtitle}</span>` : ''}
      </div>` : '';

    return `
      ${headerHtml}
      <div class="wrap">
        <table>
          <thead>
            <tr>
              <th style="width: 50px; text-align: center;" onclick="window.__subdollar_sort('score')">#</th>
              <th onclick="window.__subdollar_sort('model')">Model & Stack</th>
              <th onclick="window.__subdollar_sort('pass')">Conformance & Effort</th>
              <th onclick="window.__subdollar_sort('dur')">Duration</th>
              <th style="text-align: center;" onclick="window.__subdollar_sort('turns')">Turns</th>
              <th onclick="window.__subdollar_sort('tp')">Throughput</th>
              <th onclick="window.__subdollar_sort('api_cost')">API cost & Tokens</th>
              <th onclick="window.__subdollar_sort('cost')">Effective cost</th>
              <th onclick="window.__subdollar_sort('score')">Score pts/¢</th>
              <th>Action</th>
            </tr>
          </thead>
          <tbody>
            ${renderRowsHtml(runs, showTaskCol, defaultRanks)}
          </tbody>
        </table>
      </div>
    `;
  }

  function renderCalibrationNote() {
    return `
      <div style="margin-bottom: 12px; padding: 10px 14px; background: rgba(88,166,255,0.06); border: 1px solid rgba(88,166,255,0.2); border-radius: 6px; font-size: 0.78rem; color: var(--mut); display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
        <span>📐</span>
        <span><strong>Scoring Regime:</strong> Throughput scores only where a live reference anchors them — <strong>redis/http</strong> only (live reference). <strong>PDF/WAL/Proxy/PubSub/Binproto</strong> timings are reported but unscored (--). Ranking is server-side (P@1-lo, P@1, score, cost) — the old formula customizer is removed.</span>
      </div>
    `;
  }



  // Wilson 95% interval on raw counts (percent). Mirrors the Rust scorer.
  function wilson95(c, n) {
    if (!n) return [0, 0];
    const z = 1.96, z2 = z * z;
    const p = c / n;
    const denom = 1 + z2 / n;
    const center = (p + z2 / (2 * n)) / denom;
    const half = z * Math.sqrt(p * (1 - p) / n + z2 / (4 * n * n)) / denom;
    return [Math.max(0, center - half) * 100, Math.min(1, center + half) * 100];
  }

  // Cross-task rollup: one row per model+effort over all tasks.
  // Pooled P@1 = passing trials / all trials; cost/turns summed; avg% averaged.
  // (Quarantine gate lives in the Rust scorer; the JS rollup mirrors it.)
  function aggregateModels() {
    // 1. Group runs by (model, effort, task) and pick canonical run (highest trials, latest timestamp)
    const canonical = {};
    for (const r of RUNS_DATA) {
      if (boardTier !== 'all' && tierOf(r) !== boardTier) continue;
      const task = (r.task || '').toLowerCase();
      const key = (r.model || '?') + '\u0001' + (r.effort || 'auto') + '\u0001' + task + '\u0001' + ((r.harness || 'omp').toLowerCase()) + '\u0001' + (r.rules_version || 'legacy') + '\u0001' + (r.protocol || 'unknown');
      const n = r.total_trials || (r.subruns && r.subruns.length) || 1;
      const existing = canonical[key];
      if (!existing) {
        canonical[key] = r;
      } else {
        // Same comparability rule as Rust rollup_models: regime match
        // outranks sample size (a 9-trial row under a new ruleset must not
        // supplant a 3-trial row under the old one).
        const exN = existing.total_trials || (existing.subruns && existing.subruns.length) || 1;
        const rulesEq = (r.rules_version || null) === (existing.rules_version || null);
        const selfSt = !!r.rules_version, exSt = !!existing.rules_version;
        let take = false;
        if (rulesEq) take = n > exN || (n === exN && (r.timestamp || '') > (existing.timestamp || ''));
        else if (selfSt && !exSt) take = true;
        else if (!selfSt && exSt) take = false;
        else take = (r.timestamp || '') > (existing.timestamp || '');
        if (take) canonical[key] = r;
      }
    }

    // 2. Aggregate only canonical runs across tasks, within one
    // comparability class (regime + protocol): Q4 — never pool statistics
    // across rule sets or evaluation protocols.
    const groups = {};
    for (const r of Object.values(canonical)) {
      const key = (r.model || '?') + '\u0001' + (r.effort || 'auto') + '\u0001' + ((r.harness || 'omp').toLowerCase()) + '\u0001' + (r.rules_version || 'legacy') + '\u0001' + (r.protocol || 'unknown');
      const g = groups[key] || (groups[key] = {
        model: r.model || '?', effort: r.effort || 'auto', harness: (r.harness || 'omp'),
        regime: r.rules_version || 'legacy', protocol: r.protocol || 'unknown',
        tasks: {}, nTrials: 0, nPass: 0, sumAvg: 0, nParents: 0,
        sumCost: 0, sumApiCost: 0, sumTurns: 0,
      });
      g.tasks[r.task] = true;
      const n = r.total_trials || (r.subruns && r.subruns.length) || 1;
      const c = (r.subruns && r.subruns.length)
        ? r.subruns.filter(sr => (sr.pass_rate || 0) >= 100).length
        : (((r.avg_pass_rate !== undefined && r.avg_pass_rate !== null) ? r.avg_pass_rate : r.pass_rate) >= 100 ? n : 0);
      g.nTrials += n;
      g.nPass += c;
      g.sumAvg += ((r.avg_pass_rate !== undefined && r.avg_pass_rate !== null) ? r.avg_pass_rate : (r.pass_rate || 0));
      g.nParents += 1;
      g.sumCost += (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null) ? r.effective_cost_usd : (r.total_cost_usd || 0);
      g.sumApiCost += (r.total_cost_usd || 0);
      g.sumTurns += (r.turns || 0);
    }
    const rows = Object.values(groups).map(g => {
      const [lo, hi] = wilson95(g.nPass, g.nTrials);
      return {
        model: g.model, effort: g.effort, harness: g.harness,
        regime: g.regime, protocol: g.protocol,
        nTasks: Object.keys(g.tasks).length,
        tasks: Object.keys(g.tasks).sort().join(', '),
        avgPct: g.sumAvg / Math.max(1, g.nParents),
        p1: g.nTrials ? (100 * g.nPass / g.nTrials) : 0,
        lo, hi, nTrials: g.nTrials, nPass: g.nPass,
        cost: g.sumCost, apiCost: g.sumApiCost, turns: g.sumTurns,
      };
    });
    rows.sort((a, b) => (b.lo - a.lo) || (b.p1 - a.p1) || (a.cost - b.cost));
    return rows;
  }

  function modelsSortVal(r, col) {
    if (col === 'model') return (r.model + ' ' + r.effort + ' ' + r.harness).toLowerCase();
    if (col === 'tasks') return r.nTasks;
    if (col === 'avg') return r.avgPct;
    if (col === 'cost') return r.cost;
    if (col === 'apiCost') return r.apiCost;
    if (col === 'turns') return r.turns;
    if (col === 'p1') return r.p1;
    return r.lo;
  }

  function renderModelsView() {
    const q = (typeof searchQuery !== 'undefined' ? (searchQuery || '') : '').toLowerCase();
    const rows = aggregateModels().filter(r => !q || ((r.model + ' ' + r.effort + ' ' + r.harness).toLowerCase().includes(q)));
    rows.sort((a, b) => {
      const va = modelsSortVal(a, modelsSortCol);
      const vb = modelsSortVal(b, modelsSortCol);
      if (typeof va === 'string') return modelsSortDir === 1 ? va.localeCompare(vb) : vb.localeCompare(va);
      if (va === vb) return (b.lo - a.lo) || (b.p1 - a.p1) || (a.cost - b.cost);
      return modelsSortDir === 1 ? (va - vb) : (vb - va);
    });
    const body = rows.map((r, i) => {
      const medal = i === 0 ? '🥇' : i === 1 ? '🥈' : i === 2 ? '🥉' : `#${i + 1}`;
      const meta = getModelMeta(r.model);
      return `<tr>
        <td style="text-align: center;">${medal}</td>
        <td><div style="display: flex; align-items: center; gap: 6px; flex-wrap: wrap;">
          <div class="model-pill" style="background: ${meta.bg}; border: 1px solid ${meta.border};">
            <span class="model-icon" title="${meta.family}">${meta.icon}</span>
            <span style="font-weight: 600; color: ${meta.color};">${escapeHtml(shortModelName(r.model))}</span>
          </div>
          <span class="badge badge-effort">effort: ${escapeHtml(r.effort)}</span>
            <span class="badge" title="Agent harness">harness: ${escapeHtml(r.harness || 'omp')}</span>
            <span class="badge badge-protocol" title="rules regime ${escapeHtml(r.regime || 'legacy')}">${escapeHtml(r.protocol || 'unknown')}</span>
        </div></td>
        <td style="text-align: center;">${r.nTasks} <span style="font-size: 0.7rem; color: var(--mut);">${escapeHtml(r.tasks)}</span></td>
        <td style="text-align: center; font-weight: 700;">${r.avgPct.toFixed(0)}%</td>
        <td style="text-align: center; font-weight: 700;">${r.p1.toFixed(0)}% <span style="font-size: 0.7rem; color: var(--mut);">(${r.lo.toFixed(0)}–${r.hi.toFixed(0)})</span>
          <div style="font-size: 0.7rem; color: var(--mut);">${r.nPass}/${r.nTrials} trials</div></td>
        <td style="text-align: center;">$${r.apiCost.toFixed(4)}</td>
        <td style="text-align: center;">$${r.cost.toFixed(4)}</td>
        <td style="text-align: center;">${r.turns}</td>
      </tr>`;
    }).join('\n');
    const arrow = (c) => modelsSortCol === c ? (modelsSortDir === 1 ? ' ▲' : ' ▼') : '';
    return `
      <div style="margin-bottom: 10px; padding: 8px 12px; background: rgba(88,166,255,0.06); border: 1px solid rgba(88,166,255,0.2); border-radius: 6px; font-size: 0.76rem; color: var(--mut);">
        🏅 <strong>Cross-task rollup</strong>: one row per model+effort+harness across all tasks. Ranked by pooled P@1 lower bound — the honest answer to "which cheap model codes best".
      </div>
      <div class="table-wrap"><table>
        <thead><tr>
          <th style="width: 40px;">#</th>
          <th onclick="window.__subdollar_models_sort('model')" style="cursor: pointer;">Model & Effort${arrow('model')}</th>
          <th onclick="window.__subdollar_models_sort('tasks')" style="cursor: pointer;">Tasks${arrow('tasks')}</th>
          <th onclick="window.__subdollar_models_sort('avg')" style="cursor: pointer;">Mean avg%${arrow('avg')}</th>
          <th onclick="window.__subdollar_models_sort('p1lo')" style="cursor: pointer;">Pooled P@1 (95% CI)${arrow('p1lo')}</th>
          <th onclick="window.__subdollar_models_sort('apiCost')" style="cursor: pointer;">Total API cost${arrow('apiCost')}</th>
          <th onclick="window.__subdollar_models_sort('cost')" style="cursor: pointer;">Total effective cost${arrow('cost')}</th>
          <th onclick="window.__subdollar_models_sort('turns')" style="cursor: pointer;">Total turns${arrow('turns')}</th>
        </tr></thead>
        <tbody>${body}</tbody>
      </table></div>`;
  }

  function updateLeaderboardView() {
    const container = document.getElementById('portalDynamicContainer');
    if (!container) return;

    ['models'].forEach(t => {
      const btn = document.getElementById('tabBtn_' + t);
      if (btn) {
        if (t === currentTab) btn.classList.add('active');
        else btn.classList.remove('active');
      }
    });

    if (currentTab === 'models') {
      container.innerHTML = `
        ${renderCalibrationNote()}
        ${renderModelsView()}
      `;
    } else {
      const runs = getSortedFilteredRuns(RUNS_DATA, currentTab, searchQuery, passOnly);
      const defTask = computeDefaultRanks(RUNS_DATA, currentTab);
      container.innerHTML = `
        ${renderCalibrationNote()}
        ${renderTaskBanner(currentTab, RUNS_DATA)}
        ${renderTableContainer(runs, false, '', '', defTask)}
      `;
    }

    container.querySelectorAll('tbody tr').forEach(row => {
      row.addEventListener('click', e => {
        if (e.target.tagName === 'A' || e.target.closest('a') || e.target.tagName === 'SUMMARY' || e.target.closest('details')) return;
        const href = row.getAttribute('data-href');
        if (href) window.location.href = href;
      });
    });
  }

  window.__subdollar_models_sort = function(col) {
    if (modelsSortCol === col) modelsSortDir = -modelsSortDir;
    else { modelsSortCol = col; modelsSortDir = (col === 'model') ? 1 : -1; }
    updateLeaderboardView();
  };

  window.__subdollar_set_tab = function(tab) {
    currentTab = tab;
    updateLeaderboardView();
  };

  window.__subdollar_search = function(val) {
    searchQuery = val || '';
    updateLeaderboardView();
  };

  window.__subdollar_pass_toggle = function(checked) {
    passOnly = !!checked;
    updateLeaderboardView();
  };

  window.__subdollar_tier = function(v) {
    boardTier = (v === 'free' || v === 'paid') ? v : 'all';
    const btns = document.querySelectorAll('[data-tier]');
    for (let i = 0; i < btns.length; i++) {
      const on = btns[i].getAttribute('data-tier') === boardTier;
      if (on) btns[i].classList.add('active');
      else btns[i].classList.remove('active');
    }
    updateLeaderboardView();
  };

  window.__subdollar_sort = function(col) {
    if (sortCol === col) {
      sortDir = -sortDir;
    } else {
      sortCol = col;
      sortDir = -1;
    }
    updateLeaderboardView();
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', updateLeaderboardView);
  } else {
    updateLeaderboardView();
  }
})();
"#
);

pub struct PortalExporter;

/// One campaign run on the public sdb-sql board (2026-10-17: the only
/// benchmark task; the pre-sdb-sql rollup is retired).
struct SqlBoardCell {
    /// The campaign dir name; the `×N runs` disclosure names each run.
    campaign_id: String,
    model: String,
    harness: String,
    effort: String,
    state: String,
    /// Highest ticket whose acceptance suite passed (with the continue
    /// ladder this is a maximum, not a contiguous prefix).
    reached: Option<u32>,
    /// Tickets passed without regressing an earlier ticket.
    regression_free: u32,
    /// Difficulty-weighted points scored (the proposal's headline
    /// numerator: tier x weight per pass, so skipping a hard ticket cannot
    /// outrank passing it).
    points: f64,
    max_points: f64,
    /// The grading rule set the cell ran under.
    rules: String,
    /// R28-1: the oracle and cap policy recorded at grading time.
    sqlite_version: Option<String>,
    cap_price_basis: Option<String>,
    cost: f64,
    over_budget: bool,
    /// Stopped with headroom under the floor (round-11 finding 2).
    cap_reached: bool,
    over_wall: bool,
    updated: String,
    /// The full ladder (attempted and not): the same Rust derivation the
    /// console uses (gui-spec.md §4.1/§6.6).
    ladder: Vec<crate::campaign::LadderEntry>,
    /// The shared board ranking key.
    key: crate::campaign::BoardSortKey,
}

impl SqlBoardCell {
    /// The headline: points per campaign dollar.
    fn points_per_dollar(&self) -> Option<f64> {
        (self.cost > 0.0).then(|| self.points / self.cost)
    }

    /// The cost-efficiency column next to it: regression-free tickets per
    /// dollar.
    fn reg_free_per_dollar(&self) -> Option<f64> {
        (self.cost > 0.0).then(|| self.regression_free as f64 / self.cost)
    }
}

impl PortalExporter {
    /// Exports a static read-only portal: leaderboard `index.html`, per-run pages
    /// with redacted logs, and a machine-readable `results.json`.
    pub fn export(
        runs_dir: &str,
        results_dir: &str,
        out: &str,
        base_url: &str,
        keep: usize,
    ) -> Result<PortalSummary> {
        let runs_path = Path::new(runs_dir);
        let mut results = LeaderboardManager::load_all(results_dir);
        results.truncate(keep);
        // Quarantined tasks stay archived in results/DB but are never
        // published per-run either. An explicit ?task=<name> still
        // renders those rows from the API/CLI.
        let visible: Vec<_> = results
            .into_iter()
            .filter(|r| {
                !is_hidden(runs_path, &r.id)
                    && !is_poisoned(runs_path, &r.id)
                    && !r.id.starts_with("test_")
                    && !crate::report::is_quarantined(&r.task)
                    && is_rankable_status(r.status.as_deref())
            })
            .collect();
        let pre_freeze = visible.len();
        let visible: Vec<_> = visible
            .into_iter()
            .filter(|r| publishable(&r.timestamp))
            .collect();
        let frozen_cut = pre_freeze - visible.len();
        if frozen_cut > 0 {
            eprintln!(
                "[PORTAL] freeze: excluded {frozen_cut} pre-freeze row(s) (timestamp < {PUBLISH_FREEZE_TS})"
            );
        }
        let out_dir = Path::new(out);
        let runs_out = out_dir.join("runs");
        fs::create_dir_all(&runs_out)
            .with_context(|| format!("create portal dir {}", out_dir.display()))?;

        let secrets = Self::secret_patterns();
        fs::write(
            out_dir.join("index.html"),
            Self::render_index(&visible, base_url),
        )?;
        fs::write(
            out_dir.join("results.json"),
            serde_json::to_string_pretty(&visible)?,
        )?;

        let mut count = 0;
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &visible {
            let clean = Path::new(&r.id)
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or(&r.id)
                .to_string();
            live.insert(clean);
            if Self::export_run(runs_path, &runs_out, &r.id, &secrets).is_ok() {
                count += 1;
            }
            for sub in &r.subruns {
                let clean_sub = Path::new(&sub.id)
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or(&sub.id)
                    .to_string();
                live.insert(clean_sub);
                let _ = Self::export_run(runs_path, &runs_out, &sub.id, &secrets);
            }
        }
        // Prune run pages (e.g. test residue) with no current result entry.
        if let Ok(entries) = fs::read_dir(&runs_out) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if entry.path().is_dir() && !live.contains(&name) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        Ok(PortalSummary {
            run_count: count,
            out_dir: out_dir.to_path_buf(),
        })
    }

    /// The public board for the sdb-sql campaign: one cell per campaign
    /// run, ranked by the ticket it reached and then by regression-free
    /// tickets per dollar. Replaces the pre-sdb-sql per-task rollup on the
    /// served portal (those tasks are retired and their runs no longer
    /// publish). Writes ONLY `index.html`.
    pub fn export_sql_board(runs_dir: &str, out: &str) -> Result<PortalSummary> {
        let mut cells: Vec<SqlBoardCell> = Vec::new();
        let mut specs: std::collections::HashMap<String, Option<crate::campaign::CampaignSpec>> =
            std::collections::HashMap::new();
        if let Ok(rd) = fs::read_dir(runs_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("campaign_") || !e.path().is_dir() {
                    continue;
                }
                let read = |f: &str| {
                    fs::read_to_string(e.path().join(f))
                        .ok()
                        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                };
                // `campaign.json` is the final rollup; `status.json` keeps
                // the live one (and the state while the cell runs).
                let status_doc = read("status.json");
                let updated_at = status_doc
                    .as_ref()
                    .and_then(|t| t["updated_at"].as_str().map(str::to_string));
                let (rollup, state) = match read("campaign.json") {
                    Some(c) => {
                        let state = status_doc
                            .as_ref()
                            .and_then(|t| t["state"].as_str().map(str::to_string))
                            .unwrap_or_else(|| "done".to_string());
                        (c, state)
                    }
                    None => match status_doc {
                        Some(t) => match t.get("rollup").filter(|r| !r.is_null()).cloned() {
                            Some(r) => {
                                let state = t["state"].as_str().unwrap_or("unknown").to_string();
                                (r, state)
                            }
                            None => continue,
                        },
                        None => continue,
                    },
                };
                // Round-12 P2: the same derivation the console uses — a dead
                // writer is stalled, never published as RUNNING.
                let state = crate::campaign::effective_state(&name, &state, updated_at.as_deref());
                // Only the active campaign publishes (one source of truth:
                // `campaign::ACTIVE_CAMPAIGNS`).
                if !crate::campaign::ACTIVE_CAMPAIGNS
                    .iter()
                    .any(|c| rollup["name"].as_str() == Some(*c))
                {
                    continue;
                }
                // A cell whose stage runs were hidden or poisoned is not a
                // measurement: never publish it.
                let runs_path = Path::new(runs_dir);
                // Round-13: a hidden or poisoned *cell* never publishes. The
                // flag lives in the campaign dir; the stage run ids a rollup
                // carries are synthetic for campaign cells.
                if is_hidden(runs_path, &name) || is_poisoned(runs_path, &name) {
                    continue;
                }
                let flagged = rollup["stages"]
                    .as_array()
                    .map(|ss| {
                        ss.iter().any(|st| {
                            st["run_id"]
                                .as_str()
                                .map(|rid| is_hidden(runs_path, rid) || is_poisoned(runs_path, rid))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if flagged {
                    continue;
                }
                // Parse once for the shared key + ladder; a merge with the
                // spec failures simply drops the strip.
                let mut parsed: Option<crate::campaign::CampaignRollup> =
                    serde_json::from_value(rollup.clone()).ok();
                // R22-5: the portal export gets the same crash relabel as
                // the API list and detail, independent of recorded rounds.
                if let Some(r) = parsed.as_mut() {
                    crate::campaign::relabel_graded_crashes(r, &e.path());
                }
                // Q5: phase cells stay off the public board until careers
                // have their own ranked row; the console can show them.
                if rollup.get("phase").and_then(|p| p.as_u64()).is_some() {
                    continue;
                }
                // The sort key comes from the raw fields, so an
                // unparseable (or sparse) rollup still ranks correctly.
                let points_v = rollup["points"].as_f64().unwrap_or(0.0);
                let cost_v = rollup["total_cost_usd"].as_f64().unwrap_or(0.0);
                let rules_v = rollup["rules_version"].as_str().unwrap_or("").to_string();
                cells.push(SqlBoardCell {
                    campaign_id: name.clone(),
                    model: rollup["model"].as_str().unwrap_or("?").to_string(),
                    harness: rollup["harness"].as_str().unwrap_or("?").to_string(),
                    effort: rollup["effort"].as_str().unwrap_or("high").to_string(),
                    state,
                    reached: rollup["reached"].as_u64().map(|v| v as u32),
                    regression_free: rollup["regression_free_tickets"].as_u64().unwrap_or(0) as u32,
                    points: points_v,
                    max_points: rollup["max_points"].as_f64().unwrap_or(0.0),
                    rules: rules_v.clone(),
                    sqlite_version: rollup["sqlite_version"].as_str().map(str::to_string),
                    cap_price_basis: rollup["cap_price_basis"].as_str().map(str::to_string),
                    cost: cost_v,
                    over_budget: rollup["over_budget"].as_bool().unwrap_or(false),
                    cap_reached: rollup["cap_reached"].as_bool().unwrap_or(false),
                    over_wall: rollup["over_wall"].as_bool().unwrap_or(false),
                    updated: rollup["finished_at"]
                        .as_str()
                        .or_else(|| rollup["started_at"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    ladder: parsed
                        .as_ref()
                        .and_then(|r| {
                            specs
                                .entry(r.name.clone())
                                .or_insert_with(|| {
                                    crate::campaign::load_campaign(&r.name).ok().map(|l| l.spec)
                                })
                                .as_ref()
                                .map(|spec| crate::campaign::ladder(spec, r, None))
                        })
                        .unwrap_or_default(),
                    key: crate::campaign::BoardSortKey {
                        live: false,
                        current_rules: crate::campaign::is_current_rules(&rules_v),
                        points: points_v,
                        per_dollar: (cost_v > 0.0).then(|| points_v / cost_v),
                        cost: cost_v,
                    },
                });
            }
        }
        // ONE ranking for console, API and board (gui-spec.md §4.1).
        cells.sort_by(|a, b| crate::campaign::board_order(&a.key, &b.key));
        let out_dir = Path::new(out);
        fs::create_dir_all(out_dir)
            .with_context(|| format!("create portal dir {}", out_dir.display()))?;
        fs::write(out_dir.join("index.html"), Self::render_sql_board(&cells))?;
        Ok(PortalSummary {
            run_count: cells.len(),
            out_dir: out_dir.to_path_buf(),
        })
    }

    /// One labelled ladder square (round-12 P3): the ticket number inside,
    /// the full story on hover — the console's square, one renderer shape.
    fn ladder_square_html(e: &crate::campaign::LadderEntry) -> String {
        let (bg, fg, cls) = match e.status.as_str() {
            "pass" => ("background:#3fb950;", "color:#0d1117;", "pass"),
            "lite" => ("background:#d29922;", "color:#0d1117;", "lite"),
            "fail" => ("background:#f85149;", "color:#0d1117;", "fail"),
            "running" => ("background:#58a6ff;", "color:#0d1117;", "run"),
            "fault" => (
                "background:repeating-linear-gradient(45deg,#8b949e,#8b949e 2px,transparent 2px,transparent 4px);",
                "color:#c9d1d9;",
                "fault",
            ),
            "skipped" => ("", "", "skip"),
            _ => ("", "", "na"),
        };
        let tip = format!(
            "ticket {} · {} · {}{}",
            e.id,
            e.title,
            e.status,
            e.cause
                .as_deref()
                .map(|c| format!(" · {c}"))
                .unwrap_or_default()
        );
        format!(
            "<span class=\"sq {cls}\" title=\"{}\" style=\"{bg}{fg}\">{}</span>",
            Self::esc(&tip),
            e.id
        )
    }

    /// The ladder mini-strip (gui-spec.md §4.1): one labelled square per
    /// ticket, status-coloured, with the one-line cause on hover. The same
    /// Rust ladder derivation the console renders.
    fn ladder_strip_html(ladder: &[crate::campaign::LadderEntry]) -> String {
        if ladder.is_empty() {
            return String::new();
        }
        let inner: String = ladder.iter().map(Self::ladder_square_html).collect();
        format!("<div class=\"strip\">{inner}</div>")
    }

    /// The one legend for every strip (round-12 P3), the console's labels.
    fn ladder_legend_html() -> &'static str {
        "<div class=\"legend\"><span class=\"sq pass\">·</span>pass<span class=\"sq lite\">·</span>lite<span class=\"sq fail\">·</span>fail<span class=\"sq fault\">·</span>fault<span class=\"sq skip\">·</span>skipped<span class=\"sq run\">·</span>running<span class=\"sq na\">·</span>not reached</div>"
    }

    /// The sdb-sql board page: campaign cells only, no legacy task specs,
    /// no per-run detail.
    fn render_sql_board(cells: &[SqlBoardCell]) -> String {
        let now = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        let best = cells
            .iter()
            .filter_map(|c| c.reached)
            .max()
            .map(|b| format!("ticket {b}"))
            .unwrap_or_else(|| "—".to_string());
        let best_points = cells.iter().map(|c| c.points).fold(0.0f64, f64::max);
        let rf: u32 = cells.iter().map(|c| c.regression_free).sum();
        let spend: f64 = cells.iter().map(|c| c.cost).sum();
        // Per-cell renderings, shared by the head row and the `×N runs`
        // disclosure (round-12 P3: the portal renders what the console does).
        let reached_of = |c: &SqlBoardCell| {
            c.reached
                .map(|r| format!("ticket {r}"))
                .unwrap_or_else(|| "—".to_string())
        };
        let ppd_of = |c: &SqlBoardCell| {
            c.points_per_dollar()
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".to_string())
        };
        let rpd_of = |c: &SqlBoardCell| {
            c.reg_free_per_dollar()
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".to_string())
        };
        let state_of = |c: &SqlBoardCell| match c.state.as_str() {
            "running" => "<span class=\"b run\">RUNNING</span>".to_string(),
            "done" => "<span class=\"b ok\">done</span>".to_string(),
            other => format!("<span class=\"b\">{}</span>", Self::esc(other)),
        };
        let flags_of = |c: &SqlBoardCell| {
            let mut f = String::new();
            if c.over_budget {
                f.push_str(" · <span class=\"b warn\">over budget</span>");
            }
            if c.cap_reached {
                // A floor stop is planned, not an overshoot.
                f.push_str(" · <span class=\"b\">budget cap</span>");
            }
            if c.over_wall {
                f.push_str(" · <span class=\"b warn\">over wall</span>");
            }
            f
        };
        let rules_of = |c: &SqlBoardCell| {
            let base = if c.rules == crate::report::rules::RULES_VERSION {
                format!("<span class=\"mut\">{}</span>", Self::esc(&c.rules))
            } else if c.rules.is_empty() {
                "<span class=\"b warn\">rules —</span>".to_string()
            } else {
                format!(
                    "<span class=\"b warn\" title=\"graded under a different rule set\">stale {}</span>",
                    Self::esc(&c.rules)
                )
            };
            let mut text = format!(
                "{base} · SQLite {}",
                Self::esc(crate::report::rules::recorded_sqlite_version(
                    c.sqlite_version.as_deref()
                ))
            );
            if crate::report::rules::recorded_cap_basis(&c.rules, c.cap_price_basis.as_deref())
                .is_some()
            {
                text.push_str(" · cap at list price");
            }
            text
        };
        // Repeat runs of one model collapse into one row with a `×N runs`
        // disclosure (round-12 P3), first-seen order preserved: the shared
        // board_order already put the best cell first.
        let mut groups: Vec<(&SqlBoardCell, Vec<&SqlBoardCell>)> = Vec::new();
        for c in cells {
            // Round-13 B1: runs under incomparable rule sets must never share
            // a row — the invalid `.17` cell used to hide the valid `.16` one.
            match groups.iter_mut().find(|(h, _)| {
                h.model == c.model
                    && h.harness == c.harness
                    && h.effort == c.effort
                    && h.rules == c.rules
            }) {
                Some((_, g)) => g.push(c),
                None => groups.push((c, Vec::new())),
            }
        }
        let mut rows = String::new();
        let mut divider_done = false;
        for (i, (c, extra)) in groups.iter().enumerate() {
            // Round-12 P3 + round-13 B2: the console's divider, through the
            // shared predicate (unknown rules count as not current).
            if !divider_done && !c.key.current_rules {
                rows.push_str(
                    "<tr class=\"divider\"><td colspan=\"10\">older rule sets (incomparable)</td></tr>",
                );
                divider_done = true;
            }
            let more = if extra.is_empty() {
                String::new()
            } else {
                let mut sorted: Vec<f64> = std::iter::once(*c)
                    .chain(extra.iter().copied())
                    .map(|x| x.points)
                    .collect();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                // P1 (round 14): an even count's median is the mean of the
                // two middle values (7 and 13 -> 10.0, not 13.0).
                let n = sorted.len();
                let median = if n.is_multiple_of(2) {
                    (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
                } else {
                    sorted[n / 2]
                };
                let mut inner = format!(
                    "<div class=\"line\">median {median:.1} pts across {} runs</div>",
                    extra.len() + 1
                );
                for x in extra {
                    inner.push_str(&format!(
                        "<div class=\"line\">{}{} · {} · {} pts · ${:.4} · {} · {} <span class=\"mut\">{}</span></div>",
                        rules_of(x),
                        flags_of(x),
                        reached_of(x),
                        x.points,
                        x.cost,
                        ppd_of(x),
                        state_of(x),
                        Self::esc(&x.campaign_id),
                    ));
                }
                format!(
                    " <details class=\"more\"><summary>×{} runs ▸</summary>{inner}</details>",
                    extra.len() + 1
                )
            };
            let strip = Self::ladder_strip_html(&c.ladder);
            rows.push_str(&format!(
                "<tr><td class=\"num\">{}</td><td><b>{}</b>{}<div class=\"mut\">{} · effort {}{}</div>{strip}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}/{} · {}</td><td class=\"num\">${:.4}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}{}</td><td class=\"mut\">{}</td></tr>",
                i + 1,
                Self::esc(&c.model),
                more,
                Self::esc(&c.harness),
                Self::esc(&c.effort),
                flags_of(c),
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
                Self::esc(c.updated.get(..16).unwrap_or(&c.updated)),
            ));
        }
        let mut h = String::new();
        h.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>SubDollarBench — sdb-sql campaign</title><style>");
        h.push_str(include_str!("portal_board.css"));
        h.push_str("</style></head><body><main>");
        h.push_str("<header><div><h1>💰 SubDollarBench — sdb-sql campaign</h1><div class=\"sub\">One ticket ladder, one task: a differential SQL database — its own on-disk format, checked statement by statement against SQLite. Ranked by difficulty-weighted points, then by points per dollar.</div></div><div class=\"right\"><a href=\"/\">🚀 Benchmark Runner &amp; Console</a>");
        h.push_str(&format!(
            "<div class=\"mut\">Updated {}</div></div></header>",
            Self::esc(&now)
        ));
        h.push_str(&format!(
            "<div class=\"sub\">Current binary: SQLite {} · cap policy: at list price (model_prices.json) for runs from .39; each row records its own.</div>",
            Self::esc(crate::report::rules::sqlite_version())
        ));
        h.push_str(&format!(
            "<div class=\"cards\"><div class=\"card\"><div class=\"k\">Cells</div><div class=\"v\">{}</div></div><div class=\"card\"><div class=\"k\">Best reached</div><div class=\"v\">{best}</div></div><div class=\"card\"><div class=\"k\">Best points</div><div class=\"v\">{best_points:.1}</div></div><div class=\"card\"><div class=\"k\">Regression-free tickets</div><div class=\"v\">{rf}</div></div><div class=\"card\"><div class=\"k\">Spend</div><div class=\"v\">${spend:.4}</div></div></div>",
            cells.len()
        ));
        h.push_str("<div class=\"wrap\"><table><thead><tr><th>#</th><th>Model</th><th>Reached</th><th>Reg-free</th><th>Points</th><th>Spend</th><th>Pts / $</th><th>Reg-free / $</th><th>State</th><th>Updated</th></tr></thead><tbody>");
        if rows.is_empty() {
            h.push_str("<tr><td colspan=\"10\" class=\"mut\">No sdb-sql campaign cells yet (run one with <code>scripts/sql-cell.sh &lt;model&gt;</code>).</td></tr>");
        } else {
            h.push_str(&rows);
        }
        h.push_str("</tbody></table>");
        h.push_str(Self::ladder_legend_html());
        h.push_str("</div>");
        h.push_str("<section class=\"methodology\"><h2>How this board is ranked</h2><ul><li><b>Points</b>: the campaign's difficulty-weighted score (each ticket contributes its tier x weight when it passes), so a cell that skips a hard ticket cannot outrank one that passed it. <b>Pts / $</b> divides by the cell's effective spend (API + compute).</li><li>Ranking: points desc, then points per $ desc, then the cheaper cell, with cells graded under a different RULES_VERSION grouped below the current ones (grading changes make them incomparable).</li><li><b>Reached</b> is the highest ticket whose suite passed; with the continue ladder this is a maximum, not a contiguous prefix. <b>Regression-free tickets</b> passed without regressing an earlier ticket's suite, and <b>Reg-free / $</b> is that count over the spend.</li><li>Repeat runs of one model, harness and effort collapse into one row with a <b>&times;N runs</b> disclosure (best run shown, median noted); the ladder squares carry the ticket numbers and the legend sits under the table.</li><li>Cells whose stage runs were hidden or poisoned are never published; over-budget, budget-cap and over-wall cells are badged.</li><li><b>Retired 2026-10-17</b>: the pre-sdb-sql tasks (redis/http/pdf/wal/proxy/pubsub/binproto) and their campaigns are no longer part of the benchmark; only the sdb-sql ladder publishes.</li></ul></section>");
        h.push_str("</main></body></html>");
        h
    }

    /// Model-rollup-only export for the public portal: one row per model
    /// across ALL tasks. No per-run detail, no run ids or links, no
    /// console/workspace content. Writes ONLY `index.html` and leaves any
    /// existing `runs/` directory as-is, so stale per-run pages are never
    /// served as fresh output. Per-run detail stays internal (see `export`
    /// above); the public portal is aggregated by design.
    pub fn export_rollup(results_dir: &str, out: &str, base_url: &str) -> Result<PortalSummary> {
        let results = LeaderboardManager::load_all(results_dir);
        // Quarantined tasks stay archived in results/DB but never publish:
        // the rollup ranks comparable active tasks only. Hidden runs likewise
        // never feed public aggregates (matches the per-run export filter).
        let runs_dir = crate::report::archive::RunArchiver::resolve_runs_dir();
        let results: Vec<_> = results
            .into_iter()
            .filter(|r| {
                !crate::report::is_quarantined(&r.task)
                    && !is_hidden(&runs_dir, &r.id)
                    && !is_poisoned(&runs_dir, &r.id)
                    && is_rankable_status(r.status.as_deref())
            })
            .collect();
        let pre_freeze = results.len();
        let fresh: Vec<_> = results
            .into_iter()
            .filter(|r| publishable(&r.timestamp))
            .collect();
        let frozen_cut = pre_freeze - fresh.len();
        if frozen_cut > 0 {
            eprintln!(
                "[PORTAL] freeze: excluded {frozen_cut} pre-freeze row(s) from rollup (timestamp < {PUBLISH_FREEZE_TS})"
            );
        }
        let rows = Self::apply_rollup_floor(Self::rollup_models(&fresh));
        let out_dir = Path::new(out);
        fs::create_dir_all(out_dir)
            .with_context(|| format!("create portal dir {}", out_dir.display()))?;
        fs::write(
            out_dir.join("index.html"),
            Self::render_rollup_index(&rows, base_url),
        )?;
        Ok(PortalSummary {
            run_count: rows.len(),
            out_dir: out_dir.to_path_buf(),
        })
    }

    /// Aggregate parents into one row per model+harness across all tasks.
    /// Step 1 (dedup): one canonical parent per (model, effort, task,
    /// harness, rules regime, protocol) — highest trial count wins, latest
    /// timestamp breaks ties — mirroring the `aggregateModels` cross-task
    /// rollup in PORTAL_JS. Step 2: group canonical parents by (model,
    /// harness, regime, protocol): rank only within one comparability
    /// class (scoring-v2 §1, operator decision Q4) — pooled statistics
    /// must never mix rule sets or evaluation protocols, and each row
    /// carries its protocol badge.
    fn rollup_models(
        results: &[crate::report::leaderboard::BenchmarkRunResult],
    ) -> Vec<ModelRollupRow> {
        let mut canonical: HashMap<
            (String, String, String, String, String, String),
            &crate::report::leaderboard::BenchmarkRunResult,
        > = HashMap::new();
        for r in results {
            // Publish hygiene: test/CI probe rows never reach the rollup.
            if r.id.starts_with("test_") {
                continue;
            }
            let key = (
                r.model.clone(),
                r.effort.clone().unwrap_or_else(|| "auto".to_string()),
                r.task.to_lowercase(),
                crate::report::leaderboard::BenchmarkRunResult::harness_of(r).to_string(),
                crate::report::leaderboard::BenchmarkRunResult::rules_of(r),
                r.protocol.clone().unwrap_or_else(|| "unknown".to_string()),
            );
            let n = r.total_trials.unwrap_or(r.subruns.len() as u32).max(1);
            let replace = match canonical.get(&key) {
                None => true,
                Some(ex) => {
                    let ex_n = ex.total_trials.unwrap_or(ex.subruns.len() as u32).max(1);
                    n > ex_n || (n == ex_n && r.timestamp > ex.timestamp)
                }
            };
            if replace {
                canonical.insert(key, r);
            }
        }

        struct Acc {
            tasks: HashSet<String>,
            n_parents: usize,
            n_trials: usize,
            n_pass: usize,
            sum_avg: f64,
            sum_cost: f64,
            sum_api_cost: f64,
            worst_stab: u8,
        }
        fn stab_rank(stability: Option<&str>) -> u8 {
            match stability {
                Some("unstable") => 3,
                Some("watch") => 2,
                Some("stable") => 1,
                _ => 0,
            }
        }
        let mut groups: HashMap<(String, String, String, String), Acc> = HashMap::new();
        for r in canonical.into_values() {
            let g = groups
                .entry((
                    r.model.clone(),
                    crate::report::leaderboard::BenchmarkRunResult::harness_of(r).to_string(),
                    crate::report::leaderboard::BenchmarkRunResult::rules_of(r),
                    r.protocol.clone().unwrap_or_else(|| "unknown".to_string()),
                ))
                .or_insert_with(|| Acc {
                    tasks: HashSet::new(),
                    n_parents: 0,
                    n_trials: 0,
                    n_pass: 0,
                    sum_avg: 0.0,
                    sum_cost: 0.0,
                    sum_api_cost: 0.0,
                    worst_stab: 0,
                });
            g.tasks.insert(r.task.clone());
            let n = r.total_trials.unwrap_or(r.subruns.len() as u32).max(1) as usize;
            let c = if r.subruns.is_empty() {
                if r.avg_pass_rate.unwrap_or(r.pass_rate) >= 100.0 {
                    n
                } else {
                    0
                }
            } else {
                r.subruns.iter().filter(|s| s.pass_rate >= 100.0).count()
            };
            g.n_parents += 1;
            g.n_trials += n;
            g.n_pass += c;
            g.sum_avg += r.avg_pass_rate.unwrap_or(r.pass_rate);
            // Prefer effective (API + compute) cost, fall back to API-only.
            g.sum_cost += r.effective_cost_usd.unwrap_or(r.total_cost_usd);
            g.sum_api_cost += r.total_cost_usd;
            g.worst_stab = g.worst_stab.max(stab_rank(r.stability.as_deref()));
        }

        let mut rows: Vec<ModelRollupRow> = groups
            .into_iter()
            .map(|((model, harness, regime, protocol), g)| {
                let (p1_lo, p1_hi) = wilson_interval_95(g.n_trials, g.n_pass);
                let mut tasks: Vec<String> = g.tasks.into_iter().collect();
                tasks.sort();
                ModelRollupRow {
                    harness,
                    regime,
                    protocol,
                    n_tasks: tasks.len(),
                    tasks,
                    model,
                    n_parents: g.n_parents,
                    n_trials: g.n_trials,
                    n_pass: g.n_pass,
                    avg_pass_rate: g.sum_avg / g.n_parents.max(1) as f64,
                    p1: if g.n_trials > 0 {
                        100.0 * g.n_pass as f64 / g.n_trials as f64
                    } else {
                        0.0
                    },
                    p1_lo,
                    p1_hi,
                    total_cost_usd: g.sum_cost,
                    avg_cost_usd: g.sum_cost / g.n_parents.max(1) as f64,
                    total_api_cost_usd: g.sum_api_cost,
                    avg_api_cost_usd: g.sum_api_cost / g.n_parents.max(1) as f64,
                    stability: match g.worst_stab {
                        3 => Some("unstable".to_string()),
                        2 => Some("watch".to_string()),
                        1 => Some("stable".to_string()),
                        _ => None,
                    },
                }
            })
            .collect();
        // Rank only within one comparability class (regime, protocol) — the
        // class keys sort first so mixed-class tables stay contiguous and
        // each row's badge names its class (operator decision Q4).
        rows.sort_by(|a, b| {
            a.regime
                .cmp(&b.regime)
                .then_with(|| a.protocol.cmp(&b.protocol))
                .then_with(|| {
                    b.avg_pass_rate
                        .partial_cmp(&a.avg_pass_rate)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    b.p1_lo
                        .partial_cmp(&a.p1_lo)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    a.total_cost_usd
                        .partial_cmp(&b.total_cost_usd)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        rows
    }

    /// Drop below-floor rollup rows, logging the excluded models to stderr
    /// so rebuilds show exactly which thin-coverage models were cut.
    fn apply_rollup_floor(mut rows: Vec<ModelRollupRow>) -> Vec<ModelRollupRow> {
        let before = rows.len();
        rows.retain(|r| r.n_tasks >= ROLLUP_MIN_TASKS && r.n_trials >= ROLLUP_MIN_TRIALS);
        let cut = before - rows.len();
        if cut > 0 {
            eprintln!(
                "[PORTAL] rollup floor: excluded {cut} below-floor model(s) (<{} tasks or <{} trials)",
                ROLLUP_MIN_TASKS, ROLLUP_MIN_TRIALS
            );
        }
        rows
    }

    /// Rollup `index.html`: one row per model, zero per-run links or ids.
    fn render_rollup_index(rows: &[ModelRollupRow], base_url: &str) -> String {
        let now = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        let base_info = if base_url.is_empty() {
            String::new()
        } else {
            format!(" · Base: {}", Self::esc(base_url))
        };
        let mut trs = String::new();
        for (i, r) in rows.iter().enumerate() {
            let rank = match i + 1 {
                1 => "<span class=\"rank rank-1\">1</span>".to_string(),
                2 => "<span class=\"rank rank-2\">2</span>".to_string(),
                3 => "<span class=\"rank rank-3\">3</span>".to_string(),
                n => format!("<span class=\"rank\">{n}</span>"),
            };
            let (stab_dot, stab_txt) = match r.stability.as_deref() {
                Some("stable") => ("stab-ok", "stable"),
                Some("watch") => ("stab-watch", "watch"),
                Some("unstable") => ("stab-bad", "unstable"),
                _ => ("stab-na", "-"),
            };
            let stab =
                format!("<span class=\"stab\"><i class=\"{stab_dot}\"></i>{stab_txt}</span>");
            // CI bar: pooled P@1 with lo-hi whiskers on a 0-100 track.
            let ci_bar = format!(
                "<span class=\"cibar\" title=\"P@1 {p1:.0}% (95% CI {lo:.0}-{hi:.0})\"><span class=\"cifill\" style=\"left:{lo:.0}%;width:{w:.0}%\"></span><span class=\"cipoint\" style=\"left:{p1:.0}%\"></span></span>",
                p1 = r.p1, lo = r.p1_lo, hi = r.p1_hi,
                w = (r.p1_hi - r.p1_lo).max(2.0),
            );
            // Task dots: 7 active tasks, filled = covered.
            let all_tasks = ["binproto", "http", "pdf", "proxy", "pubsub", "redis", "wal"];
            let mut dots = String::new();
            for t in all_tasks {
                let on = r.tasks.iter().any(|x| x == t);
                dots.push_str(&format!(
                    "<i class=\"tdot{}\" title=\"{t}\"></i>",
                    if on { " on" } else { "" },
                ));
            }
            trs.push_str(&format!(
                "<tr class=\"mrow\"><td style=\"text-align:center;\">{rank}</td>\
                 <td><span class=\"mname\" title=\"{model}\">{short}</span> <span class=\"badge badge-effort\">{harness}</span> <span class=\"badge badge-protocol\" title=\"rules regime {regime}\">{protocol}</span><br><span class=\"tdots\" title=\"{tasks}\">{dots}</span></td>\
                 <td class=\"bignum\">{avg:.0}<span class=\"pct\">%</span></td>\
                 <td>{ci_bar}<br><span class=\"mut\">{p1:.0}% ({lo:.0}-{hi:.0})</span></td>\
                 <td style=\"text-align:center;\" title=\"API ${total_api:.4} / effective ${total:.4}\"><b>${total_api:.2}</b><br><span class=\"mut\">${avg_api:.2}/run</span></td>\
                 <td style=\"text-align:center;\">{stab}<br><span class=\"mut\">{ntrials}t/{nparents}r</span></td></tr>\n",
                rank = rank,
                model = Self::esc(&r.model),
                harness = Self::esc(&r.harness),
                protocol = Self::esc(&r.protocol),
                regime = Self::esc(&r.regime),
                short = [
                    Self::esc(LeaderboardManager::short_model_name(&r.model)),
                    Self::esc(LeaderboardManager::pricing_badge(&r.model)),
                ]
                .concat(),
                tasks = Self::esc(&r.tasks.join(", ")),
                dots = dots,
                ntrials = r.n_trials,
                nparents = r.n_parents,
                avg = r.avg_pass_rate,
                ci_bar = ci_bar,
                p1 = r.p1,
                lo = r.p1_lo,
                hi = r.p1_hi,
                total_api = r.total_api_cost_usd,
                total = r.total_cost_usd,
                avg_api = r.avg_api_cost_usd,
                stab = stab,
            ));
        }
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>SubDollarBench Model Rollup</title>\
             <style>{css}</style></head><body><main>\
             <h1>SubDollarBench Model Rollup</h1>\
             <div class=\"sub\">Public aggregate: one row per model across all tasks · {n} models · {now}{base_info} · publish freeze 2026-09-26 (pre-freeze rows excluded)</div>\
             <table class=\"roll\"><thead><tr>\
             <th></th><th>Model</th><th>Avg pass</th>\
             <th>P@1 with 95% CI</th><th>Cost (API total / per run)</th><th>Status</th>\
             </tr></thead><tbody>{trs}</tbody></table><details class=\"method\"><summary>Methodology & limitations</summary>{methodology_inner}{floor_note}</details>\
             <footer class=\"muted\">SubDollarBench rollup · {now} · per-run detail is internal and not published here</footer></main></body></html>",
            css = PORTAL_CSS,
            n = rows.len(),
            methodology_inner = PORTAL_METHODOLOGY_HTML
                .replace("<section class=\"methodology\" style=\"margin-top:2rem;padding:1rem 1.2rem;border:1px solid var(--line);border-radius:8px;\">", "")
                .replace("</section>", ""),
            floor_note = ROLLUP_FLOOR_NOTE_HTML,
            now = now,
            base_info = base_info,
            trs = trs,
        )
    }

    /// Shared scrubber: also used by [`crate::report::archive`] so raw
    /// `console.log` never lands on disk with keys (the agent env-dumps
    /// `OPENROUTER_API_KEY=sk-or-v1-...` into the log stream).
    pub fn secret_patterns() -> Vec<Regex> {
        [
            r"sk-or-v1-[A-Za-z0-9_-]+",
            r"sk-proj-[A-Za-z0-9_-]+",
            r"sk-ant-[A-Za-z0-9_-]+",
            r"sk-[A-Za-z0-9_-]{8,}",
            r"hf_[A-Za-z0-9_-]{16,}",
            r"(?i)bearer\s+[A-Za-z0-9_\-\.~\+/=]+",
            r"(?i)(?:api_key|auth_token|access_token|token)\s*[:=]\s*\S+",
            r"(?i)\b[A-Za-z0-9_]*_(?:KEY|SECRET)\s*=\s*\S+",
            r"(?i)(?:openrouter_api_key|openai_api_key|anthropic_api_key)\s*=\s*\S+",
            r"--api-key(?:\s+|=)\S+",
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    }

    pub fn redact(text: &str, patterns: &[Regex]) -> String {
        let mut out = text.to_string();
        for re in patterns {
            out = re.replace_all(&out, "***REDACTED***").to_string();
        }
        out
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    pub fn get_model_meta(
        model: &str,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        let m = model.to_lowercase();
        if m.contains("claude") || m.contains("anthropic") {
            (
                "Anthropic",
                "✦",
                "#f27244",
                "rgba(242, 114, 68, 0.14)",
                "rgba(242, 114, 68, 0.35)",
            )
        } else if m.contains("gpt")
            || m.contains("openai")
            || m.contains("o1")
            || m.contains("o3")
            || m.contains("o4")
        {
            (
                "OpenAI",
                "✳️",
                "#10a37f",
                "rgba(16, 163, 127, 0.14)",
                "rgba(16, 163, 127, 0.35)",
            )
        } else if m.contains("gemini") || m.contains("google") {
            (
                "Google",
                "✨",
                "#4285f4",
                "rgba(66, 133, 244, 0.14)",
                "rgba(66, 133, 244, 0.35)",
            )
        } else if m.contains("deepseek") {
            (
                "DeepSeek",
                "🐋",
                "#0084ff",
                "rgba(0, 132, 255, 0.14)",
                "rgba(0, 132, 255, 0.35)",
            )
        } else if m.contains("llama") || m.contains("meta") {
            (
                "Meta",
                "🦙",
                "#8b5cf6",
                "rgba(139, 92, 246, 0.14)",
                "rgba(139, 92, 246, 0.35)",
            )
        } else if m.contains("qwen") {
            (
                "Qwen",
                "🔮",
                "#a855f7",
                "rgba(168, 85, 247, 0.14)",
                "rgba(168, 85, 247, 0.35)",
            )
        } else if m.contains("mistral") || m.contains("codestral") || m.contains("pixtral") {
            (
                "Mistral",
                "🌪️",
                "#ff7000",
                "rgba(255, 112, 0, 0.14)",
                "rgba(255, 112, 0, 0.35)",
            )
        } else if m.contains("minimax") {
            (
                "MiniMax",
                "⚡",
                "#e11d48",
                "rgba(225, 29, 72, 0.14)",
                "rgba(225, 29, 72, 0.35)",
            )
        } else if m.contains("nova") || m.contains("amazon") {
            (
                "Amazon",
                "☀️",
                "#f59e0b",
                "rgba(245, 158, 11, 0.14)",
                "rgba(245, 158, 11, 0.35)",
            )
        } else if m.contains("cohere") || m.contains("command") {
            (
                "Cohere",
                "🌿",
                "#14b8a6",
                "rgba(20, 184, 166, 0.14)",
                "rgba(20, 184, 166, 0.35)",
            )
        } else if m.contains("local")
            || m.contains("ollama")
            || m.contains("llama.cpp")
            || m.contains("vllm")
        {
            (
                "Local",
                "💻",
                "#94a3b8",
                "rgba(148, 163, 184, 0.14)",
                "rgba(148, 163, 184, 0.35)",
            )
        } else {
            let colors = [
                (
                    "Other",
                    "🤖",
                    "#06b6d4",
                    "rgba(6, 182, 212, 0.14)",
                    "rgba(6, 182, 212, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#ec4899",
                    "rgba(236, 72, 153, 0.14)",
                    "rgba(236, 72, 153, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#84cc16",
                    "rgba(132, 204, 22, 0.14)",
                    "rgba(132, 204, 22, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#3b82f6",
                    "rgba(59, 130, 246, 0.14)",
                    "rgba(59, 130, 246, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#f43f5e",
                    "rgba(244, 63, 94, 0.14)",
                    "rgba(244, 63, 94, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#6366f1",
                    "rgba(99, 102, 241, 0.14)",
                    "rgba(99, 102, 241, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#10b981",
                    "rgba(16, 185, 129, 0.14)",
                    "rgba(16, 185, 129, 0.35)",
                ),
                (
                    "Other",
                    "🤖",
                    "#f59e0b",
                    "rgba(245, 158, 11, 0.14)",
                    "rgba(245, 158, 11, 0.35)",
                ),
            ];
            let hash: usize = m.bytes().fold(0usize, |acc, b| {
                acc.wrapping_mul(31).wrapping_add(b as usize)
            });
            colors[hash % colors.len()]
        }
    }

    fn render_index(
        results: &[crate::report::leaderboard::BenchmarkRunResult],
        base_url: &str,
    ) -> String {
        let http_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("http"))
            .count();
        let redis_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("redis"))
            .count();
        let pdf_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("pdf"))
            .count();
        let wal_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("wal"))
            .count();
        let proxy_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("proxy"))
            .count();
        let pubsub_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("pubsub"))
            .count();
        let passed_n = results.iter().filter(|r| r.pass_rate >= 100.0).count();
        let now = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();

        let runs_json = serde_json::to_string(results).unwrap_or_else(|_| "[]".to_string());

        // Initial server-side fallback rows for crawlers and JS-disabled view
        let mut initial_rows = String::new();
        for (i, r) in results.iter().enumerate() {
            let unit = crate::report::leaderboard::LeaderboardManager::throughput_unit(&r.task);
            let tp = r
                .throughput_req_sec
                .map(|v| format!("{:.0} {}", v, unit))
                .unwrap_or_else(|| "N/A".to_string());
            let score = r.throughput_score.unwrap_or(r.efficiency_score);
            let medal = match i + 1 {
                1 => "🥇 #1".to_string(),
                2 => "🥈 #2".to_string(),
                3 => "🥉 #3".to_string(),
                n => format!("#{}", n),
            };
            let api_c = r.total_cost_usd;
            let eff_c = r.effective_cost_usd.unwrap_or(r.total_cost_usd);
            let dur_s = r.duration_seconds.unwrap_or(0.0);
            let dur_txt = format_duration(dur_s);
            let tok_n = if r.total_tokens > 0 {
                r.total_tokens
            } else {
                r.prompt_tokens + r.cached_tokens + r.completion_tokens
            };
            let tok_txt = format!("{:.1}k tokens", tok_n as f64 / 1000.0);
            let pass_badge = if r.pass_rate >= 100.0 {
                format!(
                    "<span class=\"badge badge-pass\">✔ 100% ({}/{})</span>",
                    r.passed_stages, r.total_stages
                )
            } else {
                format!(
                    "<span class=\"badge badge-fail\">✕ {:.0}% ({}/{})</span>",
                    r.pass_rate, r.passed_stages, r.total_stages
                )
            };
            let turns_str = r
                .turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());

            let subruns_badge = if !r.subruns.is_empty() {
                format!(" <span class=\"badge\" style=\"background:rgba(163,113,247,0.15);color:#d2a8ff;font-weight:600;\" title=\"Fair Expected Score: Median TP & Passing Mean Cost scaled by Pass@1\">{} trials (Fair)</span>", r.subruns.len())
            } else {
                String::new()
            };
            let pass_at_k_html = if !r.subruns.is_empty() {
                if let (Some(p1), Some(pk)) = (r.pass_at_1, r.pass_at_k) {
                    format!("<div style=\"font-size:0.72rem;color:var(--grn);font-weight:600;\">Pass@1: {:.0}% · Pass@{}: {:.0}%</div>", p1, r.subruns.len(), pk)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            let mut subruns_html = String::new();
            if !r.subruns.is_empty() {
                subruns_html.push_str(&format!(
                    "<details style=\"margin-top:5px;font-size:0.72rem;color:var(--mut);\"><summary style=\"cursor:pointer;color:var(--acc);font-weight:600;\">View {} subruns breakdown ▾</summary><div style=\"margin-top:4px;padding:6px 8px;background:rgba(0,0,0,0.25);border-radius:4px;border:1px solid rgba(255,255,255,0.06);display:flex;flex-direction:column;gap:4px;\">",
                    r.subruns.len()
                ));
                for (s_idx, sr) in r.subruns.iter().enumerate() {
                    let s_tp = sr
                        .throughput_req_sec
                        .map(|v| format!("{:.0} {}", v, unit))
                        .unwrap_or_else(|| "N/A".to_string());
                    let s_cost = sr.effective_cost_usd.unwrap_or(sr.total_cost_usd);
                    let s_dur = format_duration(sr.duration_seconds.unwrap_or(0.0));
                    let s_turns = sr
                        .turns
                        .map(|t| format!("{}t", t))
                        .unwrap_or_else(|| "-".to_string());
                    subruns_html.push_str(&format!(
                        "<div style=\"display:flex;justify-content:space-between;align-items:center;gap:8px;border-bottom:1px dashed rgba(255,255,255,0.05);padding-bottom:2px;\"><span><strong>Trial {}:</strong> {} · ${:.4} · {} · {}</span><a href=\"runs/{}/\" style=\"color:var(--primary);text-decoration:underline;\">Inspect</a></div>",
                        s_idx + 1, s_tp, s_cost, s_dur, s_turns, Self::esc(&sr.id)
                    ));
                }
                subruns_html.push_str("</div></details>");
            }

            let (m_fam, m_icon, m_color, m_bg, m_border) = Self::get_model_meta(&r.model);
            initial_rows.push_str(&format!(
                "<tr data-href=\"runs/{}/\" class=\"leaderboard-row\" style=\"border-left: 4px solid {}; background: linear-gradient(90deg, {} 0%, rgba(18, 23, 36, 0.6) 12%, transparent 100%);\"><td style=\"text-align:center;\">{}</td>                 <td><div style=\"display:flex;flex-direction:column;gap:3px;\"><div style=\"display:flex;gap:6px;align-items:center;flex-wrap:wrap;\"><div class=\"model-pill\" style=\"background:{};border:1px solid {};\"><span class=\"model-icon\" title=\"{}\">{}</span><a href=\"runs/{}/\" style=\"font-weight:600;color:{};text-decoration:none;\" title=\"{}\">{}</a></div> <span class=\"badge badge-info\">{}</span>{} <span class=\"badge\" style=\"background:rgba(63,185,80,0.15);color:var(--grn);\">{}</span></div><div style=\"font-size:0.72rem;color:var(--mut);\">{}</div>{}</div></td>                 <td><div style=\"display:flex;flex-direction:column;gap:3px;\"><div>{}</div>{}<div><span class=\"badge badge-effort\">effort: {}</span></div></div></td>                 <td><span style=\"font-weight:500;\">{}</span></td>                 <td style=\"text-align:center;\"><span class=\"badge\" style=\"background:rgba(88,166,255,0.15);color:var(--acc);font-weight:600;\">{}</span></td>                 <td><span style=\"font-weight:600;\">{}</span></td>                 <td><div style=\"display:flex;flex-direction:column;gap:2px;\"><div><span style=\"font-size:0.72rem;color:var(--mut);\">API </span><span style=\"font-weight:600;\">${:.4}</span></div><div><span style=\"font-size:0.72rem;color:var(--mut);\">Effective </span><span style=\"font-weight:600;color:var(--gold);\">${:.4}</span></div><span style=\"font-size:0.72rem;color:var(--mut);\">{}</span></div></td>                 <td class=\"score\"><div style=\"font-size:0.95rem;font-weight:700;\">{:.1} <small style=\"font-size:0.72rem;color:var(--mut);\">pts/¢</small></div></td>                 <td><a href=\"runs/{}/\" class=\"badge badge-info\" style=\"text-decoration:none;padding:4px 8px;\">Inspect &rarr;</a></td></tr>\n",
                Self::esc(&r.id),
                m_color,
                m_bg,
                medal,
                m_bg,
                m_border,
                m_fam,
                m_icon,
                Self::esc(&r.id),
                m_color,
                Self::esc(&r.model),
                Self::esc(crate::report::LeaderboardManager::short_model_name(&r.model)),
                Self::esc(&r.task),
                subruns_badge,
                Self::esc(&r.language),
                Self::esc(&r.timestamp),
                subruns_html,
                pass_badge,
                pass_at_k_html,
                Self::esc(r.effort.as_deref().unwrap_or("auto")),
                dur_txt,
                turns_str,
                tp,
                api_c,
                eff_c,
                tok_txt,
                score,
                Self::esc(&r.id),
            ));
        }

        let base_info = if base_url.is_empty() {
            String::new()
        } else {
            format!(" · Base: {}", Self::esc(base_url))
        };

        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">             <title>SubDollarBench Leaderboard Portal</title>             <style>{css}</style></head><body><main>             <div style=\"display:flex;justify-content:space-between;align-items:flex-start;flex-wrap:wrap;gap:12px;margin-bottom:1.2rem;\">               <div>                 <h1>🏆 SubDollarBench Leaderboard Portal</h1>                 <div class=\"sub\">Autonomous under-$1 LLM systems benchmark · Task-specific &amp; consolidated rankings · Strict 100% pass rate requirement · Calibrated reference-adjusted throughput (pts/¢)</div>               </div>               <div style=\"display:flex;flex-direction:column;align-items:flex-end;gap:6px;\">                 <div style=\"display:flex;align-items:center;gap:8px;\">                   <a href=\"/\" style=\"display:inline-flex;align-items:center;gap:5px;padding:6px 12px;border-radius:4px;background:rgba(88,166,255,0.12);color:var(--primary);text-decoration:none;font-weight:600;font-size:0.78rem;border:1px solid rgba(88,166,255,0.25);\">🚀 Benchmark Runner &amp; Console</a>                 </div>                 <div style=\"text-align:right;font-size:0.74rem;color:var(--mut);\">                   <div>Updated {now}{base_info}</div>                   <div style=\"margin-top:4px;\"><span class=\"badge badge-pass\">✔ {passed} Passed</span> <span class=\"badge badge-info\">📊 {n} Total</span></div>                 </div>               </div>             </div>             <div class=\"grid\">               <div class=\"card\"><div class=\"k\">Total Runs</div><div class=\"v\">{n}</div></div>               <div class=\"card\"><div class=\"k\">Passed (100%)</div><div class=\"v\" style=\"color:var(--grn);\">{passed}</div></div>               <div class=\"card\"><div class=\"k\">🌐 HTTP Server</div><div class=\"v\">{http}</div></div>               <div class=\"card\"><div class=\"k\">⚡ Redis Clone</div><div class=\"v\">{redis}</div></div>               <div class=\"card\"><div class=\"k\">📄 PDF Microservice</div><div class=\"v\">{pdf}</div></div><div class=\"card\"><div class=\"k\">📓 WAL Broker</div><div class=\"v\">{wal}</div></div><div class=\"card\"><div class=\"k\">🔀 Reverse Proxy</div><div class=\"v\">{proxy}</div></div><div class=\"card\"><div class=\"k\">📢 Pub/Sub Broker</div><div class=\"v\">{pubsub}</div></div>             </div>             <div class=\"controls-bar\">               <div class=\"pills-group\">                 <button type=\"button\" class=\"btn-toggle active\" data-tier=\"all\" onclick=\"window.__subdollar_tier('all')\" title=\"Compare every row\">All</button>                 <button type=\"button\" class=\"btn-toggle\" data-tier=\"free\" onclick=\"window.__subdollar_tier('free')\" title=\"Free = $0 API spend (compute floor only). Never compare across tiers on cost.\">Free ($0 API)</button>                 <button type=\"button\" class=\"btn-toggle\" data-tier=\"paid\" onclick=\"window.__subdollar_tier('paid')\" title=\"Rows with paid API usage\">Paid</button>                                  <label style=\"font-size:0.75rem;color:var(--mut);display:flex;align-items:center;gap:5px;cursor:pointer;\">                   <input type=\"checkbox\" onchange=\"window.__subdollar_pass_toggle(this.checked)\"> 100% Pass Only                 </label>                 <input type=\"text\" class=\"search-input\" placeholder=\"🔍 Search model or language...\" oninput=\"window.__subdollar_search(this.value)\">               </div>             </div>                          <div id=\"portalDynamicContainer\">               <div style=\"margin-bottom:12px;padding:10px 14px;background:rgba(88,166,255,0.06);border:1px solid rgba(88,166,255,0.2);border-radius:6px;font-size:0.78rem;color:var(--mut);display:flex;align-items:center;gap:8px;\">                 <span>📐</span>                 <span><strong>Scoring Regime:</strong> Throughput scoring is normalized against measured reference baselines on this benchmark host: <strong>HTTP</strong> (Nginx: 33k req/s), <strong>Redis</strong> (Redis: 71k req/s), <strong>DNS</strong> (Reference UDP Resolver: 19k queries/s), <strong>PDF</strong> (Reference Extractor: 4.6k pages/s).</span>               </div>               <div class=\"wrap\">                 <table>                   <thead>                     <tr>                       <th style=\"width:50px;text-align:center;\">#</th>                       <th>Model &amp; Stack</th>                       <th>Conformance &amp; Effort</th>                       <th>Duration</th>                       <th style=\"text-align:center;\">Turns</th>                       <th>Throughput</th>                       <th>Cost &amp; Tokens</th>                       <th>Score pts/&cent;</th>                       <th>Action</th>                     </tr>                   </thead>                   <tbody>{initial_rows}</tbody>                 </table>               </div>             </div>             {methodology}<script>window.__SUBDOLLAR_RUNS__ = {runs_json};</script>             <script>{js}</script>             <footer class=\"muted\">SubDollarBench static portal · Click any row to inspect run logs and source code · Click a table header to sort.</footer>             </main></body></html>",
            css = PORTAL_CSS,
            js = PORTAL_JS,
            n = results.len(),
            passed = passed_n,
            http = http_n,
            redis = redis_n,
            pdf = pdf_n,
            wal = wal_n,
            proxy = proxy_n,
            pubsub = pubsub_n,
            now = now,
            base_info = base_info,
            initial_rows = initial_rows,
            runs_json = runs_json,
            methodology = PORTAL_METHODOLOGY_HTML,
        )
    }

    fn export_run(runs_dir: &Path, runs_out: &Path, run_id: &str, secrets: &[Regex]) -> Result<()> {
        let clean = Path::new(run_id)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(run_id);
        let src = runs_dir.join(clean);
        let dst = runs_out.join(clean);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&dst)?;

        let manifest: Option<RunManifest> = fs::read_to_string(src.join("manifest.json"))
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok());

        for name in [
            "manifest.json",
            "env.json",
            "console.log",
            "README.md",
            "review.json",
        ] {
            if let Ok(content) = fs::read_to_string(src.join(name)) {
                fs::write(dst.join(name), Self::redact(&content, secrets))?;
            }
        }

        Self::copy_workspace(&src.join("workspace"), &dst.join("workspace"), secrets)?;

        let body = match &manifest {
            Some(m) => Self::render_run(m, &dst, secrets)?,
            None => format!("<h1>{}</h1><p>Manifest missing.</p>", Self::esc(clean)),
        };
        fs::write(dst.join("index.html"), body)?;
        Ok(())
    }

    fn render_review(v: &serde_json::Value) -> String {
        fn items(v: &serde_json::Value, key: &str, cls: &str, title: &str) -> String {
            let lis = v
                .get(key)
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .map(|t| format!("<li>{}</li>", PortalExporter::esc(t)))
                        .collect::<String>()
                })
                .unwrap_or_default();
            format!(
                "<div><span class=\"pill {}\">{}</span><ul>{}</ul></div>",
                cls, title, lis
            )
        }
        let concl = v.get("conclusion").and_then(|x| x.as_str()).unwrap_or("");
        format!(
            "<h2>Code review</h2><div class=\"review\">{}{}{}<p><b>Conclusion:</b> {}</p></div>",
            items(v, "good", "pass", "Strengths"),
            items(v, "bad", "fail", "Weaknesses"),
            "",
            PortalExporter::esc(concl)
        )
    }
    fn render_run(m: &RunManifest, dst: &Path, secrets: &[Regex]) -> Result<String> {
        let passed_all = m.pass_rate >= 100.0;
        let mut stages = String::new();
        for s in &m.stages {
            let cls = if s.passed { "pass" } else { "fail" };
            let verdict = if s.passed { "PASS" } else { "FAIL" };
            let err = s.error.as_deref().unwrap_or("");
            stages.push_str(&format!(
                "<div class=\"stage {cls}\"><span class=\"pill {cls}\">{verdict}</span><div><b>{name}</b>{err}</div></div>\n",
                cls = cls,
                verdict = verdict,
                name = Self::esc(&s.name),
                err = if err.is_empty() {
                    String::new()
                } else {
                    format!("<div class=\"muted\">{}</div>", Self::esc(err))
                },
            ));
        }
        let env_rows = match serde_json::to_value(&m.env) {
            Ok(serde_json::Value::Object(map)) => {
                let mut rows: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                rows.sort_by(|a, b| a.0.cmp(b.0));
                rows.iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(x) => x.clone(),
                            _ => v.to_string(),
                        };
                        format!(
                            "<tr><td>{}</td><td>{}</td></tr>\n",
                            Self::esc(k),
                            Self::esc(&val)
                        )
                    })
                    .collect::<String>()
            }
            _ => "<tr><td colspan=\"2\">N/A</td></tr>\n".to_string(),
        };
        let log_html = fs::read_to_string(dst.join("console.log"))
            .map(|c| {
                format!(
                    "<details><summary>console.log (full trace)</summary><pre>{}</pre></details>",
                    Self::esc(&Self::redact(&c, secrets))
                )
            })
            .unwrap_or_default();
        let readme_html = fs::read_to_string(dst.join("README.md"))
            .map(|c| {
                format!(
                    "<details><summary>README.md</summary><pre>{}</pre></details>",
                    Self::esc(&c)
                )
            })
            .unwrap_or_default();
        let review_html = fs::read_to_string(dst.join("review.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|v| Self::render_review(&v))
            .unwrap_or_default();
        let mut file_list = String::new();
        let mut inline_code = String::new();
        Self::render_workspace(&dst.join("workspace"), "", &mut file_list, &mut inline_code)?;

        let api = m.cost_usd;
        let eff = m.effective_cost_usd.unwrap_or(m.cost_usd);
        let comp = (eff - api).max(0.0);
        let score = m.throughput_score.unwrap_or(m.efficiency_score);
        let unit = crate::report::leaderboard::LeaderboardManager::throughput_unit(&m.task);
        let (tp_txt, tp_bar) = match (m.throughput_req_sec, m.reference_throughput_req_sec) {
            (Some(tp), Some(ref_tp)) if ref_tp > 0.0 => {
                let pct = (tp / ref_tp * 100.0).clamp(0.0, 100.0) as u32;
                (
                    format!(
                        "{:.0} {} ({:.0}% of reference)",
                        tp,
                        unit,
                        tp / ref_tp * 100.0
                    ),
                    format!(
                        "<div class=\"bar\"><div class=\"fill\" style=\"width:{}%\"></div></div>",
                        pct
                    ),
                )
            }
            (Some(tp), _) => (format!("{:.0} {}", tp, unit), String::new()),
            _ => ("N/A".to_string(), String::new()),
        };
        let total_tok = m.tokens.total_tokens;
        let trials_pill = match m.total_trials {
            Some(n) if n > 1 => {
                let p1 = m
                    .pass_at_1
                    .map(|v| format!(" · P@1 {v:.0}%"))
                    .unwrap_or_default();
                format!("<span class=\"pill info\">{n} trials{p1}</span> ")
            }
            _ => String::new(),
        };
        let stab_pill = match m.stability.as_deref() {
            Some("stable") => "<span class=\"pill pass\">🟢 stable</span> ".to_string(),
            Some("watch") => "<span class=\"pill info\">🟡 watch</span> ".to_string(),
            Some("unstable") => "<span class=\"pill fail\">🔴 unstable</span> ".to_string(),
            _ => String::new(),
        };
        let tott_str = format!("{:.1}k", total_tok as f64 / 1000.0);
        Ok(format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>{id}</title>\
             <style>{css}</style></head><body><main>\
             <p><a href=\"../../\">&larr; leaderboard</a></p>\
             <div class=\"hero\"><div class=\"muted\">{id}</div><h1>{model}</h1>\
             <div class=\"pills\"><span class=\"pill info\">{task}</span> \
             <span class=\"pill {scls}\">{status}</span> \
             <span class=\"pill {pcls}\">{pass:.1}% ({passed}/{total})</span> \
             <span class=\"pill info\">{lang}</span>{trialspill}{stabpill}</div></div>\
             <div class=\"grid\">\
             <div class=\"card\"><div class=\"k\">Score</div><div class=\"v score\">{score:.1} <small>pts/&cent;</small></div></div>\
             <div class=\"card\"><div class=\"k\">Duration</div><div class=\"v\">{dur}</div><div class=\"muted\">{turns} turns</div></div>\
             <div class=\"card\"><div class=\"k\">Cost</div><div class=\"v\">${eff:.4}</div><div class=\"muted\">API ${api:.4} + compute ${comp:.4}</div></div>\
             <div class=\"card\"><div class=\"k\">Tokens</div><div class=\"v\">{tott}</div><div class=\"muted\">{pt} prompt / {ct} cached / {xt} completion</div></div>\
             <div class=\"card wide\"><div class=\"k\">Throughput</div><div class=\"v\">{tp}</div>{tpbar}</div>\
             </div>\
             <h2>Stages</h2><div class=\"stages\">{stages}</div>\
             {review}<h2>Code</h2>{code}\
             <h2>All files</h2><ul>{files}</ul>\
             {readme}\
             <details><summary>Environment</summary><table><tr><th>Key</th><th>Value</th></tr>{env}</table></details>\
             {log}\
             <footer class=\"muted\">SubDollarBench run archive</footer></main></body></html>",
            css = PORTAL_CSS,
            id = Self::esc(&m.run_id),
            model = Self::esc(&m.model),
            task = Self::esc(&m.task),
            status = Self::esc(&m.status),
            scls = status_pill_class(&m.status),
            // The pass-rate pill was left binary by 09d63ff, so a degraded run
            // still rendered a red "0.0% (0/6)" next to the neutral status
            // pill - publishing the same harness fault by the back door. A
            // non-result's stages were never meaningfully measured, so it gets
            // the neutral pill here too.
            pcls = if is_non_result(&m.status) {
                "info"
            } else if passed_all {
                "pass"
            } else {
                "fail"
            },
            pass = m.pass_rate,
            passed = m.passed_stages,
            total = m.total_stages,
            lang = Self::esc(&m.language),
            trialspill = trials_pill,
            stabpill = stab_pill,
            score = score,
            dur = format_duration(m.duration_seconds),
            turns = m.turns.map(|t| t.to_string()).unwrap_or_else(|| "N/A".to_string()),
            eff = eff,
            api = api,
            comp = comp,
            tott = tott_str,
            pt = m.tokens.prompt_tokens,
            ct = m.tokens.cached_tokens,
            xt = m.tokens.completion_tokens,
            tp = tp_txt,
            tpbar = tp_bar,
            stages = stages,
            code = inline_code,
            files = file_list,
            readme = readme_html,
            review = review_html,
            env = env_rows,
            log = log_html,
        ))
    }

    fn render_workspace(
        dir: &Path,
        prefix: &str,
        list: &mut String,
        inline: &mut String,
    ) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        entries.sort_by_key(|a| a.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
            {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            if path.is_dir() {
                let mut sub_list = String::new();
                let mut sub_inline = String::new();
                Self::render_workspace(&path, &rel, &mut sub_list, &mut sub_inline)?;
                list.push_str(&format!(
                    "<li>{}/<ul>{}</ul></li>\n",
                    Self::esc(&name),
                    sub_list
                ));
                inline.push_str(&sub_inline);
            } else if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                list.push_str(&format!(
                    "<li><a href=\"workspace/{}\">{} ({} bytes)</a></li>\n",
                    Self::esc(&rel),
                    Self::esc(&name),
                    size
                ));
                if size <= 100 * 1024 {
                    if let Ok(content) = fs::read_to_string(&path) {
                        inline.push_str(&format!(
                            "<details><summary><code>{}</code> <span class=\"muted\">({} bytes)</span></summary><pre>{}</pre></details>\n",
                            Self::esc(&rel),
                            size,
                            Self::esc(&content)
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn copy_workspace(src: &Path, dst: &Path, secrets: &[Regex]) -> Result<()> {
        let entries = fs::read_dir(src);
        if entries.is_err() {
            return Ok(());
        }
        fs::create_dir_all(dst)?;
        for entry in entries.unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
            {
                continue;
            }
            if path.is_dir() {
                Self::copy_workspace(&path, &dst.join(&name), secrets)?;
            } else if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size <= 512 * 1024 {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let _ = fs::write(dst.join(&name), Self::redact(&content, secrets));
                    } else {
                        let _ = fs::copy(&path, dst.join(&name));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a real model verdict may render red. An `infra_error` run is a
    /// harness fault; publishing it as a failure is the exact confusion the
    /// status exists to prevent. Same for a dead or killed agent.
    #[test]
    fn test_status_pill_distinguishes_verdicts_from_harness_faults() {
        assert_eq!(status_pill_class("completed"), "pass");
        assert_eq!(status_pill_class("failed_tests"), "fail");
        assert_eq!(status_pill_class("disqualified_cheat"), "fail");

        assert_eq!(
            status_pill_class("infra_error"),
            "info",
            "a harness fault must not be published as a model failure"
        );
        assert_eq!(status_pill_class("agent_failed"), "info");
        assert_eq!(status_pill_class("agent_killed"), "info");
        // Unknown statuses are neutral, never red by default.
        assert_eq!(status_pill_class("something_new"), "info");
    }

    /// Both pills on a run row must agree. 09d63ff fixed the status pill and
    /// left the pass-rate pill binary, so a degraded run still published a red
    /// "0.0% (0/6)" next to the neutral status pill.
    #[test]
    fn test_non_result_runs_are_not_rendered_as_failures() {
        assert!(!is_non_result("completed"));
        assert!(!is_non_result("failed_tests"));
        assert!(!is_non_result("disqualified_cheat"));

        for s in [
            "infra_error",
            "agent_failed",
            "agent_killed",
            "future_status",
        ] {
            assert!(is_non_result(s), "{s} must be treated as a non-result");
            assert_eq!(status_pill_class(s), "info", "{s} pill must be neutral");
        }
    }

    #[test]
    fn test_portal_redaction_strips_secrets() {
        let patterns = PortalExporter::secret_patterns();
        let dirty = "export OPENROUTER_API_KEY=sk-or-v1-abc123XYZ here\nAuthorization: Bearer tok456\nrun --api-key sk-or-v1-other ok\nOPENAI_API_KEY=sk-proj-xyz987\nhf_token=hf_abcdef12345678901234\nsk-ant-secret";
        let clean = PortalExporter::redact(dirty, &patterns);
        assert!(!clean.contains("abc123XYZ"));
        assert!(!clean.contains("tok456"));
        assert!(!clean.contains("sk-or-v1-other"));
        assert!(!clean.contains("sk-proj-xyz987"));
        assert!(!clean.contains("hf_abcdef12345678901234"));
        assert!(!clean.contains("sk-ant-secret"));
        assert!(clean.contains("***REDACTED***"));
        // Ordinary text survives redaction.
        assert!(clean.contains("export"));
    }

    #[test]
    fn test_portal_redaction_generic_keys_and_env_dumps() {
        let patterns = PortalExporter::secret_patterns();
        let dirty = "config api_key=yyy123secret token: tok_abc456 here\nMY_CUSTOM_KEY=supersecretvalue ok\nplain sk-abcdefgh12345678 leaked";
        let clean = PortalExporter::redact(dirty, &patterns);
        assert!(!clean.contains("yyy123secret"));
        assert!(!clean.contains("tok_abc456"));
        assert!(!clean.contains("supersecretvalue"));
        assert!(!clean.contains("sk-abcdefgh12345678"));
        assert!(clean.contains("***REDACTED***"));
        // Ordinary prose survives redaction.
        assert!(clean.contains("config"));
        assert!(clean.contains("here"));
    }

    #[test]
    fn test_portal_export_skips_hidden_runs() {
        let base = std::env::temp_dir().join(format!("test_portal_hide_{}", std::process::id()));
        let runs_dir = base.join("runs");
        let results_dir = base.join("results");
        let out_dir = base.join("portal");
        let run_dir = runs_dir.join("hidden_run_1");
        fs::create_dir_all(run_dir.join("workspace")).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::json!({
                "run_id": "hidden_run_1", "model": "m", "task": "http", "status": "completed",
                "language": "Python", "started_at": "t", "completed_at": "t",
                "duration_seconds": 10.0, "pass_rate": 100.0, "passed_stages": 5, "total_stages": 5,
                "stages": [], "tokens": {"prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1, "total_tokens": 2},
                "cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 50.0, "files": []
            })
            .to_string(),
        )
        .unwrap();
        fs::create_dir_all(&results_dir).unwrap();
        fs::write(
            results_dir.join("hidden_run_1.json"),
            serde_json::json!({
                "id": "hidden_run_1", "model": "m", "task": "http", "language": "Python",
                "pass_rate": 100.0, "passed_stages": 5, "total_stages": 5,
                "prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1, "total_tokens": 2,
                "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 50.0,
                "timestamp": "t"
            })
            .to_string(),
        )
        .unwrap();

        assert!(set_hidden(&runs_dir, "hidden_run_1", true).unwrap());
        assert!(is_hidden(&runs_dir, "hidden_run_1"));
        let summary = PortalExporter::export(
            runs_dir.to_str().unwrap(),
            results_dir.to_str().unwrap(),
            out_dir.to_str().unwrap(),
            "",
            200,
        )
        .unwrap();
        assert_eq!(summary.run_count, 0);
        let index = fs::read_to_string(out_dir.join("index.html")).unwrap();
        assert!(!index.contains("hidden_run_1"));
        assert!(!out_dir.join("runs/hidden_run_1").exists());

        let _ = fs::remove_dir_all(&base);
    }
    #[test]
    fn test_portal_export_writes_index_and_run_page() {
        let base = std::env::temp_dir().join(format!("test_portal_{}", std::process::id()));
        let runs_dir = base.join("runs");
        let results_dir = base.join("results");
        let out_dir = base.join("portal");
        let run_dir = runs_dir.join("demo_run_1");
        fs::create_dir_all(run_dir.join("workspace")).unwrap();
        fs::write(run_dir.join("workspace/server.py"), "print(1)").unwrap();
        fs::write(run_dir.join("console.log"), "token sk-or-v1-zzz done").unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::json!({
                "run_id": "demo_run_1", "model": "m", "task": "http", "status": "completed",
                "language": "Python", "started_at": "t", "completed_at": "t",
                "duration_seconds": 10.0, "pass_rate": 100.0, "passed_stages": 5, "total_stages": 5,
                "stages": [], "tokens": {"prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1, "total_tokens": 2},
                "cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 50.0, "files": []
            })
            .to_string(),
        )
        .unwrap();
        fs::create_dir_all(&results_dir).unwrap();
        fs::write(
            results_dir.join("demo_run_1.json"),
            serde_json::json!({
                "id": "demo_run_1", "model": "m", "task": "http", "language": "Python",
                "total_trials": 1,
                "pass_rate": 100.0, "passed_stages": 5, "total_stages": 5,
                "prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1, "total_tokens": 2,
                "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 50.0,
                "timestamp": "t"
            })
            .to_string(),
        )
        .unwrap();

        fs::write(
            run_dir.join("review.json"),
            "{\"good\": [\"g1\"], \"bad\": [\"b1\"], \"conclusion\": \"done well\"}",
        )
        .unwrap();
        let summary = PortalExporter::export(
            runs_dir.to_str().unwrap(),
            results_dir.to_str().unwrap(),
            out_dir.to_str().unwrap(),
            "",
            200,
        )
        .unwrap();
        assert_eq!(summary.run_count, 1);
        let index = fs::read_to_string(out_dir.join("index.html")).unwrap();
        assert!(index.contains("demo_run_1"));
        assert!(index.contains("<style>:root{color-scheme:dark;--bg:"));
        assert!(!index.contains("{{"));
        let run_page = fs::read_to_string(out_dir.join("runs/demo_run_1/index.html")).unwrap();
        assert!(run_page.contains("demo_run_1"));
        assert!(run_page.contains("Code review"));
        assert!(run_page.contains("done well"));
        let log = fs::read_to_string(out_dir.join("runs/demo_run_1/console.log")).unwrap();
        assert!(!log.contains("sk-or-v1-zzz"));

        let _ = fs::remove_dir_all(&base);
    }

    // Test fixture builder: 15 call sites read better as flat positional args
    // than as a struct literal, and the params are all trivially typed.
    #[allow(clippy::too_many_arguments)]
    fn rollup_parent(
        id: &str,
        model: &str,
        task: &str,
        pass: f64,
        cost: f64,
        effective: Option<f64>,
        trials: u32,
        stab: Option<&str>,
    ) -> crate::report::leaderboard::BenchmarkRunResult {
        crate::report::leaderboard::BenchmarkRunResult {
            id: id.to_string(),
            model: model.to_string(),
            task: task.to_string(),
            language: "Python".to_string(),
            effort: Some("high".to_string()),
            effort_effective: None,
            status: None,
            harness: None,
            pass_rate: pass,
            passed_stages: 5,
            total_stages: 5,
            throughput_req_sec: None,
            reference_throughput_req_sec: None,
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            total_cost_usd: cost,
            cost_source: None,
            cost_formula_usd: None,
            effective_cost_usd: effective,
            duration_seconds: None,
            turns: None,
            savings_percent: 0.0,
            efficiency_score: 0.0,
            throughput_score: None,
            timestamp: "2026-09-26T16:00:00".to_string(),
            method_version: None,
            rules_version: None,
            graded_under: None,
            subruns: vec![],
            total_trials: Some(trials),
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: Some(pass),
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: stab.map(|s| s.to_string()),
            frontier_fraction: None,
            protocol: None,
            solved: Some(pass >= 100.0),
            solved_at: if pass >= 100.0 { Some(1) } else { None },
            best_stage_score: Some(pass),
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        }
    }

    #[test]
    fn test_rollup_groups_by_model_across_tasks() {
        let parents = vec![
            rollup_parent(
                "roll_a1",
                "model-a",
                "http",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
            // effective cost absent -> falls back to total_cost_usd.
            rollup_parent(
                "roll_a2",
                "model-a",
                "redis",
                50.0,
                0.20,
                None,
                3,
                Some("watch"),
            ),
            rollup_parent(
                "roll_b1",
                "model-b",
                "http",
                100.0,
                0.05,
                Some(0.05),
                1,
                None,
            ),
        ];
        let rows = PortalExporter::rollup_models(&parents);
        assert_eq!(rows.len(), 2);
        // Sorted by avg pass desc: model-b (100) before model-a (75).
        assert_eq!(rows[0].model, "model-b");
        assert_eq!(rows[1].model, "model-a");
        let a = &rows[1];
        assert_eq!(a.n_tasks, 2);
        assert_eq!(a.tasks, vec!["http".to_string(), "redis".to_string()]);
        assert_eq!(a.n_trials, 6);
        assert_eq!(a.n_parents, 2);
        assert!((a.avg_pass_rate - 75.0).abs() < 1e-9);
        assert!((a.total_cost_usd - 0.30).abs() < 1e-9);
        assert!((a.avg_cost_usd - 0.15).abs() < 1e-9);
        // Dual cost columns: API-only sums tracked separately from effective.
        // model-a: API 0.10 + 0.20 = 0.30; effective 0.10 + 0.20 (fallback) = 0.30.
        assert!((a.total_api_cost_usd - 0.30).abs() < 1e-9);
        assert!((a.avg_api_cost_usd - 0.15).abs() < 1e-9);
        // Pooled P@1: 3 passing of 6 trials; Wilson interval brackets it.
        assert!((a.p1 - 50.0).abs() < 1e-9);
        assert!(a.p1_lo <= a.p1 && a.p1 <= a.p1_hi);
        assert!(a.p1_hi - a.p1_lo > 1.0);
        // Stability is worst-case across the model's parents.
        assert_eq!(a.stability.as_deref(), Some("watch"));

        let html = PortalExporter::render_rollup_index(&rows, "");
        assert!(html.contains("model-a"));
        assert!(html.contains("model-b"));
        assert!(html.contains(">75<"));
        assert!(html.contains("P@1 with 95% CI"));
        assert!(html.contains("Cost (API total / per run)"));
        assert!(html.contains("tdot"));
        assert!(html.contains("cibar"));
        assert!(html.contains("Methodology"));
        assert!(html.contains("<style>:root{color-scheme:dark;--bg:"));
        assert!(!html.contains("{{"));
        // Zero per-run detail: no run ids, no per-run links.
        for id in ["roll_a1", "roll_a2", "roll_b1"] {
            assert!(!html.contains(id), "run id {id} leaked into rollup");
        }
        assert!(!html.contains("runs/"));
    }

    /// The sdb-sql board: ranked by difficulty-weighted points, cells from
    /// a different RULES_VERSION grouped below the current ones, flagged
    /// (poisoned/hidden) cells and retired campaigns never published, and
    /// the live cell badged.
    /// R28-1 acceptance: the portal shows the recorded oracle and cap
    /// basis for a run, and an old run shows neither today's oracle nor a
    /// list-price label.
    #[test]
    fn r28_r1_portal_uses_recorded_oracle_and_cap() {
        let key = || crate::campaign::BoardSortKey {
            live: false,
            current_rules: true,
            points: 1.0,
            per_dollar: Some(10.0),
            cost: 0.1,
        };
        let cell = |sqlite: Option<&str>, cap: Option<&str>, rules: &str| SqlBoardCell {
            campaign_id: "campaign_sdb-sql_r28".into(),
            model: "m".into(),
            harness: "omp".into(),
            effort: "high".into(),
            state: "done".into(),
            reached: Some(0),
            regression_free: 1,
            points: 1.0,
            max_points: 76.0,
            rules: rules.into(),
            sqlite_version: sqlite.map(str::to_string),
            cap_price_basis: cap.map(str::to_string),
            cost: 0.1,
            over_budget: false,
            cap_reached: false,
            over_wall: false,
            updated: "2026-10-23T00:00:00Z".into(),
            ladder: Vec::new(),
            key: key(),
        };
        let recorded = PortalExporter::render_sql_board(&[cell(
            Some("3.0.0"),
            Some("recorded cap basis"),
            "2026-10-19.40",
        )]);
        assert!(recorded.contains("SQLite 3.0.0"), "{recorded}");
        assert!(recorded.contains("cap at list price"), "{recorded}");

        let old = PortalExporter::render_sql_board(&[cell(None, None, "2026-10-19.38")]);
        assert!(!old.contains("SQLite 3.0.0"), "{old}");
        assert!(old.contains("SQLite —"), "{old}");
        assert!(!old.contains("· cap at list price"), "{old}");
    }

    /// R28 F5/F1: the public board names the cap basis and the SQLite
    /// oracle next to the rules stamp.
    #[test]
    fn r28_f5_portal_shows_cap_basis_and_oracle() {
        let html = PortalExporter::render_sql_board(&[]);
        assert!(html.contains("at list price"), "missing cap basis");
        assert!(html.contains("model_prices.json"), "missing price source");
        assert!(
            html.contains(&format!(
                "SQLite {}",
                crate::report::rules::sqlite_version()
            )),
            "missing SQLite oracle version"
        );
    }

    #[test]
    fn test_sql_board_ranks_points_and_groups_rules() {
        use crate::report::rules::RULES_VERSION;
        let dir = std::env::temp_dir().join(format!("sql_board_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let runs = dir.join("runs");
        let out = dir.join("portal");
        #[allow(clippy::too_many_arguments)]
        let cell = |name: &str,
                    model: &str,
                    reached: Option<u32>,
                    rf: u32,
                    points: f64,
                    cost: f64,
                    rules: &str,
                    state: &str,
                    run_id: &str| {
            let d = runs.join(name);
            fs::create_dir_all(&d).unwrap();
            let campaign = name.split('_').nth(1).unwrap_or("sdb-sql");
            let rollup = serde_json::json!({
                "campaign_id": name,
                "name": campaign,
                "model": model,
                "harness": "omp",
                "effort": "high",
                "task": "sql",
                "started_at": "2026-10-17T09:00:00Z",
                "finished_at": "2026-10-17T10:00:00Z",
                "stages": [
                    {
                        "id": 0, "title": "t0", "type": "genesis", "passed": true,
                        "pass_rate": 100.0, "passed_stages": 1, "total_stages": 1,
                        "regressed": [], "regression_severity": 0,
                        "effective_cost_usd": 0.01, "status": "passed", "run_id": run_id
                    },
                    {
                        "id": 1, "title": "t1", "type": "feature", "passed": false,
                        "pass_rate": 0.0, "passed_stages": 0, "total_stages": 1,
                        "regressed": [], "regression_severity": 0,
                        "effective_cost_usd": 0.01, "status": "failed", "run_id": run_id
                    }
                ],
                "reached": reached,
                "regression_free_tickets": rf,
                "points": points,
                "max_points": 36.0,
                "rules_version": rules,
                "total_cost_usd": cost,
                "over_budget": false,
                "over_wall": false
            });
            fs::write(
                d.join("status.json"),
                serde_json::json!({
                    "state": state,
                    "current_stage": 1,
                    "rollup": rollup,
                    // A running cell is only live while its writer is fresh
                    // (round-12 P2; the exporter derives the console's
                    // effective state).
                    "updated_at": chrono::Utc::now().to_rfc3339(),
                })
                .to_string(),
            )
            .unwrap();
            if state == "done" {
                fs::write(d.join("campaign.json"), rollup.to_string()).unwrap();
            }
        };
        cell(
            "campaign_sdb-sql_m1_20261017",
            "m1",
            Some(5),
            3,
            10.0,
            0.30,
            RULES_VERSION,
            "done",
            "run_m1",
        );
        cell(
            "campaign_sdb-sql_m2_20261017",
            "m2",
            Some(5),
            2,
            12.0,
            0.05,
            RULES_VERSION,
            "done",
            "run_m2",
        );
        cell(
            "campaign_sdb-sql_m3_20261017",
            "m3",
            Some(6),
            1,
            20.0,
            0.90,
            "2026-10-16.12",
            "done",
            "run_m3",
        );
        cell(
            "campaign_sdb-sql_m4_20261017",
            "m4",
            Some(7),
            4,
            30.0,
            0.50,
            RULES_VERSION,
            "done",
            "run_m4_poisoned",
        );
        cell(
            "campaign_sdb-sql_m5_20261017",
            "m5",
            None,
            0,
            0.0,
            0.02,
            RULES_VERSION,
            "running",
            "run_m5",
        );
        // A second run of m2 (round-12 P3: repeat runs collapse into the
        // best row with a `×N runs` disclosure).
        cell(
            "campaign_sdb-sql_m2b_20261017",
            "m2",
            Some(4),
            2,
            8.0,
            0.04,
            RULES_VERSION,
            "done",
            "run_m2b",
        );
        cell(
            "campaign_sdb-kv_zzretired_20261017",
            "zz_retired_model",
            Some(9),
            9,
            99.0,
            0.01,
            RULES_VERSION,
            "done",
            "run_retired",
        );
        // m4's stage run was killed by infrastructure: never a measurement.
        fs::create_dir_all(runs.join("run_m4_poisoned")).unwrap();
        fs::write(runs.join("run_m4_poisoned").join(".poisoned"), "").unwrap();
        // Q5: phase cells stay off the public board until careers have their
        // own ranked row.
        let phase_dir = runs.join("campaign_sdb-sql_phase_hidden");
        fs::create_dir_all(&phase_dir).unwrap();
        let phase_rollup = serde_json::json!({
            "campaign_id": "campaign_sdb-sql_phase_hidden",
            "name": "sdb-sql",
            "model": "phase_model",
            "harness": "omp",
            "effort": "high",
            "task": "sql",
            "started_at": "2026-10-21T00:00:00Z",
            "finished_at": "2026-10-21T01:00:00Z",
            "stages": [],
            "phase": 1,
            "career": {"points": 10.0, "total_cost_usd": 0.5, "cells": ["campaign_sdb-sql_phase_hidden"]},
            "points": 10.0,
            "max_points": 10.0,
            "rules_version": RULES_VERSION,
            "total_cost_usd": 0.5,
            "over_budget": false,
            "over_wall": false
        });
        fs::write(phase_dir.join("campaign.json"), phase_rollup.to_string()).unwrap();
        fs::write(
            phase_dir.join("status.json"),
            serde_json::json!({"state": "done", "rollup": phase_rollup}).to_string(),
        )
        .unwrap();

        let summary =
            PortalExporter::export_sql_board(runs.to_str().unwrap(), out.to_str().unwrap())
                .expect("sql board");
        assert_eq!(
            summary.run_count, 5,
            "poisoned, retired and phase cells never publish"
        );
        let html = fs::read_to_string(out.join("index.html")).unwrap();
        let pos = |m: &str| {
            html.find(m)
                .unwrap_or_else(|| panic!("missing {m}: {html}"))
        };
        // Current rules first: m2 (12 points), m1 (10), the running m5 (0);
        // the stale 20-point m3 groups below them.
        assert!(pos("m2") < pos("m1"), "points order: {html}");
        assert!(pos("m1") < pos("m5"), "current before stale: {html}");
        assert!(pos("m5") < pos("m3"), "stale rules grouped last: {html}");
        assert!(!html.contains("m4"), "poisoned cell leaked: {html}");
        assert!(
            !html.contains("zz_retired_model"),
            "retired campaign leaked: {html}"
        );
        assert!(html.contains("stale 2026-10-16.12"), "{html}");
        assert!(html.contains("RUNNING"), "the live cell is badged: {html}");
        // Round-12 P3: the console's strip, legend, `×N runs` and the
        // older-rules divider.
        assert!(html.contains("class=\"sq "), "labelled squares: {html}");
        assert!(html.contains("class=\"legend\""), "one legend: {html}");
        assert!(html.contains("class=\"strip\""), "{html}");
        assert!(html.contains("×2 runs"), "repeat runs collapse: {html}");
        assert!(
            pos("m2") < pos("m2b") && pos("m2b") < pos("m1"),
            "the duplicate run folds under its best: {html}"
        );
        assert!(
            html.contains("median 10.0 pts across 2 runs"),
            "an even count uses the mean of the two middles: {html}"
        );
        assert!(
            pos("older rule sets") < pos("m3"),
            "the stale cell sits under the divider: {html}"
        );
        assert!(
            pos("m5") < pos("older rule sets"),
            "current rules before the divider: {html}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Round-13 B1/B2: runs under incomparable rule sets never share a row,
    /// unknown rules count as not current, and a poisoned cell never
    /// publishes at all.
    #[test]
    fn test_sql_board_separates_runs_by_rules_version() {
        use crate::report::rules::RULES_VERSION;
        let dir = std::env::temp_dir().join(format!("sql_board_rules_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let runs = dir.join("runs");
        let out = dir.join("portal");
        let cell = |name: &str, model: &str, points: f64, rules: &str| {
            let d = runs.join(name);
            fs::create_dir_all(&d).unwrap();
            let rollup = serde_json::json!({
                "campaign_id": name,
                "name": "sdb-sql",
                "model": model,
                "harness": "omp",
                "effort": "high",
                "task": "sql",
                "started_at": "2026-10-18T00:00:00Z",
                "finished_at": "2026-10-18T00:30:00Z",
                "stages": [],
                "reached": 1,
                "regression_free_tickets": 1,
                "points": points,
                "max_points": 36.0,
                "rules_version": rules,
                "total_cost_usd": 0.10,
                "over_budget": false,
                "over_wall": false
            });
            fs::write(d.join("campaign.json"), rollup.to_string()).unwrap();
        };
        cell("campaign_sdb-sql_a_20261018", "m", 12.0, RULES_VERSION);
        cell("campaign_sdb-sql_b_20261018", "m", 9.0, "2026-10-16.12");
        cell("campaign_sdb-sql_c_20261018", "m", 8.0, "2026-10-16.12");
        cell("campaign_sdb-sql_d_20261018", "m", 7.0, "");
        cell(
            "campaign_sdb-sql_e_20261018",
            "poisoned-model",
            99.0,
            RULES_VERSION,
        );
        fs::write(
            runs.join("campaign_sdb-sql_e_20261018").join(".poisoned"),
            "",
        )
        .unwrap();
        PortalExporter::export_sql_board(runs.to_str().unwrap(), out.to_str().unwrap())
            .expect("sql board");
        let html = fs::read_to_string(out.join("index.html")).unwrap();
        let pos = |m: &str| {
            html.find(m)
                .unwrap_or_else(|| panic!("missing {m}: {html}"))
        };
        // A poisoned cell never publishes.
        assert!(!html.contains("poisoned-model"), "{html}");
        // The current-rules run sits above the divider; the stale and the
        // unknown-rules runs sit below it (the head rows carry their points).
        assert!(pos("12/36") < pos("older rule sets"), "{html}");
        assert!(pos("older rule sets") < pos("9/36"), "{html}");
        assert!(pos("older rule sets") < pos("7/36"), "{html}");
        // Same model, same stale rules: the second run folds into the first.
        assert!(html.contains("×2 runs"), "stale runs fold: {html}");
        assert!(
            !html.contains("×3 runs") && !html.contains("×4 runs"),
            "different rule sets never fold together: {html}"
        );
        // Three head rows: current, the stale pair, and the unknown one.
        assert_eq!(
            html.matches("<b>m</b>").count(),
            3,
            "one row per (model, rules) group: {html}"
        );
        assert!(
            html.contains("campaign_sdb-sql_c"),
            "the folded run is named"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Round-12 P2: the portal derives the console's effective state — a
    /// running cell whose writer is gone is stalled, never RUNNING.
    #[test]
    fn test_sql_board_marks_dead_running_cells_stalled() {
        let dir = std::env::temp_dir().join(format!("sql_board_p2_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let runs = dir.join("runs");
        let out = dir.join("portal");
        let cell = |name: &str, model: &str, state: &str, updated: &str| {
            let d = runs.join(name);
            fs::create_dir_all(&d).unwrap();
            let rollup = serde_json::json!({
                "campaign_id": name,
                "name": "sdb-sql",
                "model": model,
                "harness": "omp",
                "effort": "high",
                "task": "sql",
                "started_at": "2026-10-17T09:00:00Z",
                "finished_at": "2026-10-17T10:00:00Z",
                "stages": [],
                "reached": 1,
                "regression_free_tickets": 1,
                "points": 1.0,
                "max_points": 36.0,
                "rules_version": crate::report::rules::RULES_VERSION,
                "total_cost_usd": 0.10,
                "over_budget": false,
                "over_wall": false
            });
            fs::write(
                d.join("status.json"),
                serde_json::json!({
                    "state": state,
                    "current_stage": 1,
                    "updated_at": updated,
                })
                .to_string(),
            )
            .unwrap();
            fs::write(d.join("campaign.json"), rollup.to_string()).unwrap();
        };
        cell(
            "campaign_sdb-sql_dead_20261017",
            "dead-model",
            "running",
            &(chrono::Utc::now() - chrono::Duration::hours(20)).to_rfc3339(),
        );
        cell(
            "campaign_sdb-sql_live_20261017",
            "live-model",
            "running",
            &chrono::Utc::now().to_rfc3339(),
        );
        PortalExporter::export_sql_board(runs.to_str().unwrap(), out.to_str().unwrap())
            .expect("sql board");
        let html = fs::read_to_string(out.join("index.html")).unwrap();
        let pos = |m: &str| {
            html.find(m)
                .unwrap_or_else(|| panic!("missing {m}: {html}"))
        };
        let row_end = |at: usize| {
            html[at..]
                .find("</tr>")
                .map(|i| at + i + 5)
                .unwrap_or(html.len())
        };
        let dead = pos("dead-model");
        let seg = &html[dead..row_end(dead)];
        assert!(
            seg.contains("stalled"),
            "a dead writer is stalled, not RUNNING: {seg}"
        );
        assert!(
            !seg.contains("RUNNING"),
            "a dead writer must not be RUNNING: {seg}"
        );
        let live = pos("live-model");
        let seg = &html[live..row_end(live)];
        assert!(seg.contains("RUNNING"), "a fresh writer stays live: {seg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_export_rollup_writes_index_only() {
        let base = std::env::temp_dir().join(format!("test_portal_rollup_{}", std::process::id()));
        let results_dir = base.join("results");
        let out_dir = base.join("portal");
        fs::create_dir_all(&results_dir).unwrap();
        // Fixture meets the rollup coverage floor: 6 active tasks x 3 trials = 18.
        let parents = vec![
            rollup_parent(
                "roll_c1",
                "model-c",
                "http",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
            rollup_parent(
                "roll_c2",
                "model-c",
                "wal",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
            rollup_parent(
                "roll_c3",
                "model-c",
                "redis",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
            rollup_parent(
                "roll_c4",
                "model-c",
                "binproto",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
            rollup_parent(
                "roll_c5",
                "model-c",
                "pdf",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
            rollup_parent(
                "roll_c6",
                "model-c",
                "proxy",
                100.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ),
        ];
        for p in &parents {
            fs::write(
                results_dir.join(format!("{}.json", p.id)),
                serde_json::to_string_pretty(p).unwrap(),
            )
            .unwrap();
        }
        let summary = PortalExporter::export_rollup(
            results_dir.to_str().unwrap(),
            out_dir.to_str().unwrap(),
            "",
        )
        .unwrap();
        assert_eq!(summary.run_count, 1);
        let index = fs::read_to_string(out_dir.join("index.html")).unwrap();
        assert!(index.contains("model-c"));
        assert!(!index.contains("roll_c1"));
        assert!(!index.contains("roll_c2"));
        assert!(!index.contains("roll_c6"));
        assert!(!index.contains("runs/"));
        // Rollup mode writes index.html only: no per-run pages, no results.json.
        assert!(!out_dir.join("runs").exists());
        assert!(!out_dir.join("results.json").exists());
        // Methodology section is visible on the rollup page.
        assert!(index.contains("Methodology"));
        assert!(index.contains("Wilson"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_rollup_floor_excludes_thin_coverage() {
        // Thin model: 1 task, 8 trials (like a single-task probe).
        let thin = rollup_parent(
            "thin_1",
            "thin-model",
            "wal",
            100.0,
            0.05,
            Some(0.05),
            8,
            None,
        );
        // Covered model: 6 tasks x 3 trials = 18 trials.
        let mut parents = vec![thin];
        for (i, task) in ["http", "redis", "pdf", "wal", "proxy", "pubsub"]
            .iter()
            .enumerate()
        {
            parents.push(rollup_parent(
                &format!("wide_{i}"),
                "wide-model",
                task,
                80.0,
                0.10,
                Some(0.10),
                3,
                Some("stable"),
            ));
        }
        let rows = PortalExporter::apply_rollup_floor(PortalExporter::rollup_models(&parents));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "wide-model");
        assert_eq!(rows[0].n_tasks, 6);
        assert_eq!(rows[0].n_trials, 18);
    }

    #[test]
    fn test_rollup_skips_test_ids() {
        let probe = rollup_parent(
            "test_probe_9",
            "probe-model",
            "http",
            100.0,
            0.01,
            None,
            3,
            None,
        );
        let real = rollup_parent("real_9", "probe-model", "http", 100.0, 0.01, None, 3, None);
        let rows = PortalExporter::rollup_models(&[probe, real]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "probe-model");
        assert_eq!(rows[0].n_parents, 1);
    }

    #[test]
    fn test_freeze_cutoff() {
        assert!(!publishable("2026-09-26T15:34:09"));
        assert!(publishable("2026-09-26T15:34:10"));
        assert!(publishable("2026-09-26T19:45:16.109579"));
        assert!(!publishable("2026-09-16T14:54:59.418874"));
    }

    /// The portal is a single inline <script>: a typo inside it breaks the
    /// whole page, and nothing in the Rust compile path would notice. These
    /// assertions pin the free/paid board split (slice 1b parity) and, when
    /// node is available, hard-fail on a JS syntax error. The paren typo this
    /// guards against actually shipped once during review.
    #[test]
    fn test_portal_js_has_tier_split_and_parses() {
        assert!(
            PORTAL_JS.contains("window.__subdollar_tier"),
            "portal must expose the tier handler"
        );
        assert!(
            PORTAL_JS.contains("data-tier"),
            "portal markup must carry the tier pills"
        );
        assert!(
            PORTAL_JS.contains("tierOf(r) === boardTier"),
            "both views must filter on the data-derived tier"
        );
        // Layout parity with the GUI: the spread line is display-only and the
        // rank key stays Wilson P@1.
        assert!(
            PORTAL_JS.contains("spread \u{2014} throughput")
                || PORTAL_JS.contains("spread — throughput")
        );

        // Leaf helpers come from shared.js (inlined by concat!), not redefined.
        assert!(PORTAL_JS.contains("function tierOf"));
        assert!(PORTAL_JS.contains("function iqr"));
        assert!(PORTAL_JS.contains("function fmtTp"));

        // Delimiter balance is necessary-but-not-sufficient; node --check is
        // the real parser. Skip (not fail) when node is absent.
        for (open, close) in [("{", "}"), ("(", ")"), ("[", "]")] {
            let o = PORTAL_JS.matches(open).count();
            let c = PORTAL_JS.matches(close).count();
            assert_eq!(o, c, "unbalanced {open}{close} in portal JS");
        }
        let dir = std::env::temp_dir().join(format!("portal_js_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("portal.js");
        std::fs::write(&f, PORTAL_JS).unwrap();
        let out = std::process::Command::new("node")
            .arg("--check")
            .arg(&f)
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        match out {
            Ok(o) => assert!(
                o.status.success(),
                "node --check rejected the portal JS: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(_) => eprintln!("node not found; skipping portal JS parse check"),
        }
    }

    #[test]
    fn test_freeze_filters_rollup_input() {
        let mut old = rollup_parent("old_1", "m", "http", 100.0, 0.01, None, 9, None);
        old.timestamp = "2026-09-16T14:54:59".to_string();
        let mut new = rollup_parent("new_1", "m", "http", 0.0, 0.01, None, 9, None);
        new.timestamp = "2026-09-26T19:45:16.109579".to_string();
        let fresh: Vec<_> = vec![old, new]
            .into_iter()
            .filter(|r| publishable(&r.timestamp))
            .collect();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, "new_1");
    }

    // --- targeted coverage (wave 3) ---------------------------------------

    fn cov_temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cov_portal_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// Minimal runs/<id>/ tree with a parseable manifest and workspace file.
    fn cov_write_run(runs_dir: &Path, id: &str, status: &str, pass: f64) {
        let dir = runs_dir.join(id);
        fs::create_dir_all(dir.join("workspace")).unwrap();
        fs::write(dir.join("workspace/server.go"), "package main").unwrap();
        fs::write(
            dir.join("manifest.json"),
            serde_json::json!({
                "run_id": id, "model": "m", "task": "redis", "status": status,
                "language": "Go", "started_at": "2026-09-26T15:00:00",
                "completed_at": "2026-09-26T15:02:00", "duration_seconds": 10.0,
                "pass_rate": pass, "passed_stages": 2, "total_stages": 4,
                "stages": [], "tokens": {"prompt_tokens": 1, "cached_tokens": 0,
                "completion_tokens": 1, "total_tokens": 2},
                "cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 5.0,
                "files": []
            })
            .to_string(),
        )
        .unwrap();
    }

    /// Minimal results/<id>.json row (post-freeze timestamp by default).
    fn cov_result(id: &str, pass: f64, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "model": "m", "task": "redis", "language": "Go",
            "total_trials": 1,
            "pass_rate": pass, "passed_stages": 2, "total_stages": 4,
            "prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1,
            "total_tokens": 2, "total_cost_usd": 0.01, "savings_percent": 0.0,
            "efficiency_score": 5.0, "timestamp": ts
        })
    }

    fn cov_manifest(status: &str, pass: f64, tp: Option<f64>, ref_tp: Option<f64>) -> RunManifest {
        serde_json::from_value(serde_json::json!({
            "run_id": "cov_run_1", "model": "anthropic/claude-x", "task": "redis",
            "status": status, "language": "Go", "started_at": "2026-09-26T15:00:00",
            "completed_at": "2026-09-26T15:02:00", "duration_seconds": 125.0,
            "turns": 7,
            "pass_rate": pass,
            "passed_stages": if pass >= 100.0 { 4 } else { 2 }, "total_stages": 4,
            "stages": [
                {"stage": 1, "name": "Stage 1: handshake", "passed": true, "error": null},
                {"stage": 2, "name": "Stage 2: kv", "passed": false, "error": "connection refused"}
            ],
            "throughput_req_sec": tp, "reference_throughput_req_sec": ref_tp,
            "tokens": {"prompt_tokens": 100, "cached_tokens": 10,
                       "completion_tokens": 90, "total_tokens": 200},
            "cost_usd": 0.02, "effective_cost_usd": 0.05, "savings_percent": 10.0,
            "efficiency_score": 7.0, "files": [],
            "total_trials": 3, "pass_at_1": 66.7, "stability": "watch"
        }))
        .unwrap()
    }

    #[test]
    fn test_esc_escapes_html_specials() {
        assert_eq!(
            PortalExporter::esc("a<b>&\"c\""),
            "a&lt;b&gt;&amp;&quot;c&quot;"
        );
        assert_eq!(PortalExporter::esc("plain text"), "plain text");
    }

    #[test]
    fn test_hidden_and_poisoned_flag_lifecycle() {
        let base = cov_temp("flags");
        let runs = base.join("runs");
        fs::create_dir_all(&runs).unwrap();
        cov_write_run(&runs, "flag_run", "completed", 100.0);
        // Hidden on/off.
        assert!(!is_hidden(&runs, "flag_run"));
        assert!(set_hidden(&runs, "flag_run", true).unwrap());
        assert!(is_hidden(&runs, "flag_run"));
        assert!(!set_hidden(&runs, "flag_run", false).unwrap());
        assert!(!is_hidden(&runs, "flag_run"));
        // Poisoned on/off, independent of hidden.
        assert!(!is_poisoned(&runs, "flag_run"));
        assert!(set_poisoned(&runs, "flag_run", true).unwrap());
        assert!(is_poisoned(&runs, "flag_run"));
        assert!(!set_poisoned(&runs, "flag_run", false).unwrap());
        assert!(!is_poisoned(&runs, "flag_run"));
        // Invalid and missing ids.
        assert!(is_hidden(&runs, ""));
        assert!(!is_poisoned(&runs, ""));
        assert!(is_hidden(&runs, "."));
        assert!(set_hidden(&runs, "", true).is_err());
        assert!(set_hidden(&runs, "ghost", true).is_err());
        assert!(set_poisoned(&runs, "", true).is_err());
        assert!(set_poisoned(&runs, "ghost", true).is_err());
        // Path-ish ids resolve to their final component.
        fs::create_dir_all(runs.join("flag_run")).unwrap();
        assert!(!is_hidden(&runs, "nested/flag_run"));
        let _ = fs::remove_dir_all(&base);
    }

    /// Round-14 N1: a campaign cell (no run manifest) can be poisoned and
    /// hidden through its own folder, with the reason written down, and the
    /// sql board drops it until it is unflagged.
    #[test]
    fn test_campaign_flag_lifecycle_and_board() {
        let base = cov_temp("campaign_flags");
        let runs = base.join("runs");
        let out = base.join("portal");
        let id = "campaign_sdb-sql_flagme_20261018";
        let d = runs.join(id);
        fs::create_dir_all(&d).unwrap();
        let rollup = serde_json::json!({
            "campaign_id": id, "name": "sdb-sql", "model": "flag-model",
            "harness": "omp", "effort": "high", "task": "sql",
            "started_at": "2026-10-18T00:00:00Z", "finished_at": "2026-10-18T00:30:00Z",
            "stages": [], "reached": 0, "regression_free_tickets": 1, "points": 1.0,
            "max_points": 26.0, "rules_version": crate::report::rules::RULES_VERSION,
            "total_cost_usd": 0.1, "over_budget": false, "over_wall": false
        });
        fs::write(d.join("campaign.json"), rollup.to_string()).unwrap();
        let board = || {
            PortalExporter::export_sql_board(runs.to_str().unwrap(), out.to_str().unwrap())
                .unwrap();
            fs::read_to_string(out.join("index.html")).unwrap()
        };
        assert!(board().contains("flag-model"));
        // The per-run setter still refuses a campaign folder (no manifest).
        assert!(set_poisoned(&runs, id, true).is_err());
        assert!(set_campaign_flag(&runs, id, POISONED_FILE, true, Some("R1: argv crash")).unwrap());
        assert!(is_poisoned(&runs, id));
        let note = fs::read_to_string(d.join(POISONED_FILE)).unwrap();
        assert!(note.contains("reason: R1: argv crash"), "{note}");
        assert!(
            !board().contains("flag-model"),
            "a poisoned cell never publishes"
        );
        assert!(!set_campaign_flag(&runs, id, POISONED_FILE, false, None).unwrap());
        assert!(board().contains("flag-model"), "unflagged, it comes back");
        assert!(set_campaign_flag(&runs, id, PORTAL_HIDDEN_FILE, true, None).unwrap());
        assert!(is_hidden(&runs, id));
        assert!(fs::read_to_string(d.join(PORTAL_HIDDEN_FILE))
            .unwrap()
            .contains("(no reason given)"));
        assert!(!board().contains("flag-model"));
        // Only campaign folders, only the two flags, only existing cells.
        assert!(set_campaign_flag(&runs, "flag_run", POISONED_FILE, true, None).is_err());
        assert!(set_campaign_flag(&runs, id, "manifest.json", true, None).is_err());
        assert!(set_campaign_flag(&runs, "campaign_ghost", POISONED_FILE, true, None).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_get_model_meta_vendors_and_hash_fallback() {
        let cases = [
            ("anthropic/claude-sonnet", "Anthropic"),
            ("openai/o1-mini", "OpenAI"),
            ("google/gemini-3", "Google"),
            ("deepseek/v4", "DeepSeek"),
            ("meta/llama-4", "Meta"),
            ("qwen/qwen-3", "Qwen"),
            ("mistral/pixtral-large", "Mistral"),
            ("minimax/m2", "MiniMax"),
            ("amazon/nova-pro", "Amazon"),
            ("cohere/command-r", "Cohere"),
            ("vllm/local-7b", "Local"),
            ("nobody/mystery-a", "Other"),
            ("nobody/mystery-b", "Other"),
        ];
        for (model, family) in cases {
            assert_eq!(PortalExporter::get_model_meta(model).0, family, "{model}");
        }
        // Unknown-vendor colouring is a deterministic hash, never random.
        assert_eq!(
            PortalExporter::get_model_meta("zzz/unknown"),
            PortalExporter::get_model_meta("zzz/unknown")
        );
    }

    #[test]
    fn test_render_review_tolerates_missing_and_invalid_fields() {
        let v = serde_json::json!({
            "good": ["clear structure"],
            "bad": ["no timeouts"],
            "conclusion": "acceptable",
        });
        let html = PortalExporter::render_review(&v);
        assert!(html.contains("Strengths"));
        assert!(html.contains("clear structure"));
        assert!(html.contains("Weaknesses"));
        assert!(html.contains("no timeouts"));
        assert!(html.contains("Conclusion:</b> acceptable"));
        // Missing keys and non-array values render empty lists, not panics.
        let html = PortalExporter::render_review(&serde_json::json!({
            "good": "not-an-array",
            "bad": [1, 2],
        }));
        assert!(html.contains("Strengths"));
        assert!(html.contains("Conclusion:</b> "));
        assert!(!html.contains("<li>not-an-array</li>"));
    }

    #[test]
    fn test_render_run_full_page_and_non_result_pills() {
        let base = cov_temp("render_run");
        fs::create_dir_all(base.join("workspace")).unwrap();
        fs::write(base.join("workspace/main.go"), "package main").unwrap();
        fs::write(base.join("console.log"), "boot ok token sk-ant-LEAKME done").unwrap();
        fs::write(base.join("README.md"), "# readme").unwrap();
        fs::write(
            base.join("review.json"),
            r#"{"good":["g"],"bad":["b"],"conclusion":"fine"}"#,
        )
        .unwrap();
        let patterns = PortalExporter::secret_patterns();
        let m = cov_manifest("completed", 100.0, Some(5_000.0), Some(10_000.0));
        let html = PortalExporter::render_run(&m, &base, &patterns).unwrap();
        // Multi-trial pill + P@1, stability pill, throughput with reference bar.
        assert!(html.contains("3 trials"));
        assert!(html.contains("P@1 67%"));
        assert!(html.contains("🟡 watch"));
        assert!(html.contains("50% of reference"));
        assert!(html.contains("50%\"") || html.contains("width:50%"));
        assert!(html.contains("2m 5s"));
        assert!(html.contains("7 turns"));
        assert!(html.contains("PASS"));
        assert!(html.contains("FAIL"));
        assert!(html.contains("connection refused"));
        assert!(html.contains("Code review"));
        assert!(html.contains("Conclusion:</b> fine"));
        assert!(html.contains("main.go"));
        assert!(html.contains("&#47;") || html.contains("workspace/main.go"));
        assert!(html.contains("# readme"));
        // Console log is redacted before embedding.
        assert!(!html.contains("sk-ant-LEAKME"));
        assert!(html.contains("***REDACTED***"));
        // API + compute split: effective 0.05, API 0.02 -> compute 0.03.
        assert!(html.contains("compute $0.0300"));

        // Degraded run: both status and pass-rate pills stay neutral, all
        // optional sections vanish, env falls back to the N/A row.
        let empty = cov_temp("render_run_empty");
        fs::create_dir_all(&empty).unwrap();
        let m2 = cov_manifest("infra_error", 0.0, None, None);
        let html2 = PortalExporter::render_run(&m2, &empty, &patterns).unwrap();
        assert!(html2.contains("class=\"pill info\">infra_error"));
        assert!(html2.contains("class=\"pill info\">0.0%"));
        assert!(html2.contains("colspan=\"2\">N/A"));
        assert!(!html2.contains("Code review"));
        assert!(!html2.contains("console.log (full trace)"));
        // Explicitly-missing turn count renders the N/A fallback.
        let mut m2b = cov_manifest("infra_error", 0.0, None, None);
        m2b.turns = None;
        let html2b = PortalExporter::render_run(&m2b, &empty, &patterns).unwrap();
        assert!(html2b.contains("N/A turns"));

        // Real failure keeps the red pass pill; unanchored throughput is
        // reported raw, and zero reference is not a bar.
        let m3 = cov_manifest("failed_tests", 50.0, Some(5_000.0), None);
        let html3 = PortalExporter::render_run(&m3, &empty, &patterns).unwrap();
        assert!(html3.contains("class=\"pill fail\">50.0%"));
        assert!(html3.contains("5000 req/s"));
        assert!(!html3.contains("of reference"));
        let m4 = cov_manifest("failed_tests", 50.0, Some(5_000.0), Some(0.0));
        let html4 = PortalExporter::render_run(&m4, &empty, &patterns).unwrap();
        assert!(html4.contains("5000 req/s"));
        assert!(!html4.contains("of reference"));

        // Stability pill variants.
        let mut m_stable = cov_manifest("completed", 100.0, None, None);
        m_stable.stability = Some("stable".to_string());
        let html_stable = PortalExporter::render_run(&m_stable, &empty, &patterns).unwrap();
        assert!(html_stable.contains("🟢 stable"));
        let mut m_unstable = cov_manifest("completed", 100.0, None, None);
        m_unstable.stability = Some("unstable".to_string());
        let html_unstable = PortalExporter::render_run(&m_unstable, &empty, &patterns).unwrap();
        assert!(html_unstable.contains("🔴 unstable"));

        // Environment table renders sorted key/value rows.
        let mut m_env = cov_manifest("completed", 100.0, None, None);
        m_env.env = Some(crate::report::environment::EnvironmentInfo {
            os: "linux".to_string(),
            kernel: "6.8.0".to_string(),
            arch: "x86_64".to_string(),
            cpu_model: "EPYC-9004".to_string(),
            cpu_cores: 8,
            total_memory: "32GB".to_string(),
            docker_version: "27.0".to_string(),
            rust_version: "1.80".to_string(),
            python_version: "3.12".to_string(),
            node_version: "20".to_string(),
            git_commit: "abc123".to_string(),
            git_branch: "main".to_string(),
            timestamp: "2026-09-26T15:00:00".to_string(),
        });
        let html_env = PortalExporter::render_run(&m_env, &empty, &patterns).unwrap();
        assert!(html_env.contains("EPYC-9004"));
        assert!(html_env.contains("x86_64"));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn test_render_workspace_lists_and_inlines_with_skip_rules() {
        let base = cov_temp("workspace");
        let dir = base.join("workspace");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::create_dir_all(dir.join("__pycache__")).unwrap();
        fs::write(dir.join("app.py"), "print(1)").unwrap();
        fs::write(dir.join("sub/nested.py"), "nested = True").unwrap();
        fs::write(dir.join(".hidden.py"), "hidden").unwrap();
        fs::write(dir.join("target/t.py"), "target").unwrap();
        fs::write(dir.join("node_modules/n.js"), "nm").unwrap();
        fs::write(dir.join("__pycache__/c.py"), "pyc").unwrap();
        fs::write(dir.join("big.txt"), "x".repeat(101 * 1024)).unwrap();
        fs::write(dir.join("blob.bin"), [0xFFu8, 0xFE, 0x00]).unwrap();
        let mut list = String::new();
        let mut inline = String::new();
        PortalExporter::render_workspace(&dir, "", &mut list, &mut inline).unwrap();
        assert!(list.contains("app.py"));
        assert!(list.contains("big.txt"));
        assert!(list.contains("sub/"));
        assert!(list.contains("nested.py"));
        assert!(list.contains("blob.bin"));
        assert!(list.contains("(103424 bytes)"));
        for skipped in [".hidden.py", "target", "node_modules", "__pycache__"] {
            assert!(!list.contains(skipped), "{skipped} leaked into listing");
        }
        assert!(inline.contains("print(1)"));
        assert!(inline.contains("nested = True"));
        // Oversized and non-UTF-8 files are listed, never inlined.
        assert!(!inline.contains("xxxxx"));
        assert!(!inline.contains("blob.bin"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_copy_workspace_redacts_copies_and_skips() {
        let base = cov_temp("copy_ws");
        let src = base.join("src");
        let dst = base.join("dst");
        // Missing source: no-op success, no destination created.
        PortalExporter::copy_workspace(&base.join("missing"), &dst, &[]).unwrap();
        assert!(!dst.exists());
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir_all(src.join("target")).unwrap();
        fs::write(src.join("keep.txt"), "key sk-or-v1-COVSECRET end").unwrap();
        fs::write(src.join("binary.bin"), [0xFFu8, 0x00, 0x01]).unwrap();
        fs::write(src.join("big.bin"), vec![0u8; 600 * 1024]).unwrap();
        fs::write(src.join(".hidden"), "hidden").unwrap();
        fs::write(src.join("target/t.rs"), "t").unwrap();
        fs::write(src.join("sub/deep.txt"), "deep").unwrap();
        PortalExporter::copy_workspace(&src, &dst, &PortalExporter::secret_patterns()).unwrap();
        assert!(fs::read_to_string(dst.join("keep.txt"))
            .unwrap()
            .contains("***REDACTED***"));
        assert_eq!(
            fs::read(dst.join("binary.bin")).unwrap(),
            [0xFFu8, 0x00, 0x01]
        );
        assert!(dst.join("sub/deep.txt").exists());
        assert!(!dst.join("big.bin").exists(), ">512KB skipped");
        assert!(!dst.join(".hidden").exists());
        assert!(!dst.join("target").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_export_run_without_manifest_writes_placeholder() {
        let base = cov_temp("ghost_run");
        let runs = base.join("runs");
        let out = base.join("out");
        fs::create_dir_all(&runs).unwrap();
        PortalExporter::export_run(&runs, &out, "ghost_run", &PortalExporter::secret_patterns())
            .unwrap();
        let page = fs::read_to_string(out.join("ghost_run/index.html")).unwrap();
        assert!(page.contains("Manifest missing."));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_portal_export_prunes_stale_and_exports_subruns() {
        let base = cov_temp("export_prune");
        let runs = base.join("runs");
        let results = base.join("results");
        let out = base.join("portal");
        fs::create_dir_all(&results).unwrap();
        cov_write_run(&runs, "prune_parent", "completed", 100.0);
        cov_write_run(&runs, "prune_sub_1", "completed", 100.0);
        let mut sub = cov_result("prune_sub_1", 100.0, "2026-09-26T19:45:16");
        sub["total_trials"] = serde_json::json!(1);
        let mut parent = cov_result("prune_parent", 100.0, "2026-09-26T19:45:17");
        parent["subruns"] = serde_json::json!([sub]);
        parent["total_trials"] = serde_json::json!(1);
        fs::write(
            results.join("prune_parent.json"),
            serde_json::to_string(&parent).unwrap(),
        )
        .unwrap();
        // Pre-existing stale run page must be pruned.
        fs::create_dir_all(out.join("runs/stale_residue")).unwrap();
        fs::write(out.join("runs/stale_residue/index.html"), "stale").unwrap();

        let summary = PortalExporter::export(
            runs.to_str().unwrap(),
            results.to_str().unwrap(),
            out.to_str().unwrap(),
            "",
            200,
        )
        .unwrap();
        assert_eq!(summary.run_count, 1);
        assert!(out.join("runs/prune_parent/index.html").exists());
        assert!(
            out.join("runs/prune_sub_1/index.html").exists(),
            "subrun pages are exported alongside the parent"
        );
        assert!(!out.join("runs/stale_residue").exists());
        // results.json ships the visible row set.
        let rows: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(out.join("results.json")).unwrap()).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_portal_export_freeze_keep_poison_and_fault_filters() {
        let base = cov_temp("export_filters");
        let runs = base.join("runs");
        let results = base.join("results");
        let out = base.join("portal");
        fs::create_dir_all(&results).unwrap();
        for id in [
            "filt_old",
            "filt_new",
            "filt_poison",
            "filt_fault",
            "filt_top",
        ] {
            cov_write_run(&runs, id, "completed", 100.0);
        }
        // Old timestamp is frozen out.
        fs::write(
            results.join("filt_old.json"),
            serde_json::to_string(&cov_result("filt_old", 10.0, "2026-09-16T14:54:59")).unwrap(),
        )
        .unwrap();
        fs::write(
            results.join("filt_new.json"),
            serde_json::to_string(&cov_result("filt_new", 50.0, "2026-09-26T19:45:16")).unwrap(),
        )
        .unwrap();
        fs::write(
            results.join("filt_poison.json"),
            serde_json::to_string(&cov_result("filt_poison", 90.0, "2026-09-26T19:45:16")).unwrap(),
        )
        .unwrap();
        let mut fault = cov_result("filt_fault", 0.0, "2026-09-26T19:45:16");
        fault["status"] = serde_json::json!("infra_error");
        fs::write(
            results.join("filt_fault.json"),
            serde_json::to_string(&fault).unwrap(),
        )
        .unwrap();
        fs::write(
            results.join("filt_top.json"),
            serde_json::to_string(&cov_result("filt_top", 100.0, "2026-09-26T19:45:16")).unwrap(),
        )
        .unwrap();
        set_poisoned(&runs, "filt_poison", true).unwrap();

        let summary = PortalExporter::export(
            runs.to_str().unwrap(),
            results.to_str().unwrap(),
            out.to_str().unwrap(),
            "",
            200,
        )
        .unwrap();
        assert_eq!(summary.run_count, 2);
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("filt_new"));
        assert!(index.contains("filt_top"));
        for excluded in ["filt_old", "filt_poison", "filt_fault"] {
            assert!(!index.contains(excluded), "{excluded} leaked into index");
            assert!(
                !out.join(format!("runs/{excluded}")).exists(),
                "{excluded} page published"
            );
        }

        // keep=1 truncates the ranked list before export: the top row
        // (filt_top at 100%) is the only page written.
        let out2 = base.join("portal_keep");
        let summary = PortalExporter::export(
            runs.to_str().unwrap(),
            results.to_str().unwrap(),
            out2.to_str().unwrap(),
            "",
            1,
        )
        .unwrap();
        assert_eq!(summary.run_count, 1);
        assert!(out2.join("runs/filt_top").exists());
        assert!(!out2.join("runs/filt_new").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_render_index_initial_rows_and_subrun_details() {
        let mut parent = rollup_parent(
            "idx_parent_1",
            "anthropic/claude-x",
            "redis",
            66.7,
            0.02,
            Some(0.05),
            2,
            Some("watch"),
        );
        parent.pass_rate = 66.7;
        parent.passed_stages = 2;
        parent.pass_at_1 = Some(50.0);
        parent.pass_at_k = Some(100.0);
        parent.throughput_req_sec = Some(5_000.0);
        let mut sub = rollup_parent(
            "idx_sub_1",
            "anthropic/claude-x",
            "redis",
            50.0,
            0.01,
            None,
            1,
            None,
        );
        sub.throughput_req_sec = None;
        sub.turns = None;
        sub.duration_seconds = None;
        parent.subruns = vec![sub];
        let simple = rollup_parent(
            "idx_simple",
            "openai/gpt-5",
            "http",
            100.0,
            0.01,
            None,
            1,
            None,
        );

        let html = PortalExporter::render_index(&[parent, simple], "https://pub.example");
        assert!(html.contains("idx_parent_1"));
        assert!(html.contains("idx_simple"));
        assert!(html.contains("1 trials (Fair)"));
        assert!(html.contains("Pass@1: 50%"));
        assert!(html.contains("View 1 subruns breakdown"));
        assert!(html.contains("Trial 1:"));
        assert!(html.contains("N/A"));
        assert!(html.contains("Base: https://pub.example"));
        // Zero-subrun row has no breakdown block.
        assert!(!html.contains("View 0 subruns"));
        // Empty input still renders a page.
        let empty = PortalExporter::render_index(&[], "");
        assert!(empty.contains("Total Runs"));
        assert!(!empty.contains("Base:"));
    }

    fn cov_rollup_row(model: &str, stab: Option<&str>, lo: f64, hi: f64) -> ModelRollupRow {
        ModelRollupRow {
            model: model.to_string(),
            harness: "omp".to_string(),
            regime: "legacy".to_string(),
            protocol: "unknown".to_string(),
            n_tasks: 6,
            tasks: vec!["http".to_string(), "redis".to_string()],
            n_parents: 1,
            n_trials: 3,
            n_pass: 2,
            avg_pass_rate: 66.7,
            p1: (lo + hi) / 2.0,
            p1_lo: lo,
            p1_hi: hi,
            total_cost_usd: 0.1,
            avg_cost_usd: 0.1,
            total_api_cost_usd: 0.05,
            avg_api_cost_usd: 0.05,
            stability: stab.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_render_rollup_index_ranks_stability_and_ci_clamp() {
        let rows = vec![
            cov_rollup_row("m1", Some("stable"), 40.0, 60.0),
            cov_rollup_row("m2", Some("watch"), 30.0, 50.0),
            cov_rollup_row("m3", Some("unstable"), 10.0, 30.0),
            // Narrow interval: the CI fill clamps to a 2-point minimum.
            cov_rollup_row("m4", None, 50.0, 51.0),
        ];
        let html = PortalExporter::render_rollup_index(&rows, "https://pub.example");
        for cls in ["rank-1", "rank-2", "rank-3"] {
            assert!(html.contains(cls), "{cls} missing");
        }
        assert!(html.contains("class=\"rank\">4"), "rank-4 missing");
        for cls in ["stab-ok", "stab-watch", "stab-bad", "stab-na"] {
            assert!(html.contains(cls), "{cls} missing");
        }
        assert!(html.contains("width:2%"));
        assert!(html.contains("Base: https://pub.example"));
        assert!(html.contains("4 models"));
        assert!(html.contains("3t/1r"));
        // No per-run detail in the rollup.
        assert!(!html.contains("runs/"));
    }

    #[test]
    fn test_rollup_canonical_dedup_prefers_trials_then_timestamp() {
        let mk = |id: &str, pass: f64, cost: f64, trials: u32, ts: &str| {
            let mut r = rollup_parent(
                id,
                "dedup-model",
                "http",
                pass,
                cost,
                Some(cost),
                trials,
                None,
            );
            r.timestamp = ts.to_string();
            r
        };
        // Same key: the 3-trial row beats the 1-trial row; between equal
        // trial counts the newer timestamp wins.
        let rows = PortalExporter::rollup_models(&[
            mk("d_old", 100.0, 0.10, 3, "2026-09-26T16:00:00"),
            mk("d_new", 50.0, 0.20, 3, "2026-09-26T17:00:00"),
            mk("d_small", 100.0, 0.30, 1, "2026-09-27T00:00:00"),
        ]);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.n_parents, 1);
        assert_eq!(r.n_trials, 3);
        assert_eq!(r.n_pass, 0, "50% parent with no subruns passed 0/3");
        assert!((r.avg_pass_rate - 50.0).abs() < 1e-9);
        assert!((r.total_cost_usd - 0.20).abs() < 1e-9, "newer row kept");

        // Equal trial counts AND equal timestamps: first inserted wins.
        let rows = PortalExporter::rollup_models(&[
            mk("d_first", 100.0, 0.11, 3, "2026-09-26T16:00:00"),
            mk("d_second", 100.0, 0.22, 3, "2026-09-26T16:00:00"),
        ]);
        assert_eq!(rows.len(), 1);
        assert!((rows[0].total_cost_usd - 0.11).abs() < 1e-9);
    }

    #[test]
    fn test_rollup_groups_by_protocol_harness_and_regime() {
        let mk = |id: &str,
                  model: &str,
                  pass: f64,
                  harness: Option<&str>,
                  rules: Option<&str>,
                  proto: Option<&str>| {
            let mut r = rollup_parent(id, model, "http", pass, 0.1, Some(0.1), 3, None);
            r.timestamp = "2026-09-26T16:00:00".to_string();
            r.harness = harness.map(|s| s.to_string());
            r.rules_version = rules.map(|s| s.to_string());
            r.protocol = proto.map(|s| s.to_string());
            r
        };
        // Protocols are separate comparability classes; unrecorded reads
        // "unknown". Sorted class-first: fixed_n < sequential_cap < unknown.
        let rows = PortalExporter::rollup_models(&[
            mk(
                "p_cap",
                "proto-model",
                100.0,
                None,
                None,
                Some("sequential_cap"),
            ),
            mk("p_fixed", "proto-model", 100.0, None, None, Some("fixed_n")),
            mk("p_none", "proto-model", 100.0, None, None, None),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].protocol, "fixed_n");
        assert_eq!(rows[1].protocol, "sequential_cap");
        assert_eq!(rows[2].protocol, "unknown");

        // Harness and rules regime also split the group.
        let rows = PortalExporter::rollup_models(&[
            mk("h_omp", "class-model", 80.0, None, None, None),
            mk("h_oc", "class-model", 80.0, Some("opencode"), None, None),
            mk(
                "h_v2",
                "class-model",
                80.0,
                None,
                Some("2026-09-30.1"),
                None,
            ),
        ]);
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .any(|r| r.harness == "opencode" && r.regime == "legacy"));
        assert!(rows.iter().any(|r| r.regime == "2026-09-30.1"));
        assert!(rows
            .iter()
            .any(|r| r.harness == "omp" && r.regime == "legacy"));
    }

    #[test]
    fn test_rollup_counts_subruns_best_stability_and_cost_tiebreak() {
        // Subrun counting: parent without an explicit trial count derives n
        // from its subruns and c from the 100% ones; avg falls back to the
        // parent's pass_rate when avg_pass_rate is absent.
        let mut parent = rollup_parent(
            "sb_par",
            "sub-model",
            "redis",
            50.0,
            0.20,
            Some(0.20),
            9,
            Some("stable"),
        );
        // Derived from subruns, not the (absent) explicit trial count.
        parent.total_trials = None;
        parent.avg_pass_rate = None;
        let mut sub_a = rollup_parent("sb_sub1", "sub-model", "redis", 100.0, 0.0, None, 1, None);
        sub_a.total_trials = None;
        let mut sub_b = rollup_parent("sb_sub2", "sub-model", "redis", 0.0, 0.0, None, 1, None);
        sub_b.total_trials = None;
        parent.subruns = vec![sub_a, sub_b];
        let rows = PortalExporter::rollup_models(&[parent]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].n_trials, 2);
        assert_eq!(rows[0].n_pass, 1);
        assert!((rows[0].avg_pass_rate - 50.0).abs() < 1e-9);
        assert!((rows[0].p1 - 50.0).abs() < 1e-9);
        assert_eq!(rows[0].stability.as_deref(), Some("stable"));

        // No trial count and no subruns: one trial, zero passes, no
        // stability verdict.
        let mut bare = rollup_parent("bare_1", "bare-model", "wal", 10.0, 0.1, None, 1, None);
        bare.total_trials = None;
        let rows = PortalExporter::rollup_models(&[bare]);
        assert_eq!(rows[0].n_trials, 1);
        assert_eq!(rows[0].n_pass, 0);
        assert_eq!(rows[0].stability, None);

        // Worst stability across a model's parents wins; equal averages and
        // Wilson bounds sort cheaper-first.
        let rows = PortalExporter::rollup_models(&[
            rollup_parent(
                "w_stable",
                "worst-model",
                "http",
                100.0,
                0.9,
                None,
                3,
                Some("stable"),
            ),
            rollup_parent(
                "w_unstable",
                "worst-model",
                "redis",
                100.0,
                0.1,
                None,
                3,
                Some("unstable"),
            ),
            rollup_parent(
                "w_watch",
                "worst-model",
                "pdf",
                100.0,
                0.5,
                None,
                3,
                Some("watch"),
            ),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stability.as_deref(), Some("unstable"));
        assert!((rows[0].total_cost_usd - 1.5).abs() < 1e-9);
        // 3 parents, 9 trials, all passing.
        assert_eq!(rows[0].n_parents, 3);
        assert_eq!(rows[0].n_trials, 9);

        // Cost tie-break on identical pass stats (different models, so the
        // regime/protocol/avg/p1_lo keys tie): the cheaper row leads.
        let rows = PortalExporter::rollup_models(&[
            rollup_parent("t_exp", "sort-model-b", "http", 100.0, 0.5, None, 3, None),
            rollup_parent(
                "t_cheap",
                "sort-model-a",
                "redis",
                100.0,
                0.1,
                None,
                3,
                None,
            ),
        ]);
        assert_eq!(rows.len(), 2);
        assert!((rows[0].total_cost_usd - 0.1).abs() < 1e-9);
        assert!((rows[1].total_cost_usd - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_export_rollup_excludes_frozen_rows_and_writes_floor_note() {
        let base = cov_temp("rollup_frozen");
        let results = base.join("results");
        let out = base.join("portal");
        fs::create_dir_all(&results).unwrap();
        // One below-floor model only: everything is filtered, but the page
        // and floor note still render.
        let mut old = rollup_parent("frz_old", "m", "http", 100.0, 0.01, None, 9, None);
        old.timestamp = "2026-09-16T14:54:59".to_string();
        fs::write(
            results.join("frz_old.json"),
            serde_json::to_string(&old).unwrap(),
        )
        .unwrap();
        let summary =
            PortalExporter::export_rollup(results.to_str().unwrap(), out.to_str().unwrap(), "")
                .unwrap();
        assert_eq!(summary.run_count, 0);
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("Rollup coverage floor"));
        assert!(!index.contains("frz_old"));
        let _ = fs::remove_dir_all(&base);
    }
}
