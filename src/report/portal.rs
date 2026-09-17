use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

use crate::report::archive::RunManifest;
use crate::report::leaderboard::{format_duration, LeaderboardManager};

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
  let currentTab = 'all';
  let searchQuery = '';
  let passOnly = false;
  let sortCol = 'score';
  let sortDir = -1;
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
    const passRate = (r.pass_rate !== undefined && r.pass_rate !== null) ? r.pass_rate : 0;
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
    const refTp = (r.reference_throughput_req_sec && r.reference_throughput_req_sec > 0) ? r.reference_throughput_req_sec : getBaseline(r.task);
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
    if (tab && tab !== 'all' && tab !== 'grouped') {
      list = list.filter(r => (r.task || '').toLowerCase() === tab.toLowerCase());
    }
    list.sort((a, b) => computeScore(b, DEFAULT_WEIGHTS) - computeScore(a, DEFAULT_WEIGHTS));
    const map = {};
    list.forEach((r, idx) => {
      map[r.id] = idx + 1;
    });
    return map;
  }

  function getSortedFilteredRuns(runs, tab, query, onlyPassed) {
    let filtered = runs.slice();
    if (tab && tab !== 'all' && tab !== 'grouped') {
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
      filtered = filtered.filter(r => (r.pass_rate || 0) >= 100);
    }

    filtered.sort((a, b) => {
      let vA, vB;
      if (sortCol === 'score') {
        vA = computeScore(a, currentWeights);
        vB = computeScore(b, currentWeights);
      } else if (sortCol === 'model') {
        vA = (a.model || '').toLowerCase();
        vB = (b.model || '').toLowerCase();
        return sortDir === 1 ? vA.localeCompare(vB) : vB.localeCompare(vA);
      } else if (sortCol === 'pass') {
        vA = a.pass_rate || 0;
        vB = b.pass_rate || 0;
      } else if (sortCol === 'dur') {
        vA = a.duration_seconds || 0;
        vB = b.duration_seconds || 0;
      } else if (sortCol === 'turns') {
        vA = a.turns || 0;
        vB = b.turns || 0;
      } else if (sortCol === 'tp') {
        vA = a.throughput_req_sec || 0;
        vB = b.throughput_req_sec || 0;
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
      const colSpan = showTaskCol ? 9 : 8;
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
      const apiDiff = (r.effective_cost_usd !== undefined && r.effective_cost_usd !== null && Math.abs(r.effective_cost_usd - r.total_cost_usd) > 0.0001)
        ? `<span style="font-size: 0.7rem; color: var(--mut);">(API: $${r.total_cost_usd.toFixed(4)})</span>`
        : '';
      const totalTokens = r.total_tokens || ((r.prompt_tokens || 0) + (r.cached_tokens || 0) + (r.completion_tokens || 0));
      const tokStr = `${(totalTokens / 1000).toFixed(1)}k tokens`;

      const curScore = computeScore(r, currentWeights);
      const defScore = computeScore(r, DEFAULT_WEIGHTS);
      
      let scoreSub = '';
      if (isCustom) {
        scoreSub = `<div style="font-size: 0.7rem; color: var(--mut);">default: ${defScore.toFixed(1)}</div>`;
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
              <span style="font-weight: 600; color: var(--gold);">$${effCost.toFixed(4)}</span>
              ${apiDiff}
            </div>
            <span style="font-size: 0.72rem; color: var(--mut);">${tokStr}</span>
          </div>
        </td>
        <td class="score">
          <div style="font-size: 0.95rem; font-weight: 700;">${curScore.toFixed(1)} <small style="font-size: 0.72rem; color: var(--mut);">pts/¢</small></div>
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
    const topRun = passRuns.slice().sort((a, b) => computeScore(b, currentWeights) - computeScore(a, currentWeights))[0];

    const peakTpRun = passRuns.slice().sort((a, b) => (b.throughput_req_sec || 0) - (a.throughput_req_sec || 0))[0];
    const lowestCostRun = passRuns.slice().sort((a, b) => {
      const cA = a.effective_cost_usd !== undefined && a.effective_cost_usd !== null ? a.effective_cost_usd : a.total_cost_usd;
      const cB = b.effective_cost_usd !== undefined && b.effective_cost_usd !== null ? b.effective_cost_usd : b.total_cost_usd;
      return cA - cB;
    })[0];

    const topModel = topRun ? shortModelName(topRun.model) : 'None yet';
    const topScore = topRun ? (computeScore(topRun, currentWeights).toFixed(1) + ' pts/¢') : '-';
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
              <th onclick="window.__subdollar_sort('cost')">Cost & Tokens</th>
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
        <span><strong>Scoring Regime:</strong> Throughput scoring is normalized against measured reference baselines on this benchmark host: <strong>HTTP</strong> (Nginx: 33k req/s), <strong>Redis</strong> (Redis: 71k req/s), <strong>DNS</strong> (Reference UDP Resolver: 19k queries/s), <strong>PDF</strong> (Reference Extractor: 4.6k pages/s).</span>
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

  function updateLeaderboardView() {
    syncFormulaControls();

    const container = document.getElementById('portalDynamicContainer');
    if (!container) return;

    ['all', 'http', 'redis', 'dns', 'pdf', 'grouped'].forEach(t => {
      const btn = document.getElementById('tabBtn_' + t);
      if (btn) {
        if (t === currentTab) btn.classList.add('active');
        else btn.classList.remove('active');
      }
    });

    if (currentTab === 'grouped') {
      const httpRuns = getSortedFilteredRuns(RUNS_DATA, 'http', searchQuery, passOnly);
      const redisRuns = getSortedFilteredRuns(RUNS_DATA, 'redis', searchQuery, passOnly);
      const dnsRuns = getSortedFilteredRuns(RUNS_DATA, 'dns', searchQuery, passOnly);
      const pdfRuns = getSortedFilteredRuns(RUNS_DATA, 'pdf', searchQuery, passOnly);
      const allRuns = getSortedFilteredRuns(RUNS_DATA, 'all', searchQuery, passOnly);

      const defHttp = computeDefaultRanks(RUNS_DATA, 'http');
      const defRedis = computeDefaultRanks(RUNS_DATA, 'redis');
      const defDns = computeDefaultRanks(RUNS_DATA, 'dns');
      const defPdf = computeDefaultRanks(RUNS_DATA, 'pdf');
      const defAll = computeDefaultRanks(RUNS_DATA, 'all');

      container.innerHTML = `
        ${renderCalibrationNote()}
        <div style="display: flex; flex-direction: column; gap: 16px;">
          <div>
            ${renderTaskBanner('http', RUNS_DATA)}
            ${renderTableContainer(httpRuns, false, '🌐 Task 2: HTTP/1.1 Web Server Rankings', 'RFC 7230/7231 Conformance', defHttp)}
          </div>
          <div>
            ${renderTaskBanner('redis', RUNS_DATA)}
            ${renderTableContainer(redisRuns, false, '⚡ Task 1: Redis Server Rankings', 'RESP2 Protocol Conformance', defRedis)}
          </div>
          <div>
            ${renderTaskBanner('dns', RUNS_DATA)}
            ${renderTableContainer(dnsRuns, false, '📡 Task 3: DNS Server Rankings', 'RFC 1035 UDP 5353 Conformance', defDns)}
          </div>
          <div>
            ${renderTaskBanner('pdf', RUNS_DATA)}
            ${renderTableContainer(pdfRuns, false, '📄 Task 4: PDF Microservice Rankings', 'Port 8090 Raw Parser Conformance', defPdf)}
          </div>
          <div>
            ${renderTableContainer(allRuns, true, '🏆 Consolidated Global Leaderboard (All Runs)', 'Global multi-task ranking', defAll)}
          </div>
        </div>
      `;
    } else if (currentTab === 'all') {
      const runs = getSortedFilteredRuns(RUNS_DATA, 'all', searchQuery, passOnly);
      const defAll = computeDefaultRanks(RUNS_DATA, 'all');
      container.innerHTML = `
        ${renderCalibrationNote()}
        ${renderTableContainer(runs, true, '', '', defAll)}
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
        let visible: Vec<_> = results.into_iter().filter(|r| !is_hidden(runs_path, &r.id)).collect();
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

    fn secret_patterns() -> Vec<Regex> {
        [
            r"sk-or-v1-[A-Za-z0-9_-]+",
            r"sk-proj-[A-Za-z0-9_-]+",
            r"sk-ant-[A-Za-z0-9_-]+",
            r"hf_[A-Za-z0-9_-]{16,}",
            r"(?i)bearer\s+[A-Za-z0-9_\-\.~\+/=]+",
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

    pub fn get_model_meta(model: &str) -> (&'static str, &'static str, &'static str, &'static str, &'static str) {
        let m = model.to_lowercase();
        if m.contains("claude") || m.contains("anthropic") {
            ("Anthropic", "✦", "#f27244", "rgba(242, 114, 68, 0.14)", "rgba(242, 114, 68, 0.35)")
        } else if m.contains("gpt") || m.contains("openai") || m.contains("o1") || m.contains("o3") || m.contains("o4") {
            ("OpenAI", "✳️", "#10a37f", "rgba(16, 163, 127, 0.14)", "rgba(16, 163, 127, 0.35)")
        } else if m.contains("gemini") || m.contains("google") {
            ("Google", "✨", "#4285f4", "rgba(66, 133, 244, 0.14)", "rgba(66, 133, 244, 0.35)")
        } else if m.contains("deepseek") {
            ("DeepSeek", "🐋", "#0084ff", "rgba(0, 132, 255, 0.14)", "rgba(0, 132, 255, 0.35)")
        } else if m.contains("llama") || m.contains("meta") {
            ("Meta", "🦙", "#8b5cf6", "rgba(139, 92, 246, 0.14)", "rgba(139, 92, 246, 0.35)")
        } else if m.contains("qwen") {
            ("Qwen", "🔮", "#a855f7", "rgba(168, 85, 247, 0.14)", "rgba(168, 85, 247, 0.35)")
        } else if m.contains("mistral") || m.contains("codestral") || m.contains("pixtral") {
            ("Mistral", "🌪️", "#ff7000", "rgba(255, 112, 0, 0.14)", "rgba(255, 112, 0, 0.35)")
        } else if m.contains("minimax") {
            ("MiniMax", "⚡", "#e11d48", "rgba(225, 29, 72, 0.14)", "rgba(225, 29, 72, 0.35)")
        } else if m.contains("nova") || m.contains("amazon") {
            ("Amazon", "☀️", "#f59e0b", "rgba(245, 158, 11, 0.14)", "rgba(245, 158, 11, 0.35)")
        } else if m.contains("cohere") || m.contains("command") {
            ("Cohere", "🌿", "#14b8a6", "rgba(20, 184, 166, 0.14)", "rgba(20, 184, 166, 0.35)")
        } else if m.contains("local") || m.contains("ollama") || m.contains("llama.cpp") || m.contains("vllm") {
            ("Local", "💻", "#94a3b8", "rgba(148, 163, 184, 0.14)", "rgba(148, 163, 184, 0.35)")
        } else {
            let colors = [
                ("Other", "🤖", "#06b6d4", "rgba(6, 182, 212, 0.14)", "rgba(6, 182, 212, 0.35)"),
                ("Other", "🤖", "#ec4899", "rgba(236, 72, 153, 0.14)", "rgba(236, 72, 153, 0.35)"),
                ("Other", "🤖", "#84cc16", "rgba(132, 204, 22, 0.14)", "rgba(132, 204, 22, 0.35)"),
                ("Other", "🤖", "#3b82f6", "rgba(59, 130, 246, 0.14)", "rgba(59, 130, 246, 0.35)"),
                ("Other", "🤖", "#f43f5e", "rgba(244, 63, 94, 0.14)", "rgba(244, 63, 94, 0.35)"),
                ("Other", "🤖", "#6366f1", "rgba(99, 102, 241, 0.14)", "rgba(99, 102, 241, 0.35)"),
                ("Other", "🤖", "#10b981", "rgba(16, 185, 129, 0.14)", "rgba(16, 185, 129, 0.35)"),
                ("Other", "🤖", "#f59e0b", "rgba(245, 158, 11, 0.14)", "rgba(245, 158, 11, 0.35)"),
            ];
            let hash: usize = m.bytes().fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
            colors[hash % colors.len()]
        }
    }

    fn render_index(results: &[crate::report::leaderboard::BenchmarkRunResult], base_url: &str) -> String {
        let http_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("http")).count();
        let redis_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("redis")).count();
        let dns_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("dns")).count();
        let pdf_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("pdf")).count();
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
            let eff_c = r.effective_cost_usd.unwrap_or(r.total_cost_usd);
            let dur_s = r.duration_seconds.unwrap_or(0.0);
            let dur_txt = format_duration(dur_s);
            let tok_n = if r.total_tokens > 0 { r.total_tokens } else { r.prompt_tokens + r.cached_tokens + r.completion_tokens };
            let tok_txt = format!("{:.1}k tokens", tok_n as f64 / 1000.0);
            let pass_badge = if r.pass_rate >= 100.0 {
                format!("<span class=\"badge badge-pass\">✔ 100% ({}/{})</span>", r.passed_stages, r.total_stages)
            } else {
                format!("<span class=\"badge badge-fail\">✕ {:.0}% ({}/{})</span>", r.pass_rate, r.passed_stages, r.total_stages)
            };
            let turns_str = r.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string());

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
                    let s_tp = sr.throughput_req_sec.map(|v| format!("{:.0} {}", v, unit)).unwrap_or_else(|| "N/A".to_string());
                    let s_cost = sr.effective_cost_usd.unwrap_or(sr.total_cost_usd);
                    let s_dur = format_duration(sr.duration_seconds.unwrap_or(0.0));
                    let s_turns = sr.turns.map(|t| format!("{}t", t)).unwrap_or_else(|| "-".to_string());
                    subruns_html.push_str(&format!(
                        "<div style=\"display:flex;justify-content:space-between;align-items:center;gap:8px;border-bottom:1px dashed rgba(255,255,255,0.05);padding-bottom:2px;\"><span><strong>Trial {}:</strong> {} · ${:.4} · {} · {}</span><a href=\"runs/{}/\" style=\"color:var(--primary);text-decoration:underline;\">Inspect</a></div>",
                        s_idx + 1, s_tp, s_cost, s_dur, s_turns, Self::esc(&sr.id)
                    ));
                }
                subruns_html.push_str("</div></details>");
            }

            let (m_fam, m_icon, m_color, m_bg, m_border) = Self::get_model_meta(&r.model);
            initial_rows.push_str(&format!(
                "<tr data-href=\"runs/{}/\" class=\"leaderboard-row\" style=\"border-left: 4px solid {}; background: linear-gradient(90deg, {} 0%, rgba(18, 23, 36, 0.6) 12%, transparent 100%);\"><td style=\"text-align:center;\">{}</td>                 <td><div style=\"display:flex;flex-direction:column;gap:3px;\"><div style=\"display:flex;gap:6px;align-items:center;flex-wrap:wrap;\"><div class=\"model-pill\" style=\"background:{};border:1px solid {};\"><span class=\"model-icon\" title=\"{}\">{}</span><a href=\"runs/{}/\" style=\"font-weight:600;color:{};text-decoration:none;\" title=\"{}\">{}</a></div> <span class=\"badge badge-info\">{}</span>{} <span class=\"badge\" style=\"background:rgba(63,185,80,0.15);color:var(--grn);\">{}</span></div><div style=\"font-size:0.72rem;color:var(--mut);\">{}</div>{}</div></td>                 <td><div style=\"display:flex;flex-direction:column;gap:3px;\"><div>{}</div>{}<div><span class=\"badge badge-effort\">effort: {}</span></div></div></td>                 <td><span style=\"font-weight:500;\">{}</span></td>                 <td style=\"text-align:center;\"><span class=\"badge\" style=\"background:rgba(88,166,255,0.15);color:var(--acc);font-weight:600;\">{}</span></td>                 <td><span style=\"font-weight:600;\">{}</span></td>                 <td><div style=\"display:flex;flex-direction:column;gap:2px;\"><div><span style=\"font-weight:600;color:var(--gold);\">${:.4}</span></div><span style=\"font-size:0.72rem;color:var(--mut);\">{}</span></div></td>                 <td class=\"score\"><div style=\"font-size:0.95rem;font-weight:700;\">{:.1} <small style=\"font-size:0.72rem;color:var(--mut);\">pts/¢</small></div></td>                 <td><a href=\"runs/{}/\" class=\"badge badge-info\" style=\"text-decoration:none;padding:4px 8px;\">Inspect &rarr;</a></td></tr>\n",
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
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">             <title>SubDollarBench Leaderboard Portal</title>             <style>{css}</style></head><body><main>             <div style=\"display:flex;justify-content:space-between;align-items:flex-start;flex-wrap:wrap;gap:12px;margin-bottom:1.2rem;\">               <div>                 <h1>🏆 SubDollarBench Leaderboard Portal</h1>                 <div class=\"sub\">Autonomous under-$1 LLM systems benchmark · Task-specific &amp; consolidated rankings · Strict 100% pass rate requirement · Calibrated reference-adjusted throughput (pts/¢)</div>               </div>               <div style=\"display:flex;flex-direction:column;align-items:flex-end;gap:6px;\">                 <div style=\"display:flex;align-items:center;gap:8px;\">                   <a href=\"/\" style=\"display:inline-flex;align-items:center;gap:5px;padding:6px 12px;border-radius:4px;background:rgba(88,166,255,0.12);color:var(--primary);text-decoration:none;font-weight:600;font-size:0.78rem;border:1px solid rgba(88,166,255,0.25);\">🚀 Benchmark Runner &amp; Console</a>                 </div>                 <div style=\"text-align:right;font-size:0.74rem;color:var(--mut);\">                   <div>Updated {now}{base_info}</div>                   <div style=\"margin-top:4px;\"><span class=\"badge badge-pass\">✔ {passed} Passed</span> <span class=\"badge badge-info\">📊 {n} Total</span></div>                 </div>               </div>             </div>             <div class=\"grid\">               <div class=\"card\"><div class=\"k\">Total Runs</div><div class=\"v\">{n}</div></div>               <div class=\"card\"><div class=\"k\">Passed (100%)</div><div class=\"v\" style=\"color:var(--grn);\">{passed}</div></div>               <div class=\"card\"><div class=\"k\">🌐 HTTP Server</div><div class=\"v\">{http}</div></div>               <div class=\"card\"><div class=\"k\">⚡ Redis Clone</div><div class=\"v\">{redis}</div></div>               <div class=\"card\"><div class=\"k\">📡 DNS Server</div><div class=\"v\">{dns}</div></div>               <div class=\"card\"><div class=\"k\">📄 PDF Microservice</div><div class=\"v\">{pdf}</div></div>             </div>             <div class=\"controls-bar\">               <div class=\"pills-group\">                 <button id=\"tabBtn_all\" class=\"btn-toggle active\" onclick=\"window.__subdollar_set_tab('all')\">🌐 Consolidated (All Tasks)</button>                 <button id=\"tabBtn_http\" class=\"btn-toggle\" onclick=\"window.__subdollar_set_tab('http')\">🌐 HTTP/1.1 Server <span class=\"badge-count\">{http}</span></button>                 <button id=\"tabBtn_redis\" class=\"btn-toggle\" onclick=\"window.__subdollar_set_tab('redis')\">⚡ Redis Clone <span class=\"badge-count\">{redis}</span></button>                 <button id=\"tabBtn_dns\" class=\"btn-toggle\" onclick=\"window.__subdollar_set_tab('dns')\">📡 DNS Server <span class=\"badge-count\">{dns}</span></button>                 <button id=\"tabBtn_pdf\" class=\"btn-toggle\" onclick=\"window.__subdollar_set_tab('pdf')\">📄 PDF Microservice <span class=\"badge-count\">{pdf}</span></button>                 <button id=\"tabBtn_grouped\" class=\"btn-toggle\" onclick=\"window.__subdollar_set_tab('grouped')\">📑 Grouped View</button>               </div>               <div style=\"display:flex;gap:10px;align-items:center;flex-wrap:wrap;\">                 <button id=\"formulaToggleBtn\" class=\"btn-formula\" onclick=\"window.__subdollar_toggle_formula()\">                   <span>🎛️ Customize Formula</span>                   <span id=\"formulaStatusBadge\" class=\"badge\" style=\"background:rgba(88,166,255,0.2);color:var(--acc);font-size:0.68rem;padding:1px 5px;display:none;\">Custom</span>                 </button>                 <label style=\"font-size:0.75rem;color:var(--mut);display:flex;align-items:center;gap:5px;cursor:pointer;\">                   <input type=\"checkbox\" onchange=\"window.__subdollar_pass_toggle(this.checked)\"> 100% Pass Only                 </label>                 <input type=\"text\" class=\"search-input\" placeholder=\"🔍 Search model or language...\" oninput=\"window.__subdollar_search(this.value)\">               </div>             </div>             <div id=\"formulaPanel\" class=\"formula-panel\" style=\"display:none;\">               <div class=\"formula-header\">                 <div>                   <div style=\"display:flex;align-items:center;gap:8px;\">                     <span style=\"font-size:1.1rem;\">🎛️</span>                     <span style=\"font-size:0.95rem;font-weight:700;color:#fff;\">Interactive Scoring Formula Customizer</span>                     <span class=\"badge badge-info\">Live Dynamic Re-scoring</span>                   </div>                   <div style=\"font-size:0.74rem;color:var(--mut);margin-top:3px;\">                     Adjust the relative importance of cost, throughput, duration, tokens, turns, and test conformance. Scores, medals, and KPI leaderboards update in real time.                   </div>                 </div>                 <div style=\"display:flex;align-items:center;gap:8px;\">                   <button class=\"btn-toggle\" onclick=\"window.__subdollar_reset_formula()\" style=\"border:1px solid var(--line);border-radius:4px;font-size:0.72rem;padding:4px 10px;\">🔄 Reset Defaults</button>                 </div>               </div>               <div style=\"font-size:0.72rem;font-weight:600;color:var(--mut);margin-bottom:6px;text-transform:uppercase;letter-spacing:0.5px;\">Quick Presets:</div>               <div class=\"formula-presets\">                 <button class=\"preset-pill active\" id=\"preset_default\" onclick=\"window.__subdollar_apply_preset('default')\">🎯 SubDollar Balanced (Default)</button>                 <button class=\"preset-pill\" id=\"preset_speed\" onclick=\"window.__subdollar_apply_preset('speed')\">⚡ Speed Demon (Throughput King)</button>                 <button class=\"preset-pill\" id=\"preset_frugal\" onclick=\"window.__subdollar_apply_preset('frugal')\">💰 Extreme Frugal (Cost &amp; Tokens)</button>                 <button class=\"preset-pill\" id=\"preset_agentic\" onclick=\"window.__subdollar_apply_preset('agentic')\">🤖 Agentic One-Shot (Turns &amp; Latency)</button>                 <button class=\"preset-pill\" id=\"preset_lenient\" onclick=\"window.__subdollar_apply_preset('lenient')\">🎓 Lenient Conformance (Partial Credit)</button>               </div>               <div class=\"formula-grid\">                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">💰 Cost Importance (W<sub>cost</sub>)</span>                     <span class=\"param-val\" id=\"val_wCost\">1.0×</span>                   </div>                   <div class=\"param-desc\">Cost sensitivity: <code>(100 / cost_¢)<sup>W</sup></code>. Higher values punish high-cost runs.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wCost\" min=\"0\" max=\"3\" step=\"0.1\" value=\"1.0\" oninput=\"window.__subdollar_weight_change('wCost', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">3.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">⚡ Throughput Importance (W<sub>tp</sub>)</span>                     <span class=\"param-val\" id=\"val_wTp\">1.0×</span>                   </div>                   <div class=\"param-desc\">Speed factor vs reference: <code>(speed_ratio·(1+2·speed)/3)<sup>W</sup></code>. 0 ignores speed.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wTp\" min=\"0\" max=\"3\" step=\"0.1\" value=\"1.0\" oninput=\"window.__subdollar_weight_change('wTp', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">3.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">⏱️ Duration / Latency (W<sub>dur</sub>)</span>                     <span class=\"param-val\" id=\"val_wDur\">0.0×</span>                   </div>                   <div class=\"param-desc\">Wall-clock reward vs 60s median: <code>(60s / duration)<sup>W</sup></code>. Rewards fast synthesis.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wDur\" min=\"0\" max=\"2\" step=\"0.1\" value=\"0.0\" oninput=\"window.__subdollar_weight_change('wDur', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">2.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">🪙 Token Economy (W<sub>tok</sub>)</span>                     <span class=\"param-val\" id=\"val_wTok\">0.0×</span>                   </div>                   <div class=\"param-desc\">Token frugality reward vs 50k median: <code>(50k / total_tokens)<sup>W</sup></code>. Rewards brevity.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wTok\" min=\"0\" max=\"2\" step=\"0.1\" value=\"0.0\" oninput=\"window.__subdollar_weight_change('wTok', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">2.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">🔄 Agent Turns (W<sub>turns</sub>)</span>                     <span class=\"param-val\" id=\"val_wTurns\">0.0×</span>                   </div>                   <div class=\"param-desc\">Turn efficiency reward vs 8 turns median: <code>(8 / turns)<sup>W</sup></code>. Rewards one-shot code.</div>                   <div class=\"slider-row\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">0.0</span>                     <input type=\"range\" class=\"formula-slider\" id=\"slider_wTurns\" min=\"0\" max=\"2\" step=\"0.1\" value=\"0.0\" oninput=\"window.__subdollar_weight_change('wTurns', this.value)\">                     <span style=\"font-size:0.68rem;color:var(--mut);\">2.0</span>                   </div>                 </div>                 <div class=\"param-card\">                   <div class=\"param-header\">                     <span class=\"param-title\">🎯 Conformance Strictness</span>                     <span class=\"param-val\" id=\"val_strictPass\">Strict</span>                   </div>                   <div class=\"param-desc\">Choose whether 100% passing tests is required or partial pass rates earn partial score credit.</div>                   <div style=\"display:flex;gap:12px;margin-top:6px;font-size:0.74rem;\">                     <label style=\"display:flex;align-items:center;gap:5px;cursor:pointer;\">                       <input type=\"radio\" name=\"strictPassRadio\" id=\"radio_strict\" value=\"strict\" checked onchange=\"window.__subdollar_strict_change(true)\">                       <span><strong>Strict</strong> (100% or 0 pts)</span>                     </label>                     <label style=\"display:flex;align-items:center;gap:5px;cursor:pointer;\">                       <input type=\"radio\" name=\"strictPassRadio\" id=\"radio_lenient\" value=\"lenient\" onchange=\"window.__subdollar_strict_change(false)\">                       <span><strong>Lenient</strong> (Pass% &times; pts)</span>                     </label>                   </div>                 </div>               </div>               <div class=\"formula-math\">                 <span style=\"color:var(--gold);font-weight:700;\">Active Formula:</span>                 <span id=\"formulaMathDisplay\">Score = Conformance &times; (100 / Cost<sub>&cent;</sub>)<sup>1.0</sup> &times; (NormSpeed)<sup>1.0</sup> &times; 3.0</span>               </div>             </div>             <div id=\"portalDynamicContainer\">               <div style=\"margin-bottom:12px;padding:10px 14px;background:rgba(88,166,255,0.06);border:1px solid rgba(88,166,255,0.2);border-radius:6px;font-size:0.78rem;color:var(--mut);display:flex;align-items:center;gap:8px;\">                 <span>📐</span>                 <span><strong>Scoring Regime:</strong> Throughput scoring is normalized against measured reference baselines on this benchmark host: <strong>HTTP</strong> (Nginx: 33k req/s), <strong>Redis</strong> (Redis: 71k req/s), <strong>DNS</strong> (Reference UDP Resolver: 19k queries/s), <strong>PDF</strong> (Reference Extractor: 4.6k pages/s).</span>               </div>               <div class=\"wrap\">                 <table>                   <thead>                     <tr>                       <th style=\"width:50px;text-align:center;\">#</th>                       <th>Model &amp; Stack</th>                       <th>Conformance &amp; Effort</th>                       <th>Duration</th>                       <th style=\"text-align:center;\">Turns</th>                       <th>Throughput</th>                       <th>Cost &amp; Tokens</th>                       <th>Score pts/&cent;</th>                       <th>Action</th>                     </tr>                   </thead>                   <tbody>{initial_rows}</tbody>                 </table>               </div>             </div>             <script>window.__SUBDOLLAR_RUNS__ = {runs_json};</script>             <script>{js}</script>             <footer class=\"muted\">SubDollarBench static portal · Click any row to inspect run logs and source code · Click a table header to sort.</footer>             </main></body></html>",
            css = PORTAL_CSS,
            js = PORTAL_JS,
            n = results.len(),
            passed = passed_n,
            http = http_n,
            redis = redis_n,
            dns = dns_n,
            pdf = pdf_n,
            now = now,
            base_info = base_info,
            initial_rows = initial_rows,
            runs_json = runs_json,
        )
    }

    fn export_run(
        runs_dir: &Path,
        runs_out: &Path,
        run_id: &str,
        secrets: &[Regex],
    ) -> Result<()> {
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

        for name in ["manifest.json", "env.json", "console.log", "README.md", "review.json"] {
            if let Ok(content) = fs::read_to_string(src.join(name)) {
                fs::write(dst.join(name), Self::redact(&content, secrets))?;
            }
        }

        Self::copy_workspace(&src.join("workspace"), &dst.join("workspace"), secrets)?;

        let body = match &manifest {
            Some(m) => Self::render_run(m, &dst, secrets)?,
            None => format!(
                "<h1>{}</h1><p>Manifest missing.</p>",
                Self::esc(clean)
            )
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
                        format!("<tr><td>{}</td><td>{}</td></tr>\n", Self::esc(k), Self::esc(&val))
                    })
                    .collect::<String>()
            }
            _ => "<tr><td colspan=\"2\">N/A</td></tr>\n".to_string(),
        };
        let log_html = fs::read_to_string(dst.join("console.log"))
            .map(|c| format!("<details><summary>console.log (full trace)</summary><pre>{}</pre></details>", Self::esc(&Self::redact(&c, secrets))))
            .unwrap_or_default();
        let readme_html = fs::read_to_string(dst.join("README.md"))
            .map(|c| format!("<details><summary>README.md</summary><pre>{}</pre></details>", Self::esc(&c)))
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
                    format!("{:.0} {} ({:.0}% of reference)", tp, unit, tp / ref_tp * 100.0),
                    format!("<div class=\"bar\"><div class=\"fill\" style=\"width:{}%\"></div></div>", pct),
                )
            }
            (Some(tp), _) => (format!("{:.0} {}", tp, unit), String::new()),
            _ => ("N/A".to_string(), String::new()),
        };
        let total_tok = m.tokens.total_tokens;
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
             <span class=\"pill info\">{lang}</span></div></div>\
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

    fn render_workspace(dir: &Path, prefix: &str, list: &mut String, inline: &mut String) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        entries.sort_by_key(|a| a.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" || name == "node_modules" || name == "__pycache__" {
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
                list.push_str(&format!("<li>{}/<ul>{}</ul></li>\n", Self::esc(&name), sub_list));
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
            if name.starts_with('.') || name == "target" || name == "node_modules" || name == "__pycache__" {
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

        fs::write(run_dir.join("review.json"), "{\"good\": [\"g1\"], \"bad\": [\"b1\"], \"conclusion\": \"done well\"}").unwrap();
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
}
