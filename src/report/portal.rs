use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

use crate::report::archive::RunManifest;
use crate::report::leaderboard::LeaderboardManager;

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

const PORTAL_CSS: &str = ":root{color-scheme:dark}body{font-family:system-ui,-apple-system,sans-serif;max-width:1200px;margin:0 auto;padding:2rem 1rem;background:#0d1117;color:#e6edf3;line-height:1.5}a{color:#58a5ff}h1{font-size:1.6rem}.muted{color:#8b949e}.wrap{overflow-x:auto}table{border-collapse:collapse;width:100%;font-size:.85rem}thead th{position:sticky;top:0;background:#161b22}td,th{border:1px solid #30363d;padding:7px 10px;text-align:left}tbody tr:nth-child(even){background:#11161d}code{font-size:.8rem;word-break:break-all;background:#161b22;padding:1px 5px;border-radius:4px}.pill{display:inline-block;padding:1px 10px;border-radius:999px;font-weight:600;font-size:.78rem;white-space:nowrap}.pass{background:#1a7f3733;color:#3fb950;border:1px solid #1a7f37}.fail{background:#da363333;color:#f85149;border:1px solid #da3633}.score{color:#79c0ff;font-weight:700;font-family:ui-monospace,monospace}pre{background:#161b22;padding:1rem;overflow-x:auto;font-size:.78rem;border:1px solid #30363d;border-radius:6px}details{margin:1rem 0}summary{cursor:pointer;color:#58a5ff}";

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
        let now = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        let mut rows = String::new();
        for (i, r) in results.iter().enumerate() {
            let tp = r
                .throughput_req_sec
                .map(|v| format!("{:.0} req/s", v))
                .unwrap_or_else(|| "N/A".to_string());
            let score = r.throughput_score.unwrap_or(r.efficiency_score);
            let medal = match i + 1 { 1 => "🥇".to_string(), 2 => "🥈".to_string(), 3 => "🥉".to_string(), n => n.to_string() };
            let cls = if r.pass_rate >= 100.0 { "pass" } else { "fail" };
            rows.push_str(&format!(
                "<tr><td>{}</td><td><a href=\"runs/{}/\">RUN</a></td><td><code>{}</code></td><td>{}</td><td>{}</td>\
                 <td><span class=\"pill {}\">{:.0}% ({}/{})</span></td><td>{}</td><td class=\"score\">{:.1}</td></tr>\n",
                medal,
                Self::esc(&r.id),
                Self::esc(&r.id),
                Self::esc(&r.model),
                Self::esc(&r.task),
                cls,
                r.pass_rate,
                r.passed_stages,
                r.total_stages,
                tp,
                score,
            ));
        }
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>SubDollarBench Portal</title>\
             <style>{css}</style></head><body>\
             <h1>SubDollarBench Portal</h1>\
             <p>Autonomous under-$1 LLM systems benchmark. {n} runs. <span class=\"muted\">Generated {now}.</span>\
             {base}</p>\
             <div class=\"wrap\"><table><thead><tr><th>#</th><th>Run</th><th>ID</th><th>Model</th><th>Task</th>\
             <th>Pass</th><th>Throughput</th><th>Score pts/&cent;</th></tr></thead><tbody>{rows}</tbody></table></div>\
             </body></html>",
            css = PORTAL_CSS,
            n = results.len(),
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

        let mut file_rows = String::new();
        Self::copy_workspace(&src.join("workspace"), &dst.join("workspace"), secrets, &mut file_rows)?;

        let body = match &manifest {
            Some(m) => Self::render_run(m, &file_rows, &dst, secrets)?,
            None => format!(
                "<h1>{}</h1><p>Manifest missing; raw files below.</p><ul>{}</ul>",
                Self::esc(clean),
                file_rows
            ),
        };
        fs::write(dst.join("index.html"), body)?;
        Ok(())
    }

    fn render_run(m: &RunManifest, file_rows: &str, dst: &Path, secrets: &[Regex]) -> Result<String> {
        let mut stages = String::new();
        for s in &m.stages {
            stages.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                s.stage,
                Self::esc(&s.name),
                if s.passed { "PASS" } else { "FAIL" },
                Self::esc(s.error.as_deref().unwrap_or("")),
            ));
        }
        let env_rows = match serde_json::to_value(&m.env) {
            Ok(serde_json::Value::Object(map)) => map
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(x) => x.clone(),
                        _ => v.to_string(),
                    };
                    format!("<tr><td>{}</td><td>{}</td></tr>\n", Self::esc(k), Self::esc(&val))
                })
                .collect::<String>(),
            _ => "<tr><td colspan=\"2\">N/A</td></tr>\n".to_string(),
        };
        let log_html = fs::read_to_string(dst.join("console.log"))
            .map(|c| format!("<details><summary>console.log</summary><pre>{}</pre></details>", Self::esc(&Self::redact(&c, secrets))))
            .unwrap_or_default();
        Ok(format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>{id}</title>\
             <style>{css}</style></head><body>\
             <p><a href=\"../../\">&larr; leaderboard</a></p><h1><code>{id}</code></h1>\
             <table>\
             <tr><th>Model</th><td>{model}</td></tr>\
             <tr><th>Task</th><td>{task}</td></tr>\
             <tr><th>Status</th><td>{status}</td></tr>\
             <tr><th>Language</th><td>{lang}</td></tr>\
             <tr><th>Pass rate</th><td>{pass:.1}% ({passed}/{total})</td></tr>\
             <tr><th>Duration</th><td>{dur:.1}s</td></tr>\
             <tr><th>Turns</th><td>{turns}</td></tr>\
             <tr><th>Cost</th><td>${cost:.4} (eff ${eff:.4})</td></tr>\
             <tr><th>Efficiency</th><td>{score:.1} pts/&cent;</td></tr>\
             <tr><th>Tokens</th><td>prompt {pt}, cached {ct}, completion {xt}</td></tr>\
             </table>\
             <h2>Stages</h2><table><tr><th>#</th><th>Name</th><th>Result</th><th>Error</th></tr>{stages}</table>\
             <h2>Environment</h2><table><tr><th>Key</th><th>Value</th></tr>{env}</table>\
             <h2>Files</h2><ul>{files}</ul>{log}</body></html>",
            css = PORTAL_CSS,
            id = Self::esc(&m.run_id),
            model = Self::esc(&m.model),
            task = Self::esc(&m.task),
            status = Self::esc(&m.status),
            lang = Self::esc(&m.language),
            pass = m.pass_rate,
            passed = m.passed_stages,
            total = m.total_stages,
            dur = m.duration_seconds,
            turns = m.turns.map(|t| t.to_string()).unwrap_or_else(|| "N/A".to_string()),
            cost = m.cost_usd,
            eff = m.effective_cost_usd.unwrap_or(m.cost_usd),
            score = m.throughput_score.unwrap_or(m.efficiency_score),
            pt = m.tokens.prompt_tokens,
            ct = m.tokens.cached_tokens,
            xt = m.tokens.completion_tokens,
            stages = stages,
            env = env_rows,
            files = file_rows,
            log = log_html,
        ))
    }

    fn copy_workspace(
        src: &Path,
        dst: &Path,
        secrets: &[Regex],
        rows: &mut String,
    ) -> Result<()> {
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
                let mut sub = String::new();
                Self::copy_workspace(&path, &dst.join(&name), secrets, &mut sub)?;
                rows.push_str(&format!("<li>{}/<ul>{}</ul></li>\n", Self::esc(&name), sub));
            } else if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size <= 512 * 1024 {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let _ = fs::write(dst.join(&name), Self::redact(&content, secrets));
                    } else {
                        let _ = fs::copy(&path, dst.join(&name));
                    }
                }
                rows.push_str(&format!(
                    "<li><a href=\"workspace/{}\">{} ({} bytes)</a></li>\n",
                    Self::esc(&name),
                    Self::esc(&name),
                    size
                ));
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
        assert!(index.contains("<style>:root{color-scheme:dark}body{"));
        assert!(!index.contains("{{"));
        let run_page = fs::read_to_string(out_dir.join("runs/demo_run_1/index.html")).unwrap();
        assert!(run_page.contains("demo_run_1"));
        let log = fs::read_to_string(out_dir.join("runs/demo_run_1/console.log")).unwrap();
        assert!(!log.contains("sk-or-v1-zzz"));

        let _ = fs::remove_dir_all(&base);
    }
}
