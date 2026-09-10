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

const PORTAL_CSS: &str = ":root{color-scheme:dark;--bg:#0b0e14;--panel:#121724;--line:#232c3d;--txt:#e8edf4;--mut:#8b94a3;--acc:#58a5ff;--grn:#3fb950;--red:#f85149}*{box-sizing:border-box}body{margin:0;background:radial-gradient(1200px 400px at 50% -10%,#16233d 0%,var(--bg) 60%);color:var(--txt);font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;line-height:1.55}main{max-width:1180px;margin:0 auto;padding:2.5rem 1.25rem 4rem}h1{font-size:2rem;margin:0;letter-spacing:-.02em;background:linear-gradient(90deg,#fff,#9ecbff);-webkit-background-clip:text;background-clip:text;color:transparent}.sub{color:var(--mut);margin:.4rem 0 1.6rem}a{color:var(--acc);text-decoration:none}a:hover{text-decoration:underline}.wrap{overflow-x:auto;background:var(--panel);border:1px solid var(--line);border-radius:14px;box-shadow:0 12px 40px rgba(0,0,0,.45)}table{border-collapse:collapse;width:100%;font-size:.86rem}thead th{position:sticky;top:0;background:#161d2c;cursor:pointer;user-select:none;white-space:nowrap;font-size:.75rem;text-transform:uppercase;letter-spacing:.06em;color:var(--mut);padding:10px 12px;text-align:left;border-bottom:1px solid var(--line)}thead th:hover{color:#fff}tbody td{padding:9px 12px;border-bottom:1px solid var(--line)}tbody tr:last-child td{border-bottom:none}tbody tr{transition:background .12s}tbody tr:hover{background:#182032}tbody tr:nth-child(even){background:rgba(255,255,255,.015)}code{font-size:.78rem;word-break:break-all;background:#0b0f18;border:1px solid var(--line);padding:2px 7px;border-radius:6px;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}.pill{display:inline-block;padding:2px 12px;border-radius:999px;font-weight:650;font-size:.76rem;white-space:nowrap}.pass{background:rgba(63,185,80,.14);color:var(--grn);border:1px solid rgba(63,185,80,.4)}.fail{background:rgba(248,81,73,.12);color:var(--red);border:1px solid rgba(248,81,73,.4)}.score{color:#79c0ff;font-weight:750;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}pre{background:#0b0f18;padding:1rem;overflow-x:auto;font-size:.78rem;border:1px solid var(--line);border-radius:10px}details{margin:1.2rem 0}summary{cursor:pointer;color:var(--acc)}.hero{margin:0 0 1.4rem}.hero h1{margin:.2rem 0 .6rem;word-break:break-word}.pills{display:flex;gap:8px;flex-wrap:wrap;margin:.4rem 0}.pill.info{background:rgba(88,165,255,.12);color:#58a5ff;border:1px solid rgba(88,165,255,.4)}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:10px;margin:1rem 0}.card{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:12px 14px}.card.wide{grid-column:1/-1}.card .k{font-size:.72rem;text-transform:uppercase;letter-spacing:.06em;color:var(--mut)}.card .v{font-size:1.15rem;font-weight:700}.card small{font-size:.75rem;font-weight:400;color:var(--mut)}.stages{display:flex;flex-direction:column;gap:8px}.stage{display:flex;gap:10px;align-items:flex-start;background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:10px 12px}.bar{height:8px;background:#0b0f18;border:1px solid var(--line);border-radius:99px;margin-top:8px;overflow:hidden}.fill{height:100%;background:linear-gradient(90deg,#1f6feb,#3fb950)}h2{margin-top:2rem}.muted{color:var(--mut)}footer{margin-top:2rem;color:var(--mut);font-size:.8rem}";

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
            r"(?i)bearer\s+[A-Za-z0-9_\-\.~\+/=]+",
            r"(?i)openrouter_api_key\s*=\s*\S+",
            r"--api-key\s+\S+",
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

    fn render_index(results: &[crate::report::leaderboard::BenchmarkRunResult], base_url: &str) -> String {
        let http_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("http")).count();
        let redis_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("redis")).count();
        let dns_n = results.iter().filter(|r| r.task.eq_ignore_ascii_case("dns")).count();
        let passed_n = results.iter().filter(|r| r.pass_rate >= 100.0).count();
        let now = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        let mut rows = String::new();
        for (i, r) in results.iter().enumerate() {
            let tp = r
                .throughput_req_sec
                .map(|v| format!("{:.0} req/s", v))
                .unwrap_or_else(|| "N/A".to_string());
            let score = r.throughput_score.unwrap_or(r.efficiency_score);
            let medal = match i + 1 { 1 => "🥇".to_string(), 2 => "🥈".to_string(), 3 => "🥉".to_string(), n => n.to_string() };
            let tp_v = r.throughput_req_sec.unwrap_or(-1.0);
            let eff_c = r.effective_cost_usd.unwrap_or(r.total_cost_usd);
            let dur_s = r.duration_seconds.unwrap_or(0.0);
            let dur_txt = format_duration(dur_s);
            let tok_n = r.prompt_tokens + r.cached_tokens + r.completion_tokens;
            let tok_txt = format!("{:.1}k", tok_n as f64 / 1000.0);
            let cls = if r.pass_rate >= 100.0 { "pass" } else { "fail" };
            rows.push_str(&format!(
                "<tr><td data-v=\"{}\">{}</td><td><a href=\"runs/{}/\">RUN</a></td><td><code>{}</code></td><td>{}</td><td>{}</td>\
                 <td data-v=\"{}\"><span class=\"pill {}\">{:.0}% ({}/{})</span></td><td data-v=\"{}\">{}</td><td data-v=\"{}\">${:.4}</td><td data-v=\"{}\">{}</td><td data-v=\"{}\">{}</td><td class=\"score\">{:.1}</td></tr>\n",
                (i + 1),
                medal,
                Self::esc(&r.id),
                Self::esc(&r.id),
                Self::esc(&r.model),
                Self::esc(&r.task),
                r.pass_rate,
                cls,
                r.pass_rate,
                r.passed_stages,
                r.total_stages,
                tp_v,
                tp,
                eff_c,
                eff_c,
                dur_s,
                dur_txt,
                tok_n,
                tok_txt,
                score,
            ));
        }
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>SubDollarBench Portal</title>\
             <style>{css}</style></head><body><main>\
             <h1>SubDollarBench Portal</h1>\
             <p>Autonomous under-$1 LLM systems benchmark. <span class=\"muted\">Updated {now}.</span>\
             {base}</p>\
             <div class=\"grid\">\
             <div class=\"card\"><div class=\"k\">Runs</div><div class=\"v\">{n}</div></div>\
             <div class=\"card\"><div class=\"k\">Passed</div><div class=\"v\" style=\"color:var(--grn)\">{passed}</div></div>\
             <div class=\"card\"><div class=\"k\">HTTP</div><div class=\"v\">{http}</div></div>\
             <div class=\"card\"><div class=\"k\">Redis</div><div class=\"v\">{redis}</div></div>\
             <div class=\"card\"><div class=\"k\">DNS</div><div class=\"v\">{dns}</div></div>\
             </div>\

             <div class=\"wrap\"><table><thead><tr><th>#</th><th>Run</th><th>ID</th><th>Model</th><th>Task</th>\
             <th>Pass</th><th>Throughput</th><th>Cost</th><th>Time</th><th>Tokens</th><th>Score pts/&cent;</th></tr></thead><tbody>{rows}</tbody></table></div><script>
(function(){{
var ths=document.querySelectorAll('thead th');
var dir=-1, col=0;
ths.forEach(function(th,idx){{
th.addEventListener('click',function(){{
var tb=document.querySelector('tbody');
var rows=Array.prototype.slice.call(tb.rows);
if(idx===col){{dir=-dir;}}else{{col=idx;dir=1;}}
rows.sort(function(a,b){{
var x=a.cells[idx].getAttribute('data-v');
var y=b.cells[idx].getAttribute('data-v');
if(x===null)x=a.cells[idx].textContent.trim();
if(y===null)y=b.cells[idx].textContent.trim();
var nx=parseFloat(x),ny=parseFloat(y);
if(x!==''&&y!==''&&!isNaN(nx)&&!isNaN(ny))return dir*(nx-ny);
x=String(x).toLowerCase();y=String(y).toLowerCase();
return dir*(x<y?-1:x>y?1:0);}});
rows.forEach(function(r){{tb.appendChild(r);}});
ths.forEach(function(h){{h.textContent=h.textContent.replace(/ [\\u25B2\\u25BC]/,'');}});
th.textContent+=(dir===1?' \\u25B2':' \\u25BC');}});}});
}})();
</script>
<footer class=\"muted\">Static mirror - click a column header to sort.</footer></main>\
             </body></html>",
            css = PORTAL_CSS,
            n = results.len(),
            passed = passed_n,
            http = http_n,
            redis = redis_n,
            dns = dns_n,
            now = now,
            base = if base_url.is_empty() {
                String::new()
            } else {
                format!("Base URL: {}", Self::esc(base_url))
            },
            rows = rows,
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

        for name in ["manifest.json", "env.json", "console.log", "README.md"] {
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
        let mut file_list = String::new();
        let mut inline_code = String::new();
        Self::render_workspace(&dst.join("workspace"), "", &mut file_list, &mut inline_code)?;

        let api = m.cost_usd;
        let eff = m.effective_cost_usd.unwrap_or(m.cost_usd);
        let comp = (eff - api).max(0.0);
        let score = m.throughput_score.unwrap_or(m.efficiency_score);
        let (tp_txt, tp_bar) = match (m.throughput_req_sec, m.reference_throughput_req_sec) {
            (Some(tp), Some(ref_tp)) if ref_tp > 0.0 => {
                let pct = ((tp / ref_tp * 100.0).min(100.0).max(0.0)) as u32;
                (
                    format!("{:.0} req/s ({:.0}% of reference)", tp, tp / ref_tp * 100.0),
                    format!("<div class=\"bar\"><div class=\"fill\" style=\"width:{}%\"></div></div>", pct),
                )
            }
            (Some(tp), _) => (format!("{:.0} req/s", tp), String::new()),
            _ => ("N/A".to_string(), String::new()),
        };
        let total_tok = m.tokens.prompt_tokens + m.tokens.cached_tokens + m.tokens.completion_tokens;
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
             <h2>Code</h2>{code}\
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
            tott = format!("{:.1}k", total_tok as f64 / 1000.0),
            pt = m.tokens.prompt_tokens,
            ct = m.tokens.cached_tokens,
            xt = m.tokens.completion_tokens,
            tp = tp_txt,
            tpbar = tp_bar,
            stages = stages,
            code = inline_code,
            files = file_list,
            readme = readme_html,
            env = env_rows,
            log = log_html,
        ))
    }

    fn render_workspace(dir: &Path, prefix: &str, list: &mut String, inline: &mut String) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
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
        let dirty = "export OPENROUTER_API_KEY=sk-or-v1-abc123XYZ here\nAuthorization: Bearer tok456\nrun --api-key sk-or-v1-other ok";
        let clean = PortalExporter::redact(dirty, &patterns);
        assert!(!clean.contains("abc123XYZ"));
        assert!(!clean.contains("tok456"));
        assert!(!clean.contains("sk-or-v1-other"));
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
                "prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1,
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
                "prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1,
                "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 50.0,
                "timestamp": "t"
            })
            .to_string(),
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
        let log = fs::read_to_string(out_dir.join("runs/demo_run_1/console.log")).unwrap();
        assert!(!log.contains("sk-or-v1-zzz"));

        let _ = fs::remove_dir_all(&base);
    }
}
