use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::report::archive::RunManifest;
use crate::report::leaderboard::{format_duration, wilson_interval_95, LeaderboardManager};

pub const PORTAL_HIDDEN_FILE: &str = ".portal_hidden";

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

pub struct PortalSummary {
    pub run_count: usize,
    pub out_dir: PathBuf,
}

/// One aggregated row of the public model rollup: a model across all tasks.
#[derive(Debug, Clone)]
pub struct ModelRollupRow {
    pub model: String,
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
const PORTAL_METHODOLOGY_HTML: &str = r#"<section class="methodology" style="margin-top:2rem;padding:1rem 1.2rem;border:1px solid var(--line);border-radius:8px;"><h2>Methodology &amp; limitations</h2><ul style="font-size:0.82rem;color:var(--mut);line-height:1.5;margin:0.4rem 0 0 1.1rem;"><li><strong>Ranking:</strong> runs rank by the Wilson 95% lower bound on the pass fraction (P@1), then the P@1 point estimate, then score — an n=3 1.00 never outranks an n=9 0.89 on thin evidence. The rollup sorts by average pass rate, ties broken by Wilson lower bound, then cost.</li><li><strong>Stability flags:</strong> any pass-rate spread across trials marks <em>unstable</em>; throughput CV &gt;25% marks <em>unstable</em> (&gt;50% flags a suspect measurement); CV 10–25% marks <em>watch</em>; otherwise <em>stable</em>. Throughput-free tasks (DNS) are judged on pass spread alone; single-trial runs default to <em>stable</em>.</li><li><strong>Infra failures count as failures:</strong> transport timeouts, DNS-resolution faults, and probe/container errors fail stages at 0% pass and are indistinguishable from model errors in scores; affected runs are not auto-excluded.</li><li><strong>Costs:</strong> total cost is the effective cost (API spend plus compute for run duration and tokens), falling back to API-only spend when compute metering is absent; avg cost/run is total divided by runs.</li><li><strong>Comparability:</strong> runs are only comparable within one <code>rules_version</code> fingerprint (<code>legacy-pre-cheat</code> for 2026-09-17 and earlier, <code>legacy-cheat</code> from 2026-09-18); cross-fingerprint ranks mix rule sets.</li><li><strong>DNS scope:</strong> the verifier scores six MUST stages (header handshake, example.com A, localhost A, test TXT, NXDOMAIN-or-empty for unknown names, 800-query burst with at least 760 ID-matched); TTL values are parsed but never asserted, and forwarding/caching are SHOULD-have judge scope only.</li><li><strong>Budget and keys:</strong> <code>budget_usd</code> caps live agent spend only and never gates cost accounting; the API key travels as env/flag into the runner and is never persisted in manifests, results, or the database.</li><li><strong>Publish freeze:</strong> only runs graded at or after the 2026-09-26 anti-cheat fix are published (timestamp &ge; 2026-09-26T15:34:10Z); earlier rows were graded under a stricter rule that has since changed. Fingerprint allowlists alone are insufficient because graded content changed without a version bump.</li><li><strong>Integrity limits:</strong> egress/static anti-cheat blocklists are self-admitted; a DNS candidate forwarding to the reference is indistinguishable from a real resolver by design; binproto STREAM, proxy X-Forwarded-For, and pubsub 1,000-subscriber requirements are published as specified but verified at reduced scale; the LLM judge may review its own model family rows; the API key travels in the agent container env.</li></ul></section>"#;

/// Rollup page appends the coverage-floor note to the shared methodology.
const ROLLUP_FLOOR_NOTE_HTML: &str =  r#"<p style="font-size:0.82rem;color:var(--mut);">Rollup coverage floor: a model appears only with at least 6 of 8 tasks and at least 18 total trials (avg 3/task); thinner coverage is excluded and logged at export.</p>"#;

/// Publish floor for the public rollup: one row per model only with >= 6 of
/// 8 tasks AND >= 18 total trials (avg 3/task). Thin-coverage rows (e.g. a
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

/* Formula Customizer Panel */
.btn-formula {
  background: rgba(88, 166, 255, 0.12);
  border: 1px solid rgba(88, 166, 255, 0.35);
  color: var(--acc);
  padding: 6px 12px;
  border-radius: 6px;
  font-size: 0.78rem;
  font-weight: 600;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  transition: all 0.15s ease;
}
.btn-formula:hover {
  background: rgba(88, 166, 255, 0.22);
  border-color: var(--acc);
  color: #fff;
}
.btn-formula.active {
  background: var(--acc);
  color: #000;
  font-weight: 700;
  border-color: var(--acc);
}
.formula-panel {
  background: #0d121c;
  border: 1px solid rgba(88, 166, 255, 0.25);
  border-radius: 8px;
  padding: 16px 18px;
  margin-bottom: 16px;
  box-shadow: 0 4px 18px rgba(0, 0, 0, 0.35);
}
.formula-header {
  display: flex;
  justify-content: space-between;
  align-items: flex-start;
  flex-wrap: wrap;
  gap: 10px;
  margin-bottom: 12px;
  padding-bottom: 10px;
  border-bottom: 1px solid rgba(255, 255, 255, 0.08);
}
.formula-presets {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
  align-items: center;
  margin-bottom: 14px;
}
.preset-pill {
  background: #161b22;
  border: 1px solid var(--line);
  border-radius: 20px;
  padding: 4px 12px;
  font-size: 0.74rem;
  color: var(--mut);
  cursor: pointer;
  transition: all 0.15s ease;
  font-weight: 500;
}
.preset-pill:hover {
  background: #1f2736;
  color: #fff;
  border-color: rgba(255, 255, 255, 0.2);
}
.preset-pill.active {
  background: rgba(88, 166, 255, 0.2);
  border-color: var(--acc);
  color: var(--acc);
  font-weight: 700;
}
.formula-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
  gap: 12px;
  margin-bottom: 14px;
}
.param-card {
  background: #111722;
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 6px;
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.param-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  font-size: 0.78rem;
}
.param-title {
  font-weight: 600;
  color: #fff;
  display: flex;
  align-items: center;
  gap: 6px;
}
.param-val {
  font-family: monospace;
  font-size: 0.78rem;
  padding: 1px 6px;
  border-radius: 4px;
  background: rgba(88, 166, 255, 0.15);
  color: var(--acc);
  font-weight: 700;
}
.param-desc {
  font-size: 0.7rem;
  color: var(--mut);
  line-height: 1.3;
}
.slider-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 2px;
}
.formula-slider {
  flex: 1;
  -webkit-appearance: none;
  height: 5px;
  border-radius: 3px;
  background: #212836;
  outline: none;
  cursor: pointer;
}
.formula-slider::-webkit-slider-thumb {
  -webkit-appearance: none;
  width: 15px;
  height: 15px;
  border-radius: 50%;
  background: var(--acc);
  cursor: pointer;
  border: 1px solid #fff;
  box-shadow: 0 0 5px rgba(88, 166, 255, 0.5);
}
.formula-slider::-moz-range-thumb {
  width: 15px;
  height: 15px;
  border-radius: 50%;
  background: var(--acc);
  cursor: pointer;
  border: 1px solid #fff;
}
.formula-math {
  background: #090d14;
  border: 1px solid rgba(88, 166, 255, 0.2);
  border-radius: 6px;
  padding: 8px 12px;
  font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, monospace;
  font-size: 0.74rem;
  color: #c9d1d9;
  display: flex;
  align-items: center;
  gap: 8px;
  overflow-x: auto;
}
.rank-delta-up {
  color: var(--grn);
  font-size: 0.7rem;
  font-weight: 700;
  background: rgba(63, 185, 80, 0.15);
  padding: 1px 4px;
  border-radius: 3px;
  margin-left: 3px;
}
.rank-delta-down {
  color: var(--red);
  font-size: 0.7rem;
  font-weight: 700;
  background: rgba(248, 81, 73, 0.15);
  padding: 1px 4px;
  border-radius: 3px;
  margin-left: 3px;
}
.rank-delta-same {
  color: var(--mut);
  font-size: 0.7rem;
  margin-left: 3px;
}

"#;

const PORTAL_JS: &str = r#"
(function() {
  function shortModelName(modelName) {
    if (!modelName) return '';
    return modelName.split('/').pop() || modelName;
  }

  function getModelMeta(modelName) {
    const m = (modelName || '').toLowerCase();
    if (m.includes('claude') || m.includes('anthropic')) {
      return { family: 'Anthropic', icon: '✦', color: '#f27244', bg: 'rgba(242, 114, 68, 0.14)', border: 'rgba(242, 114, 68, 0.35)' };
    }
    if (m.includes('gpt') || m.includes('openai') || m.includes('o1') || m.includes('o3') || m.includes('o4')) {
      return { family: 'OpenAI', icon: '✳️', color: '#10a37f', bg: 'rgba(16, 163, 127, 0.14)', border: 'rgba(16, 163, 127, 0.35)' };
    }
    if (m.includes('gemini') || m.includes('google')) {
      return { family: 'Google', icon: '✨', color: '#4285f4', bg: 'rgba(66, 133, 244, 0.14)', border: 'rgba(66, 133, 244, 0.35)' };
    }
    if (m.includes('deepseek')) {
      return { family: 'DeepSeek', icon: '🐋', color: '#0084ff', bg: 'rgba(0, 132, 255, 0.14)', border: 'rgba(0, 132, 255, 0.35)' };
    }
    if (m.includes('llama') || m.includes('meta')) {
      return { family: 'Meta', icon: '🦙', color: '#8b5cf6', bg: 'rgba(139, 92, 246, 0.14)', border: 'rgba(139, 92, 246, 0.35)' };
    }
    if (m.includes('qwen')) {
      return { family: 'Qwen', icon: '🔮', color: '#a855f7', bg: 'rgba(168, 85, 247, 0.14)', border: 'rgba(168, 85, 247, 0.35)' };
    }
    if (m.includes('mistral') || m.includes('codestral') || m.includes('pixtral')) {
      return { family: 'Mistral', icon: '🌪️', color: '#ff7000', bg: 'rgba(255, 112, 0, 0.14)', border: 'rgba(255, 112, 0, 0.35)' };
    }
    if (m.includes('minimax')) {
      return { family: 'MiniMax', icon: '⚡', color: '#e11d48', bg: 'rgba(225, 29, 72, 0.14)', border: 'rgba(225, 29, 72, 0.35)' };
    }
    if (m.includes('nova') || m.includes('amazon')) {
      return { family: 'Amazon', icon: '☀️', color: '#f59e0b', bg: 'rgba(245, 158, 11, 0.14)', border: 'rgba(245, 158, 11, 0.35)' };
    }
    if (m.includes('cohere') || m.includes('command')) {
      return { family: 'Cohere', icon: '🌿', color: '#14b8a6', bg: 'rgba(20, 184, 166, 0.14)', border: 'rgba(20, 184, 166, 0.35)' };
    }
    let hash = 0;
    for (let i = 0; i < m.length; i++) hash = (hash * 31 + m.charCodeAt(i)) & 0xffffffff;
    const PALETTE = [
      { color: '#06b6d4', bg: 'rgba(6, 182, 212, 0.14)', border: 'rgba(6, 182, 212, 0.35)' },
      { color: '#ec4899', bg: 'rgba(236, 72, 153, 0.14)', border: 'rgba(236, 72, 153, 0.35)' },
      { color: '#84cc16', bg: 'rgba(132, 204, 22, 0.14)', border: 'rgba(132, 204, 22, 0.35)' },
      { color: '#3b82f6', bg: 'rgba(59, 130, 246, 0.14)', border: 'rgba(59, 130, 246, 0.35)' },
      { color: '#f43f5e', bg: 'rgba(244, 63, 94, 0.14)', border: 'rgba(244, 63, 94, 0.35)' },
      { color: '#6366f1', bg: 'rgba(99, 102, 241, 0.14)', border: 'rgba(99, 102, 241, 0.35)' },
      { color: '#10b981', bg: 'rgba(16, 185, 129, 0.14)', border: 'rgba(16, 185, 129, 0.35)' },
      { color: '#f59e0b', bg: 'rgba(245, 158, 11, 0.14)', border: 'rgba(245, 158, 11, 0.35)' }
    ];
    const choice = PALETTE[Math.abs(hash) % PALETTE.length];
    return { family: 'Other', icon: '🤖', color: choice.color, bg: choice.bg, border: choice.border };
  }
  const RUNS_DATA = window.__SUBDOLLAR_RUNS__ || [];
  
  const TASK_SPECS = {
    http: {
      name: 'HTTP/1.1 Web Server',
      icon: '🌐',
      desc: 'RFC 7230/7231 web server handling GET /, 404s, /echo/{str}, header reflection, POST/GET /files/{name} persistence, keep-alive socket reuse, and path traversal security guards.',
      stagesCount: 5,
      refInfo: 'nginx:alpine reference container on port 8081 (~33,000 req/s with wrk load test)',
      benchTool: 'wrk -t2 -c20 -d3s http://127.0.0.1:8080/'
    },
    redis: {
      name: 'In-Memory Concurrent Redis Clone',
      icon: '⚡',
      desc: 'RESP2 protocol TCP server handling pipelined streams, simple strings, bulk strings, integers, errors, PING, ECHO, SET (with PX expiry), GET, DEL, EXISTS, and INCR non-integer guards.',
      stagesCount: 4,
      refInfo: 'redis:alpine reference container on port 6380 (~71,000 req/s with redis-benchmark)',
      benchTool: 'redis-benchmark -p 6379 -n 5000 -c 10 -t get,set -q'
    },
    dns: {
      name: 'RFC 1035 UDP DNS Server',
      icon: '📡',
      desc: 'Raw UDP DNS resolver handling RFC 1035 packet headers, query questions, standard flags (QR, Opcode, AA, RD, RA, RCODE), dynamic port listening, and A-record address resolution.',
      stagesCount: 6,
      refInfo: 'Reference RFC 1035 UDP resolver on testbed (~19,000 queries/s)',
      benchTool: 'UDP raw-socket sequential burst (200 queries)'
    },
    wal: {
      name: 'TCP Write-Ahead Log',
      icon: '📓',
      desc: 'TCP WAL broker handling PUB append with fsync durability, SUB-from-offset replay, topic isolation, SIGKILL crash recovery, and PUB burst throughput.',
      stagesCount: 5,
      refInfo: 'fsync-bound append on disk (~1,000 msgs/s calibrated from empirical batches)',
      benchTool: '2000-msg PUB burst + SIGKILL recovery probe'
    },
    proxy: {
      name: 'HTTP Reverse Proxy',
      icon: '🔀',
      desc: 'HTTP reverse proxy on port 8091 over three upstream servers: round-robin routing, upstream keep-alive pooling, X-Forwarded-For injection, and failover with zero dropped requests.',
      stagesCount: 5,
      refInfo: 'same nginx denominator as http (~33,000 req/s: a proxy cannot beat direct)',
      benchTool: '200-req routing + failover + concurrent GET burst'
    },
    pubsub: {
      name: 'WebSocket Pub/Sub Broker',
      icon: '📢',
      desc: 'WebSocket fan-out broker on port 8092: RFC 6455 handshake, multi-subscriber broadcast, channel isolation, ping/pong, and 200-subscriber soak.',
      stagesCount: 4,
      refInfo: 'fan-out soak on testbed (~80,000 deliveries/s calibrated from empirical batches)',
      benchTool: 'Soak: 200 subscribers x 20 msgs'
    },
    pdf: {
      name: 'PDF-to-Text Microservice',
      icon: '📄',
      desc: 'RFC-compliant HTTP microservice on port 8090 converting binary PDF documents to plaintext using a custom PDF parser and DEFLATE engine (no external PDF libraries).',
      stagesCount: 5,
      refInfo: 'Reference DEFLATE/xref text extractor on testbed (~4,600 pages/s)',
      benchTool: 'Synthetic multi-page PDF extraction burst (20 requests, 60 pages)'
    }
  };

  const DEFAULT_WEIGHTS = {
    wCost: 1.0,
    wTp: 1.0,
    wDur: 0.0,
    wTok: 0.0,
    wTurns: 0.0,
    strictPass: true
  };

  const PRESETS = {
    default: { wCost: 1.0, wTp: 1.0, wDur: 0.0, wTok: 0.0, wTurns: 0.0, strictPass: true },
    speed:   { wCost: 0.5, wTp: 2.5, wDur: 0.5, wTok: 0.0, wTurns: 0.0, strictPass: true },
    frugal:  { wCost: 2.5, wTp: 0.2, wDur: 0.0, wTok: 1.5, wTurns: 0.0, strictPass: true },
    agentic: { wCost: 0.8, wTp: 0.5, wDur: 1.2, wTok: 0.5, wTurns: 2.0, strictPass: true },
    lenient: { wCost: 1.0, wTp: 1.0, wDur: 0.0, wTok: 0.0, wTurns: 0.0, strictPass: false }
  };

  function loadStoredWeights() {
    try {
      const raw = localStorage.getItem('subdollarbench_formula_weights');
      if (raw) {
        const parsed = JSON.parse(raw);
        return {
          wCost: typeof parsed.wCost === 'number' ? parsed.wCost : DEFAULT_WEIGHTS.wCost,
          wTp: typeof parsed.wTp === 'number' ? parsed.wTp : DEFAULT_WEIGHTS.wTp,
          wDur: typeof parsed.wDur === 'number' ? parsed.wDur : DEFAULT_WEIGHTS.wDur,
          wTok: typeof parsed.wTok === 'number' ? parsed.wTok : DEFAULT_WEIGHTS.wTok,
          wTurns: typeof parsed.wTurns === 'number' ? parsed.wTurns : DEFAULT_WEIGHTS.wTurns,
          strictPass: typeof parsed.strictPass === 'boolean' ? parsed.strictPass : DEFAULT_WEIGHTS.strictPass
        };
      }
    } catch (e) {}
    return Object.assign({}, DEFAULT_WEIGHTS);
  }

  function saveStoredWeights(w) {
    try {
      localStorage.setItem('subdollarbench_formula_weights', JSON.stringify(w));
    } catch (e) {}
  }

  let currentWeights = loadStoredWeights();
  let currentTab = 'models';
  let searchQuery = '';
  let passOnly = false;
  let sortCol = 'score';
  let sortDir = -1;
  // Models-tab ordering. Default: pooled P@1 lower bound, descending.
  let modelsSortCol = 'p1lo';
  let modelsSortDir = -1;
  let formulaPanelOpen = false;
  try {
    formulaPanelOpen = localStorage.getItem('subdollarbench_formula_open') === '1';
  } catch (e) {}

  function escapeHtml(s) {
    if (s === null || s === undefined) return '';
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }

  function formatDuration(sec) {
    if (sec === null || sec === undefined) return '-';
    if (sec < 60) return `${Math.round(sec)}s`;
    const m = Math.floor(sec / 60);
    const s = Math.round(sec % 60);
    return `${m}m ${s}s`;
  }

  function formatThroughput(tp, task) {
    if (!tp) return 'N/A';
    const t = (task || '').toLowerCase();
    const unit = t === 'pdf' ? 'pages/s' : (t === 'dns' ? 'queries/s' : 'req/s');
    return `${Math.round(tp).toLocaleString()} ${unit}`;
  }

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

  function getBaseline(task) {
    const t = (task || 'http').toLowerCase();
    if (t === 'redis') return 71000;
    if (t === 'dns') return 19000;
    if (t === 'pdf') return 4600;
    if (t === 'wal') return 1000;
    if (t === 'proxy') return 33000;
    if (t === 'pubsub') return 80000;
    return 33000;
  }
  function regimeOf(r) {
    if (r.ref_image) {
      const hex = (r.ref_image.split('@')[1] || r.ref_image).replace(/^sha256:/, '');
      return r.ref_drift ? hex.slice(0, 12) + '+drift' : hex.slice(0, 12);
    }
    return 'legacy';
  }

  function computeScore(r, weights) {
    const w = weights || DEFAULT_WEIGHTS;
    let confFactor = 1.0;
    const passRate = (r.avg_pass_rate !== undefined && r.avg_pass_rate !== null) ? r.avg_pass_rate : ((r.pass_rate !== undefined && r.pass_rate !== null) ? r.pass_rate : 0);
    if (w.strictPass) {
      if (passRate < 100) return 0.0;
    } else {
      confFactor = passRate / 100.0;
    }
    const relFactor = (r.pass_at_1 !== undefined && r.pass_at_1 !== null) ? (r.pass_at_1 / 100.0) : 1.0;

    const effCost = (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null) ? r.effective_cost_usd : (r.total_cost_usd || 0.01);
    const costCents = Math.max(effCost * 100.0, 0.0001);
    const baseCost = 100.0 / costCents;
    const costFactor = Math.pow(baseCost, w.wCost);

    const candTp = Math.max(r.throughput_req_sec || 0, 0);
    const hasLiveRef = (r.reference_throughput_req_sec && r.reference_throughput_req_sec > 0);
    // No live reference, no score: fallback baselines are unlabeled
    // constants unmeasured on this testbed (mirrors the Rust scorer).
    if (w.wTp > 0 && !hasLiveRef) return null;
    const refTp = hasLiveRef ? r.reference_throughput_req_sec : 0;
    const speedRatio = refTp > 0 ? (candTp / refTp) : 0;
    let tpFactor = 1.0;
    if (w.wTp > 0) {
      if (speedRatio <= 0) return 0.0;
      const normRatio = (speedRatio * (1.0 + 2.0 * speedRatio)) / 3.0;
      tpFactor = Math.pow(normRatio, w.wTp) * 3.0;
    }

    let durFactor = 1.0;
    if (w.wDur > 0) {
      const dur = Math.max(r.duration_seconds || 60, 5.0);
      durFactor = Math.pow(60.0 / dur, w.wDur);
    }

    let tokFactor = 1.0;
    if (w.wTok > 0) {
      const totalTokens = Math.max(r.total_tokens || ((r.prompt_tokens || 0) + (r.cached_tokens || 0) + (r.completion_tokens || 0)), 5000);
      tokFactor = Math.pow(50000.0 / totalTokens, w.wTok);
    }

    let turnsFactor = 1.0;
    if (w.wTurns > 0) {
      const turns = Math.max(r.turns || 8, 1);
      turnsFactor = Math.pow(8.0 / turns, w.wTurns);
    }

    return confFactor * relFactor * costFactor * tpFactor * durFactor * tokFactor * turnsFactor;
  }

  function isCustomFormula(w) {
    if (!w) return false;
    return Math.abs(w.wCost - DEFAULT_WEIGHTS.wCost) > 0.001 ||
           Math.abs(w.wTp - DEFAULT_WEIGHTS.wTp) > 0.001 ||
           Math.abs(w.wDur - DEFAULT_WEIGHTS.wDur) > 0.001 ||
           Math.abs(w.wTok - DEFAULT_WEIGHTS.wTok) > 0.001 ||
           Math.abs(w.wTurns - DEFAULT_WEIGHTS.wTurns) > 0.001 ||
           w.strictPass !== DEFAULT_WEIGHTS.strictPass;
  }

  function computeDefaultRanks(runs, tab) {
    let list = runs.slice();
    if (tab && tab !== 'all') {
      list = list.filter(r => (r.task || '').toLowerCase() === tab.toLowerCase());
    }
    const rankLo = r => (r.pass_at_1_lo !== undefined && r.pass_at_1_lo !== null) ? r.pass_at_1_lo : ((r.pass_at_1 !== undefined && r.pass_at_1 !== null) ? r.pass_at_1 : (r.pass_rate || 0));
    const scoreOrNeg = (r, w) => { const v = computeScore(r, w); return (v === null || v === undefined) ? -1 : v; };
    list.sort((a, b) => (scoreOrNeg(b, DEFAULT_WEIGHTS) - scoreOrNeg(a, DEFAULT_WEIGHTS)) || (rankLo(b) - rankLo(a)));
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

    filtered.sort((a, b) => {
      let vA, vB;
      if (sortCol === 'score') {
        const sA = computeScore(a, currentWeights);
        const sB = computeScore(b, currentWeights);
        vA = (sA === null || sA === undefined) ? -1 : sA;
        vB = (sB === null || sB === undefined) ? -1 : sB;
        if (vA === vB) {
          const loA = (a.pass_at_1_lo !== undefined && a.pass_at_1_lo !== null) ? a.pass_at_1_lo : 0;
          const loB = (b.pass_at_1_lo !== undefined && b.pass_at_1_lo !== null) ? b.pass_at_1_lo : 0;
          return sortDir === 1 ? (loA - loB) : (loB - loA);
        }
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

    const isCustom = isCustomFormula(currentWeights);

    return runs.map((r, idx) => {
      const curRank = idx + 1;
      let deltaBadge = '';
      if (isCustom && defaultRanks && defaultRanks[r.id] !== undefined) {
        const defRank = defaultRanks[r.id];
        const delta = defRank - curRank;
        if (delta > 0) {
          deltaBadge = `<span class="rank-delta-up" title="Up ${delta} rank(s) vs default formula">▲ +${delta}</span>`;
        } else if (delta < 0) {
          deltaBadge = `<span class="rank-delta-down" title="Down ${Math.abs(delta)} rank(s) vs default formula">▼ ${delta}</span>`;
        } else {
          deltaBadge = `<span class="rank-delta-same" title="Rank unchanged vs default formula">=</span>`;
        }
      }

      let rankBadge = `<span style="font-weight: 700; color: var(--mut);">#${curRank}</span>${deltaBadge}`;
      if (curRank === 1) rankBadge = `<span style="font-weight: 800; font-size: 1rem; color: var(--gold);">🥇 #1</span>${deltaBadge}`;
      else if (curRank === 2) rankBadge = `<span style="font-weight: 800; font-size: 1rem; color: #c0c0c0;">🥈 #2</span>${deltaBadge}`;
      else if (curRank === 3) rankBadge = `<span style="font-weight: 800; font-size: 1rem; color: #cd7f32;">🥉 #3</span>${deltaBadge}`;

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
              ${r.subruns.map((sr, sIdx) => {
                const sPass = (sr.pass_rate || 0) >= 100;
                const statusTag = sPass ? '<span style="color:var(--grn);font-weight:600;">✔ 100%</span>' : `<span style="color:var(--red);font-weight:600;">✕ ${Math.round(sr.pass_rate || 0)}%</span>`;
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
      
      const effCost = (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null) ? r.effective_cost_usd : r.total_cost_usd;
      const apiCost = r.total_cost_usd;
      const apiDiff = (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null && Math.abs(r.effective_cost_usd - r.total_cost_usd) > 0.0001)
        ? `<span style="font-size: 0.7rem; color: var(--mut);">(API: $${r.total_cost_usd.toFixed(4)})</span>`
        : '';
      const totalTokens = r.total_tokens || ((r.prompt_tokens || 0) + (r.cached_tokens || 0) + (r.completion_tokens || 0));
      const tokStr = `${(totalTokens / 1000).toFixed(1)}k tokens`;

      const curScore = computeScore(r, currentWeights);
      const defScore = computeScore(r, DEFAULT_WEIGHTS);
      const scoreMain = (curScore === null || curScore === undefined)
        ? `-- <small style="font-size: 0.72rem; color: var(--mut);">(eff ${(r.efficiency_score || 0).toFixed(1)})</small>`
        : `${curScore.toFixed(1)} <small style="font-size: 0.72rem; color: var(--mut);">pts/¢</small>`;
      
      let scoreSub = '';
      if (isCustom) {
        scoreSub = `<div style="font-size: 0.7rem; color: var(--mut);">default: ${(defScore === null || defScore === undefined) ? '--' : defScore.toFixed(1)}</div>`;
      } else {
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
    const passRuns = taskRuns.filter(r => currentWeights.strictPass ? (r.pass_rate >= 100) : ((r.pass_rate || 0) > 0));
    const scoreNum = (r) => { const v = computeScore(r, currentWeights); return (v === null || v === undefined) ? -1 : v; };
    const topRun = passRuns.slice().sort((a, b) => scoreNum(b) - scoreNum(a))[0];

    const peakTpRun = passRuns.slice().sort((a, b) => (b.throughput_req_sec || 0) - (a.throughput_req_sec || 0))[0];
    const lowestCostRun = passRuns.slice().sort((a, b) => {
      const cA = a.effective_cost_usd !== undefined && a.effective_cost_usd !== null ? a.effective_cost_usd : a.total_cost_usd;
      const cB = b.effective_cost_usd !== undefined && b.effective_cost_usd !== null ? b.effective_cost_usd : b.total_cost_usd;
      return cA - cB;
    })[0];

    const topModel = topRun ? shortModelName(topRun.model) : 'None yet';
    const topScoreVal = topRun ? computeScore(topRun, currentWeights) : null;
    const topScore = topRun ? (((topScoreVal === null || topScoreVal === undefined) ? '-- (eff ' + (topRun.efficiency_score || 0).toFixed(1) + ')' : topScoreVal.toFixed(1)) + ' pts/¢') : '-';
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
    const isCustom = isCustomFormula(currentWeights);
    const customBadge = isCustom
      ? `<span class="badge badge-info" style="margin-left: auto;">🎛️ Custom Formula Active</span>`
      : '';

    return `
      <div style="margin-bottom: 12px; padding: 10px 14px; background: rgba(88,166,255,0.06); border: 1px solid rgba(88,166,255,0.2); border-radius: 6px; font-size: 0.78rem; color: var(--mut); display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
        <span>📐</span>
        <span><strong>Scoring Regime:</strong> Throughput scores only where a live reference anchors them — <strong>HTTP/Proxy</strong> (Nginx, live), <strong>Redis</strong> (live). <strong>DNS</strong> (live CoreDNS where measured), <strong>PDF/WAL/PubSub</strong> timings are reported but unscored (--). DNS rejoined the global board 2026-09-27 after healthy fresh-wave cells.</span>
        ${customBadge}
      </div>
    `;
  }

  function renderMathExpression(w) {
    const parts = [];
    parts.push(w.strictPass ? '<strong>StrictPass(100%)</strong>' : `(Pass% / 100)`);
    if (w.wCost > 0) {
      parts.push(`(100 / Cost<sub>¢</sub>)<sup>${Number(w.wCost).toFixed(1)}</sup>`);
    }
    if (w.wTp > 0) {
      parts.push(`(NormSpeed)<sup>${Number(w.wTp).toFixed(1)}</sup> &times; 3.0`);
    }
    if (w.wDur > 0) {
      parts.push(`(60s / Dur)<sup>${Number(w.wDur).toFixed(1)}</sup>`);
    }
    if (w.wTok > 0) {
      parts.push(`(50k / Tokens)<sup>${Number(w.wTok).toFixed(1)}</sup>`);
    }
    if (w.wTurns > 0) {
      parts.push(`(8 / Turns)<sup>${Number(w.wTurns).toFixed(1)}</sup>`);
    }
    return 'Score = ' + (parts.length > 0 ? parts.join(' &times; ') : '1.0');
  }

  function syncFormulaControls() {
    const panel = document.getElementById('formulaPanel');
    const toggleBtn = document.getElementById('formulaToggleBtn');
    const badge = document.getElementById('formulaStatusBadge');
    const math = document.getElementById('formulaMathDisplay');

    if (panel) {
      panel.style.display = formulaPanelOpen ? 'block' : 'none';
    }
    if (toggleBtn) {
      if (formulaPanelOpen) toggleBtn.classList.add('active');
      else toggleBtn.classList.remove('active');
    }

    const isCustom = isCustomFormula(currentWeights);
    if (badge) {
      badge.style.display = isCustom ? 'inline-block' : 'none';
    }

    ['wCost', 'wTp', 'wDur', 'wTok', 'wTurns'].forEach(k => {
      const slider = document.getElementById('slider_' + k);
      const valEl = document.getElementById('val_' + k);
      if (slider) slider.value = currentWeights[k];
      if (valEl) valEl.textContent = Number(currentWeights[k]).toFixed(1) + '×';
    });

    const radioStrict = document.getElementById('radio_strict');
    const radioLenient = document.getElementById('radio_lenient');
    const valStrict = document.getElementById('val_strictPass');
    if (radioStrict) radioStrict.checked = !!currentWeights.strictPass;
    if (radioLenient) radioLenient.checked = !currentWeights.strictPass;
    if (valStrict) valStrict.textContent = currentWeights.strictPass ? 'Strict' : 'Lenient';

    if (math) {
      math.innerHTML = renderMathExpression(currentWeights);
    }

    // Sync presets highlighting
    Object.keys(PRESETS).forEach(pKey => {
      const p = PRESETS[pKey];
      const btn = document.getElementById('preset_' + pKey);
      if (btn) {
        const matches = Math.abs(p.wCost - currentWeights.wCost) < 0.001 &&
                        Math.abs(p.wTp - currentWeights.wTp) < 0.001 &&
                        Math.abs(p.wDur - currentWeights.wDur) < 0.001 &&
                        Math.abs(p.wTok - currentWeights.wTok) < 0.001 &&
                        Math.abs(p.wTurns - currentWeights.wTurns) < 0.001 &&
                        p.strictPass === currentWeights.strictPass;
        if (matches) btn.classList.add('active');
        else btn.classList.remove('active');
      }
    });
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
  // (DNS quarantine lifted 2026-09-27: fresh-wave dns cells healthy, e.g.
  // muse-spark dns 100%/watch; backend QUARANTINED_TASKS already empty.)
  function aggregateModels() {
    // 1. Group runs by (model, effort, task) and pick canonical run (highest trials, latest timestamp)
    const canonical = {};
    for (const r of RUNS_DATA) {
      const task = (r.task || '').toLowerCase();
      const key = (r.model || '?') + '\u0001' + (r.effort || 'auto') + '\u0001' + task;
      const n = r.total_trials || (r.subruns && r.subruns.length) || 1;
      const existing = canonical[key];
      if (!existing) {
        canonical[key] = r;
      } else {
        const exN = existing.total_trials || (existing.subruns && existing.subruns.length) || 1;
        if (n > exN || (n === exN && (r.timestamp || '') > (existing.timestamp || ''))) {
          canonical[key] = r;
        }
      }
    }

    // 2. Aggregate only canonical runs across tasks
    const groups = {};
    for (const r of Object.values(canonical)) {
      const key = (r.model || '?') + '\u0001' + (r.effort || 'auto');
      const g = groups[key] || (groups[key] = {
        model: r.model || '?', effort: r.effort || 'auto',
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
        model: g.model, effort: g.effort,
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
    if (col === 'model') return (r.model + ' ' + r.effort).toLowerCase();
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
    const rows = aggregateModels().filter(r => !q || ((r.model + ' ' + r.effort).toLowerCase().includes(q)));
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
        🏅 <strong>Cross-task rollup</strong>: one row per model+effort across all tasks. Ranked by pooled P@1 lower bound — the honest answer to "which cheap model codes best".
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
    syncFormulaControls();
    // The formula customizer never scored the Models rollup: hide its
    // toggle there, restore it on task tabs.
    const ft0 = document.getElementById('formulaToggleBtn');
    if (ft0) ft0.style.display = (typeof currentTab !== 'undefined' && currentTab === 'models') ? 'none' : '';
    const fp0 = document.getElementById('formulaPanel');
    if (fp0 && typeof currentTab !== 'undefined' && currentTab === 'models') fp0.style.display = 'none';

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
      const ft = document.getElementById('formulaToggleBtn');
      if (ft) ft.style.display = 'none';
      const fp = document.getElementById('formulaPanel');
      if (fp) fp.style.display = 'none';
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

  window.__subdollar_sort = function(col) {
    if (sortCol === col) {
      sortDir = -sortDir;
    } else {
      sortCol = col;
      sortDir = -1;
    }
    updateLeaderboardView();
  };

  window.__subdollar_toggle_formula = function() {
    formulaPanelOpen = !formulaPanelOpen;
    try {
      localStorage.setItem('subdollarbench_formula_open', formulaPanelOpen ? '1' : '0');
    } catch (e) {}
    syncFormulaControls();
  };

  window.__subdollar_apply_preset = function(presetKey) {
    if (PRESETS[presetKey]) {
      currentWeights = Object.assign({}, PRESETS[presetKey]);
      saveStoredWeights(currentWeights);
      updateLeaderboardView();
    }
  };

  window.__subdollar_weight_change = function(param, val) {
    currentWeights[param] = parseFloat(val);
    saveStoredWeights(currentWeights);
    updateLeaderboardView();
  };

  window.__subdollar_strict_change = function(isStrict) {
    currentWeights.strictPass = !!isStrict;
    saveStoredWeights(currentWeights);
    updateLeaderboardView();
  };

  window.__subdollar_reset_formula = function() {
    currentWeights = Object.assign({}, DEFAULT_WEIGHTS);
    saveStoredWeights(currentWeights);
    updateLeaderboardView();
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', updateLeaderboardView);
  } else {
    updateLeaderboardView();
  }
})();
"#;

pub struct PortalExporter;

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
        let visible: Vec<_> = results
            .into_iter()
            .filter(|r| !is_hidden(runs_path, &r.id) && !r.id.starts_with("test_"))
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


    /// Model-rollup-only export for the public portal: one row per model
    /// across ALL tasks. No per-run detail, no run ids or links, no
    /// console/workspace content. Writes ONLY `index.html` and leaves any
    /// existing `runs/` directory as-is, so stale per-run pages are never
    /// served as fresh output. Per-run detail stays internal (see `export`
    /// above); the public portal is aggregated by design.
    pub fn export_rollup(results_dir: &str, out: &str, base_url: &str) -> Result<PortalSummary> {
        let results = LeaderboardManager::load_all(results_dir);
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

    /// Aggregate parents into one row per model across all tasks.
    /// Step 1 (dedup): one canonical parent per (model, effort, task) —
    /// highest trial count wins, latest timestamp breaks ties — mirroring
    /// the `aggregateModels` cross-task rollup in PORTAL_JS. Step 2: group
    /// canonical parents by model only (no per-task split).
    fn rollup_models(
        results: &[crate::report::leaderboard::BenchmarkRunResult],
    ) -> Vec<ModelRollupRow> {
        let mut canonical: HashMap<
            (String, String, String),
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
        let mut groups: HashMap<String, Acc> = HashMap::new();
        for r in canonical.into_values() {
            let g = groups.entry(r.model.clone()).or_insert_with(|| Acc {
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
            .map(|(model, g)| {
                let (p1_lo, p1_hi) = wilson_interval_95(g.n_trials, g.n_pass);
                let mut tasks: Vec<String> = g.tasks.into_iter().collect();
                tasks.sort();
                ModelRollupRow {
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
        // Rank by average pass rate (desc), then P@1 lower bound, then cost.
        rows.sort_by(|a, b| {
            b.avg_pass_rate
                .partial_cmp(&a.avg_pass_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
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
            let medal = match i + 1 {
                1 => "🥇 #1".to_string(),
                2 => "🥈 #2".to_string(),
                3 => "🥉 #3".to_string(),
                n => format!("#{}", n),
            };
            let stab = match r.stability.as_deref() {
                Some("stable") => {
                    "<span class=\"badge badge-pass\">🟢 stable</span>".to_string()
                }
                Some("watch") => {
                    "<span class=\"badge badge-info\">🟡 watch</span>".to_string()
                }
                Some("unstable") => {
                    "<span class=\"badge badge-fail\">🔴 unstable</span>".to_string()
                }
                _ => "<span style=\"color:var(--mut);\">-</span>".to_string(),
            };
            trs.push_str(&format!(
                "<tr><td style=\"text-align:center;\">{medal}</td>\
                 <td><span style=\"font-weight:600;\" title=\"{model}\">{short}</span></td>\
                 <td style=\"text-align:center;\">{ntasks} <span style=\"font-size:0.72rem;color:var(--mut);\" title=\"{tasks}\">{tasks}</span></td>\
                 <td style=\"text-align:center;\">{ntrials} <span style=\"font-size:0.72rem;color:var(--mut);\">trials / {nparents} runs</span></td>\
                 <td style=\"text-align:center;font-weight:700;\">{avg:.0}%</td>\
                 <td style=\"text-align:center;font-weight:700;\">{p1:.0}% <span style=\"font-size:0.72rem;color:var(--mut);\">({lo:.0}–{hi:.0})</span></td>\
                 <td style=\"text-align:center;\">${total_api:.4}</td>\
                 <td style=\"text-align:center;\">${total:.4}</td>\
                 <td style=\"text-align:center;\">${avg_api:.4}</td>\
                 <td style=\"text-align:center;\">${avgc:.4}</td>\
                 <td style=\"text-align:center;\">{stab}</td><td style=\"text-align:center;font-size:0.72rem;color:var(--mut);\">post-2026-09-26</td></tr>\n",
                medal = medal,
                model = Self::esc(&r.model),
                short = Self::esc(LeaderboardManager::short_model_name(&r.model)),
                ntasks = r.n_tasks,
                tasks = Self::esc(&r.tasks.join(", ")),
                ntrials = r.n_trials,
                nparents = r.n_parents,
                avg = r.avg_pass_rate,
                p1 = r.p1,
                lo = r.p1_lo,
                hi = r.p1_hi,
                total = r.total_cost_usd,
                avgc = r.avg_cost_usd,
                total_api = r.total_api_cost_usd,
                avg_api = r.avg_api_cost_usd,
                stab = stab,
            ));
        }
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>SubDollarBench Model Rollup</title>\
             <style>{css}</style></head><body><main>\
             <h1>🏆 SubDollarBench Model Rollup</h1>\
             <div class=\"sub\">Publish freeze 2026-09-26T15:34:10Z: pre-freeze rows excluded; graded content changed without a version bump, so fingerprint allowlists alone are insufficient.</div>\
             <div class=\"sub\">Public aggregate: one row per model across all tasks · {n} models · {now}{base_info}</div>\
             <p style=\"font-size:0.8rem;color:var(--mut);\">Per-run detail (logs, code, workspaces) is internal and not published here.</p>\
             <table><thead><tr>\
             <th>#</th><th>Model</th><th>Tasks</th><th>Trials</th><th>Avg pass</th>\
             <th>P@1 (lo-hi)</th><th>Total API cost</th><th>Total effective cost</th><th>Avg API cost/run</th><th>Avg effective cost/run</th><th>Stability</th><th>Rules</th>\
             </tr></thead><tbody>{trs}</tbody></table>{methodology}{floor_note}\
             <footer class=\"muted\">SubDollarBench rollup · {now}</footer></main></body></html>",
            css = PORTAL_CSS,
            n = rows.len(),
            methodology = PORTAL_METHODOLOGY_HTML,
            floor_note = ROLLUP_FLOOR_NOTE_HTML,
            now = now,
            base_info = base_info,
            trs = trs,
        )
    }

    fn secret_patterns() -> Vec<Regex> {
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
        let dns_n = results
            .iter()
            .filter(|r| r.task.eq_ignore_ascii_case("dns"))
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
            let unit = match r.task.to_lowercase().as_str() {
                "pdf" => "pages/s",
                "dns" => "queries/s",
                "wal" => "msgs/s",
                "pubsub" => "deliveries/s",
                _ => "req/s",
            };
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
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">             <title>SubDollarBench Leaderboard Portal</title>             <style>{css}</style></head><body><main>             <div style=\"display:flex;justify-content:space-between;align-items:flex-start;flex-wrap:wrap;gap:12px;margin-bottom:1.2rem;\">               <div>                 <h1>🏆 SubDollarBench Leaderboard Portal</h1>                 <div class=\"sub\">Autonomous under-$1 LLM systems benchmark · Task-specific &amp; consolidated rankings · Strict 100% pass rate requirement · Calibrated reference-adjusted throughput (pts/¢)</div>               </div>               <div style=\"display:flex;flex-direction:column;align-items:flex-end;gap:6px;\">                 <div style=\"display:flex;align-items:center;gap:8px;\">                   <a href=\"/\" style=\"display:inline-flex;align-items:center;gap:5px;padding:6px 12px;border-radius:4px;background:rgba(88,166,255,0.12);color:var(--primary);text-decoration:none;font-weight:600;font-size:0.78rem;border:1px solid rgba(88,166,255,0.25);\">🚀 Benchmark Runner &amp; Console</a>                 </div>                 <div style=\"text-align:right;font-size:0.74rem;color:var(--mut);\">                   <div>Updated {now}{base_info}</div>                   <div style=\"margin-top:4px;\"><span class=\"badge badge-pass\">✔ {passed} Passed</span> <span class=\"badge badge-info\">📊 {n} Total</span></div>                 </div>               </div>             </div>             <div class=\"grid\">               <div class=\"card\"><div class=\"k\">Total Runs</div><div class=\"v\">{n}</div></div>               <div class=\"card\"><div class=\"k\">Passed (100%)</div><div class=\"v\" style=\"color:var(--grn);\">{passed}</div></div>               <div class=\"card\"><div class=\"k\">🌐 HTTP Server</div><div class=\"v\">{http}</div></div>               <div class=\"card\"><div class=\"k\">⚡ Redis Clone</div><div class=\"v\">{redis}</div></div>               <div class=\"card\"><div class=\"k\">📡 DNS Server</div><div class=\"v\">{dns}</div></div>               <div class=\"card\"><div class=\"k\">📄 PDF Microservice</div><div class=\"v\">{pdf}</div></div><div class=\"card\"><div class=\"k\">📓 WAL Broker</div><div class=\"v\">{wal}</div></div><div class=\"card\"><div class=\"k\">🔀 Reverse Proxy</div><div class=\"v\">{proxy}</div></div><div class=\"card\"><div class=\"k\">📢 Pub/Sub Broker</div><div class=\"v\">{pubsub}</div></div>             </div>             <div class=\"controls-bar\">               <div class=\"pills-group\">                 <button id=\"tabBtn_models\" class=\"btn-toggle active\" onclick=\"window.__subdollar_set_tab('models')\">🏅 Models</button>                                                                                                                                      </div>               <div style=\"display:flex;gap:10px;align-items:center;flex-wrap:wrap;\">                 <button id=\"formulaToggleBtn\" class=\"btn-formula\" onclick=\"window.__subdollar_toggle_formula()\">                   <span>🎛️ Customize Formula</span>                   <span id=\"formulaStatusBadge\" class=\"badge\" style=\"background:rgba(88,166,255,0.2);color:var(--acc);font-size:0.68rem;padding:1px 5px;display:none;\">Custom</span>                 </button>                 <label style=\"font-size:0.75rem;color:var(--mut);display:flex;align-items:center;gap:5px;cursor:pointer;\">                   <input type=\"checkbox\" onchange=\"window.__subdollar_pass_toggle(this.checked)\"> 100% Pass Only                 </label>                 <input type=\"text\" class=\"search-input\" placeholder=\"🔍 Search model or language...\" oninput=\"window.__subdollar_search(this.value)\">               </div>             </div>             <div id=\"formulaPanel\" class=\"formula-panel\" style=\"display:none;\">               <div class=\"formula-header\">                 <div>                   <div style=\"display:flex;align-items:center;gap:8px;\">                     <span style=\"font-size:1.1rem;\">🎛️</span>                     <span style=\"font-size:0.95rem;font-weight:700;color:#fff;\">Interactive Scoring Formula Customizer</span>                     <span class=\"badge badge-info\">Live Dynamic Re-scoring</span>                   </div>                   <div style=\"font-size:0.74rem;color:var(--mut);margin-top:3px;\">                     Adjust the relative importance of cost, throughput, duration, tokens, turns, and test conformance. Scores, medals, and KPI leaderboards update in real time.                   </div>                 </div>                 <div style=\"display:flex;align-items:center;gap:8px;\">                   <button class=\"btn-toggle\" onclick=\"window.__subdollar_reset_formula()\" style=\"border:1px solid var(--line);border-radius:4px;font-size:0.72rem;padding:4px 10px;\">🔄 Reset Defaults</button>                 </div>               </div>               <div style=\"font-size:0.72rem;font-weight:600;color:var(--mut);margin-bottom:6px;text-transform:uppercase;letter-spacing:0.5px;\">Quick Presets:</div>               <div class=\"formula-presets\">                 <button class=\"preset-pill active\" id=\"preset_default\" onclick=\"window.__subdollar_apply_preset('default')\">🎯 SubDollar Balanced (Default)</button>                 <button class=\"preset-pill\" id=\"preset_speed\" onclick=\"window.__subdollar_apply_preset('speed')\">⚡ Speed Demon (Throughput King)</button>                 <button class=\"preset-pill\" id=\"preset_frugal\" onclick=\"window.__subdollar_apply_preset('frugal')\">💰 Extreme Frugal (Cost &amp; Tokens)</button>                 <button class=\"preset-pill\" id=\"preset_agentic\" onclick=\"window.__subdollar_apply_preset('agentic')\">🤖 Agentic One-Shot (Turns &amp; Latency)</button>                 <button class=\"preset-pill\" id=\"preset_lenient\" onclick=\"window.__subdollar_apply_preset('lenient')\">🎓 Lenient Conformance (Partial Credit)</button>               </div>               <div class=\"formula-grid\">                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">💰 Cost Importance (W<sub>cost</sub>)</span>                     <span class=\"param-val\" id=\"val_wCost\">1.0×</span>                   </div>                   <div class=\"param-desc\">Cost sensitivity: <code>(100 / cost_¢)<sup>W</sup></code>. Higher values punish high-cost runs.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wCost\" min=\"0\" max=\"3\" step=\"0.1\" value=\"1.0\" oninput=\"window.__subdollar_weight_change('wCost', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">3.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">⚡ Throughput Importance (W<sub>tp</sub>)</span>                     <span class=\"param-val\" id=\"val_wTp\">1.0×</span>                   </div>                   <div class=\"param-desc\">Speed factor vs reference: <code>(speed_ratio·(1+2·speed)/3)<sup>W</sup></code>. 0 ignores speed.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wTp\" min=\"0\" max=\"3\" step=\"0.1\" value=\"1.0\" oninput=\"window.__subdollar_weight_change('wTp', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">3.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">⏱️ Duration / Latency (W<sub>dur</sub>)</span>                     <span class=\"param-val\" id=\"val_wDur\">0.0×</span>                   </div>                   <div class=\"param-desc\">Wall-clock reward vs 60s median: <code>(60s / duration)<sup>W</sup></code>. Rewards fast synthesis.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wDur\" min=\"0\" max=\"2\" step=\"0.1\" value=\"0.0\" oninput=\"window.__subdollar_weight_change('wDur', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">2.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">🪙 Token Economy (W<sub>tok</sub>)</span>                     <span class=\"param-val\" id=\"val_wTok\">0.0×</span>                   </div>                   <div class=\"param-desc\">Token frugality reward vs 50k median: <code>(50k / total_tokens)<sup>W</sup></code>. Rewards brevity.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wTok\" min=\"0\" max=\"2\" step=\"0.1\" value=\"0.0\" oninput=\"window.__subdollar_weight_change('wTok', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">2.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">🔄 Agent Turns (W<sub>turns</sub>)</span>                     <span class=\"param-val\" id=\"val_wTurns\">0.0×</span>                   </div>                   <div class=\"param-desc\">Turn efficiency reward vs 8 turns median: <code>(8 / turns)<sup>W</sup></code>. Rewards one-shot code.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wTurns\" min=\"0\" max=\"2\" step=\"0.1\" value=\"0.0\" oninput=\"window.__subdollar_weight_change('wTurns', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">2.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">🎯 Conformance Strictness</span>                     <span class=\"param-val\" id=\"val_strictPass\">Strict</span>                   </div>                   <div class=\"param-desc\">Choose whether 100% passing tests is required or partial pass rates earn partial score credit.</div>                   <div style=\"display:flex;gap:12px;margin-top:6px;font-size:0.74rem;\">                     <label style=\"display:flex;align-items:center;gap:5px;cursor:pointer;\">                       <input type=\"radio\" name=\"strictPassRadio\" id=\"radio_strict\" value=\"strict\" checked onchange=\"window.__subdollar_strict_change(true)\">                       <span><strong>Strict</strong> (100% or 0 pts)</span>                     </label>                     <label style=\"display:flex;align-items:center;gap:5px;cursor:pointer;\">                       <input type=\"radio\" name=\"strictPassRadio\" id=\"radio_lenient\" value=\"lenient\" onchange=\"window.__subdollar_strict_change(false)\">                       <span><strong>Lenient</strong> (Pass% &times; pts)</span>                     </label>                   </div>                 </div>               </div>               <div class=\"formula-math\">                 <span style=\"color:var(--gold);font-weight:700;\">Active Formula:</span>                 <span id=\"formulaMathDisplay\">Score = Conformance &times; (100 / Cost<sub>&cent;</sub>)<sup>1.0</sup> &times; (NormSpeed)<sup>1.0</sup> &times; 3.0</span>               </div>             </div>             <div id=\"portalDynamicContainer\">               <div style=\"margin-bottom:12px;padding:10px 14px;background:rgba(88,166,255,0.06);border:1px solid rgba(88,166,255,0.2);border-radius:6px;font-size:0.78rem;color:var(--mut);display:flex;align-items:center;gap:8px;\">                 <span>📐</span>                 <span><strong>Scoring Regime:</strong> Throughput scoring is normalized against measured reference baselines on this benchmark host: <strong>HTTP</strong> (Nginx: 33k req/s), <strong>Redis</strong> (Redis: 71k req/s), <strong>DNS</strong> (Reference UDP Resolver: 19k queries/s), <strong>PDF</strong> (Reference Extractor: 4.6k pages/s).</span>               </div>               <div class=\"wrap\">                 <table>                   <thead>                     <tr>                       <th style=\"width:50px;text-align:center;\">#</th>                       <th>Model &amp; Stack</th>                       <th>Conformance &amp; Effort</th>                       <th>Duration</th>                       <th style=\"text-align:center;\">Turns</th>                       <th>Throughput</th>                       <th>Cost &amp; Tokens</th>                       <th>Score pts/&cent;</th>                       <th>Action</th>                     </tr>                   </thead>                   <tbody>{initial_rows}</tbody>                 </table>               </div>             </div>             {methodology}<script>window.__SUBDOLLAR_RUNS__ = {runs_json};</script>             <script>{js}</script>             <footer class=\"muted\">SubDollarBench static portal · Click any row to inspect run logs and source code · Click a table header to sort.</footer>             </main></body></html>",
            css = PORTAL_CSS,
            js = PORTAL_JS,
            n = results.len(),
            passed = passed_n,
            http = http_n,
            redis = redis_n,
            dns = dns_n,
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
        let unit = match m.task.to_lowercase().as_str() {
            "pdf" => "pages/s",
            "dns" => "queries/s",
            "wal" => "msgs/s",
            "pubsub" => "deliveries/s",
            _ => "req/s",
        };
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
            scls = if m.status == "completed" { "pass" } else { "fail" },
            pcls = if passed_all { "pass" } else { "fail" },
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
            effective_cost_usd: effective,
            duration_seconds: None,
            turns: None,
            savings_percent: 0.0,
            efficiency_score: 0.0,
            throughput_score: None,
            timestamp: "2026-09-26T16:00:00".to_string(),
            method_version: None,
            rules_version: None,
            subruns: vec![],
            total_trials: Some(trials),
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: Some(pass),
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: stab.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_rollup_groups_by_model_across_tasks() {
        let parents = vec![
            rollup_parent("roll_a1", "model-a", "http", 100.0, 0.10, Some(0.10), 3, Some("stable")),
            // effective cost absent -> falls back to total_cost_usd.
            rollup_parent("roll_a2", "model-a", "redis", 50.0, 0.20, None, 3, Some("watch")),
            rollup_parent("roll_b1", "model-b", "http", 100.0, 0.05, Some(0.05), 1, None),
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
        assert!(html.contains("75%"));
        assert!(html.contains("P@1 (lo-hi)"));
        assert!(html.contains("Total API cost"));
        assert!(html.contains("Total effective cost"));
        assert!(html.contains("Avg API cost/run"));
        assert!(html.contains("Avg effective cost/run"));
        assert!(html.contains("<style>:root{color-scheme:dark;--bg:"));
        assert!(!html.contains("{{"));
        // Zero per-run detail: no run ids, no per-run links.
        for id in ["roll_a1", "roll_a2", "roll_b1"] {
            assert!(!html.contains(id), "run id {id} leaked into rollup");
        }
        assert!(!html.contains("runs/"));
    }

    #[test]
    fn test_export_rollup_writes_index_only() {
        let base = std::env::temp_dir().join(format!("test_portal_rollup_{}", std::process::id()));
        let results_dir = base.join("results");
        let out_dir = base.join("portal");
        fs::create_dir_all(&results_dir).unwrap();
        // Fixture meets the rollup coverage floor: 6 tasks x 3 trials = 18.
        let parents = vec![
            rollup_parent("roll_c1", "model-c", "http", 100.0, 0.10, Some(0.10), 3, Some("stable")),
            rollup_parent("roll_c2", "model-c", "wal", 100.0, 0.10, Some(0.10), 3, Some("stable")),
            rollup_parent("roll_c3", "model-c", "redis", 100.0, 0.10, Some(0.10), 3, Some("stable")),
            rollup_parent("roll_c4", "model-c", "dns", 100.0, 0.10, Some(0.10), 3, Some("stable")),
            rollup_parent("roll_c5", "model-c", "pdf", 100.0, 0.10, Some(0.10), 3, Some("stable")),
            rollup_parent("roll_c6", "model-c", "proxy", 100.0, 0.10, Some(0.10), 3, Some("stable")),
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
        let thin = rollup_parent("thin_1", "thin-model", "dns", 100.0, 0.05, Some(0.05), 8, None);
        // Covered model: 6 tasks x 3 trials = 18 trials.
        let mut parents = vec![thin];
        for (i, task) in ["http", "redis", "dns", "pdf", "wal", "proxy"].iter().enumerate() {
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
        let probe = rollup_parent("test_probe_9", "probe-model", "http", 100.0, 0.01, None, 3, None);
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

    #[test]
    fn test_freeze_filters_rollup_input() {
        let mut old = rollup_parent("old_1", "m", "http", 100.0, 0.01, None, 9, None);
        old.timestamp = "2026-09-16T14:54:59".to_string();
        let mut new = rollup_parent("new_1", "m", "http", 0.0, 0.01, None, 9, None);
        new.timestamp = "2026-09-26T19:45:16.109579".to_string();
        let fresh: Vec<_> = vec![old, new].into_iter().filter(|r| publishable(&r.timestamp)).collect();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, "new_1");
    }
}
