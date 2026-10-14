use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::report::archive::RunManifest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewJson {
    pub good: Vec<String>,
    pub bad: Vec<String>,
    pub conclusion: String,
}

pub const MAX_WORKSPACE_CHARS: usize = 24_000;
pub const MAX_PROMPT_CHARS: usize = 8_000;
const JUDGE_TIMEOUT_SECS: u64 = 120;

pub struct RunJudge;

impl RunJudge {
    pub fn validate(r: &ReviewJson) -> Result<()> {
        if r.conclusion.trim().is_empty() {
            anyhow::bail!("review conclusion is empty");
        }
        let good: Vec<&str> = r
            .good
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let bad: Vec<&str> = r
            .bad
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if good.is_empty() || bad.is_empty() {
            anyhow::bail!("review needs at least one strength and one weakness");
        }
        if good.len() + bad.len() != 9 {
            anyhow::bail!(
                "review must carry exactly 9 bullets plus conclusion, got {}+{}",
                good.len(),
                bad.len()
            );
        }
        for b in good.iter().chain(bad.iter()) {
            if b.len() > 500 {
                anyhow::bail!("review bullet exceeds 500 chars");
            }
        }
        Ok(())
    }

    pub fn extract_json(text: &str) -> Option<String> {
        let t = text.trim();
        let t = t
            .strip_prefix("```json")
            .or_else(|| t.strip_prefix("```"))
            .unwrap_or(t);
        let t = t.strip_suffix("```").unwrap_or(t);
        let start = t.find('{')?;
        let end = t.rfind('}')?;
        if end <= start {
            return None;
        }
        Some(t[start..=end].to_string())
    }

    pub fn collect_workspace(workdir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut budget = MAX_WORKSPACE_CHARS;
        Self::collect_dir(workdir, "", &mut out, &mut budget);
        out
    }

    fn collect_dir(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>, budget: &mut usize) {
        if *budget == 0 {
            return;
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        entries.sort_by_key(|a| a.file_name());
        for entry in entries {
            if *budget == 0 {
                return;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // *.log excluded: harness probe data (appendonly.log can be
            // 200KB+ and eats the 24k budget ahead of real source — Bunny's
            // report judged Dockerfile+README+truncated AOF, zero Go files).
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
                || name.ends_with(".log")
            {
                continue;
            }
            let path = entry.path();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            if path.is_dir() {
                Self::collect_dir(&path, &rel, out, budget);
            } else if path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let take = (*budget).min(content.len());
                    let boundary = content.floor_char_boundary(take);
                    let mut clipped = content[..boundary].to_string();
                    if content.len() > boundary {
                        clipped.push_str("\n...[truncated]");
                    }
                    *budget = budget.saturating_sub(boundary);
                    out.push((rel, clipped));
                }
            }
        }
    }

    pub fn build_prompt(
        task: &str,
        task_prompt: &str,
        files: &[(String, String)],
        metrics: &str,
    ) -> String {
        let mut p = String::new();
        p.push_str("You are an expert systems-engineering code reviewer. Review the candidate ");
        p.push_str(task);
        p.push_str(" server below against its task requirements and its measured results.\n");
        p.push_str("Judge ONLY this run on its own prompt and its own numbers. Weigh run economy (turns, tokens, cost, duration) alongside correctness and throughput. Never compare against other runs, models, or leaderboard positions.\n");
        p.push_str("Respond with a single JSON object and nothing else, exactly this shape:\n");
        p.push_str(
            "{\"good\": [\"...\", ...], \"bad\": [\"...\", ...], \"conclusion\": \"...\"}\n",
        );
        p.push_str("Rules: good plus bad must total exactly 9 bullets (any split, at least one of each), each bullet one or two sentences grounded in the code or the numbers, conclusion exactly three sentences. No markdown fences.\n\n");
        p.push_str("## Task requirements (excerpt)\n");
        p.push_str(
            &task_prompt
                .chars()
                .take(MAX_PROMPT_CHARS)
                .collect::<String>(),
        );
        p.push_str("\n\n## Measured results\n");
        p.push_str(metrics);
        p.push_str("\n\n## Candidate files\n");
        for (name, content) in files {
            p.push_str("\n--- ");
            p.push_str(name);
            p.push_str(" ---\n");
            p.push_str(content);
        }
        p
    }

    fn resolve_target(review_model: &str) -> Result<(String, String)> {
        let m = review_model.trim();
        if m.ends_with(":batch") {
            anyhow::bail!("batch API judges stall the run; use the interactive variant");
        }
        if m.is_empty() {
            anyhow::bail!("empty review model");
        }
        // Cloud-only: local judge backends (llama.cpp, Ollama) were removed.
        if m.starts_with("openrouter/") || !m.contains('/') {
            Ok((
                "https://openrouter.ai/api/v1/chat/completions".to_string(),
                m.trim_start_matches("openrouter/").to_string(),
            ))
        } else {
            anyhow::bail!("unsupported judge model {}: use openrouter/<id>", m)
        }
    }

    async fn call_once(
        client: &reqwest::Client,
        url: &str,
        key: Option<&str>,
        model: &str,
        messages: &[serde_json::Value],
        max_tokens: u32,
    ) -> Result<String> {
        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/lubod/subdollarbench")
            .header("X-Title", "subdollarbench-judge")
            .json(&serde_json::json!({
                "model": model,
                "messages": messages,
                "temperature": 0.2,
                "max_tokens": max_tokens,
            }));
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.context("judge request failed")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("judge response unreadable")?;
        if !status.is_success() {
            anyhow::bail!(
                "judge API error {}: {}",
                status,
                body.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
            );
        }
        let msg = &body["choices"][0]["message"];
        msg["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                msg["reasoning_content"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                msg["reasoning"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| anyhow::anyhow!("judge response had no content"))
    }

    pub async fn judge_run(
        api_key: Option<&str>,
        review_model: &str,
        task: &str,
        task_prompt: &str,
        workdir: &Path,
        manifest: &RunManifest,
    ) -> Result<ReviewJson> {
        let key = api_key
            .map(|k| k.to_string())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok());
        let (url, model) = Self::resolve_target(review_model)?;
        if url.contains("openrouter.ai") && key.is_none() {
            anyhow::bail!("judge needs an OpenRouter API key");
        }
        let files = Self::collect_workspace(workdir);
        if files.is_empty() {
            anyhow::bail!("no reviewable workspace files");
        }
        let metrics = serde_json::json!({
            "model": manifest.model,
            "task": manifest.task,
            "status": manifest.status,
            "pass_rate": manifest.pass_rate,
            "passed_stages": manifest.passed_stages,
            "total_stages": manifest.total_stages,
            "stages": manifest.stages.iter().map(|s| serde_json::json!({"name": s.name, "passed": s.passed, "error": s.error})).collect::<Vec<_>>(),
            "duration_seconds": manifest.duration_seconds,
            "turns": manifest.turns,
            "prompt_tokens": manifest.tokens.prompt_tokens,
            "cached_tokens": manifest.tokens.cached_tokens,
            "completion_tokens": manifest.tokens.completion_tokens,
            "total_tokens": manifest.tokens.total_tokens,
            "cost_usd": manifest.cost_usd,
            "effective_cost_usd": manifest.effective_cost_usd,
            "throughput_req_sec": manifest.throughput_req_sec,
            "reference_throughput_req_sec": manifest.reference_throughput_req_sec,
        })
        .to_string();
        let prompt = Self::build_prompt(task, task_prompt, &files, &metrics);
        let urls = vec![url];
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(JUDGE_TIMEOUT_SECS))
            .build()?;

        let mut last_err = anyhow::anyhow!("no judge endpoint attempted");
        for endpoint in &urls {
            match Self::attempt(&client, endpoint, key.as_deref(), &model, prompt.clone()).await {
                Ok(review) => return Ok(review),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    async fn attempt(
        client: &reqwest::Client,
        url: &str,
        key: Option<&str>,
        model: &str,
        prompt: String,
    ) -> Result<ReviewJson> {
        let max_tokens = if url.contains("127.0.0.1") {
            4000
        } else {
            2000
        };
        let system = serde_json::json!({"role": "system", "content": "You review one benchmark run against its own task prompt and measured results. Reply with exactly one JSON object, no prose, no fences."});
        let user = serde_json::json!({"role": "user", "content": prompt});
        let mut messages = vec![system, user];

        for attempt in 0..2 {
            let text = Self::call_once(client, url, key, model, &messages, max_tokens).await?;
            if let Some(raw) = Self::extract_json(&text) {
                match serde_json::from_str::<ReviewJson>(&raw) {
                    Ok(review) if Self::validate(&review).is_ok() => return Ok(review),
                    _ => {
                        messages.push(serde_json::json!({"role": "assistant", "content": text}));
                        messages.push(serde_json::json!({"role": "user", "content": "That output was invalid. Reply with ONLY the JSON object: good plus bad totalling exactly 9 bullets (at least one of each), each bullet grounded in the code or numbers, plus a three-sentence conclusion."}));
                    }
                }
            } else if attempt == 0 {
                messages.push(serde_json::json!({"role": "assistant", "content": text}));
                messages.push(serde_json::json!({"role": "user", "content": "No JSON object found. Reply with ONLY the JSON object."}));
            } else {
                break;
            }
        }
        anyhow::bail!("judge did not return a valid review after retry")
    }
    /// Whole-run synthesis: deterministic stats block (computed in code,
    /// never hallucinated) + one judge call reserved for code/architecture
    /// synthesis across the trial workspaces. Advisory-only, like trial
    /// reviews: never touches scores, no regime impact.
    pub fn run_stats_block(
        run_id: &str,
        model: &str,
        task: &str,
        trials: &[(String, f64, u32, u64, f64, f64)],
    ) -> String {
        Self::run_stats_block_with_failures(run_id, model, task, trials, &[])
    }

    /// Same, plus deterministic per-trial failure reasons
    /// (`failures[i]` = stage-error summary for `trials[i]`, empty when
    /// passing). Grounded in trial manifests, never inferred.
    pub fn run_stats_block_with_failures(
        run_id: &str,
        model: &str,
        task: &str,
        trials: &[(String, f64, u32, u64, f64, f64)],
        failures: &[String],
    ) -> String {
        // (trial_id, pass_rate, turns, tokens, cost_usd, duration_s)
        let n = trials.len().max(1) as f64;
        let passes = trials.iter().filter(|t| t.1 >= 100.0).count();
        let mean_pass: f64 = trials.iter().map(|t| t.1).sum::<f64>() / n;
        let mean_turns: f64 = trials.iter().map(|t| t.2 as f64).sum::<f64>() / n;
        let mean_tokens: f64 = trials.iter().map(|t| t.3 as f64).sum::<f64>() / n;
        let mean_cost: f64 = trials.iter().map(|t| t.4).sum::<f64>() / n;
        let mean_dur: f64 = trials.iter().map(|t| t.5).sum::<f64>() / n;
        let spread = trials
            .iter()
            .map(|t| t.1)
            .fold((100.0_f64, 0.0_f64), |(lo, hi), p| (lo.min(p), hi.max(p)));
        let mut s = format!(
            "RUN {run_id} model={model} task={task} trials={} passed={}/{} mean_pass={:.1}% (range {:.0}-{:.0}) mean_turns={:.0} mean_tokens={:.0} mean_cost=${:.4} mean_dur={:.0}s\n",
            trials.len(),
            passes,
            trials.len(),
            mean_pass,
            spread.0,
            spread.1,
            mean_turns,
            mean_tokens,
            mean_cost,
            mean_dur,
        );
        for (i, t) in trials.iter().enumerate() {
            s.push_str(&format!(
                "  trial {} pass={:.0}% turns={} tokens={} cost=${:.4} dur={:.0}s\n",
                t.0, t.1, t.2, t.3, t.4, t.5
            ));
            if let Some(f) = failures.get(i) {
                if !f.is_empty() {
                    s.push_str(&format!("    failures: {f}\n"));
                }
            }
        }
        s
    }

    /// Synthesis prompt: architecture + failure-mode review across trials.
    /// Numbers come from `run_stats_block` (deterministic); the judge reads
    /// code, not spreadsheets.
    pub fn build_synthesis_prompt(
        stats: &str,
        task_prompt: &str,
        trial_notes: &[(String, String)],
        best_files: &[(String, String)],
    ) -> String {
        let mut p = String::new();
        p.push_str("You are an expert systems-engineering reviewer synthesizing a multi-trial benchmark run. The statistics below are COMPUTED FACTS — do not recompute or restate numbers beyond naming them; focus your judgment on architecture, failure modes, and code quality across trials.\n");
        p.push_str("Respond with a single JSON object and nothing else, exactly this shape:\n");
        p.push_str(
            "{\"good\": [\"...\", ...], \"bad\": [\"...\", ...], \"conclusion\": \"...\"}\n",
        );
        p.push_str("Rules: good plus bad must total exactly 9 bullets (any split, at least one of each), each bullet one or two sentences grounded in the code or the trial reviews, conclusion exactly three sentences. No markdown fences.\n\n");
        p.push_str("## Run statistics (facts, do not recompute)\n");
        p.push_str(stats);
        p.push_str("\n## Task requirements (excerpt)\n");
        p.push_str(
            &task_prompt
                .chars()
                .take(MAX_PROMPT_CHARS)
                .collect::<String>(),
        );
        p.push_str("\n\n## Per-trial review conclusions\n");
        for (id, conclusion) in trial_notes {
            p.push_str(&format!("### {}\n{}\n", id, conclusion));
        }
        if !best_files.is_empty() {
            p.push_str(
                "\n## Canonical best-trial workspace (read the code, not just the reviews)\n",
            );
            for (name, content) in best_files {
                p.push_str(&format!("### {}\n{}\n", name, content));
            }
        }
        p
    }

    /// One judge call for whole-run synthesis. Same transport as trial
    /// reviews; returns the parsed ReviewJson.
    pub async fn judge_synthesis(
        api_key: Option<&str>,
        review_model: &str,
        prompt: String,
    ) -> Result<ReviewJson> {
        let key = api_key
            .map(|k| k.to_string())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok());
        let (url, model) = Self::resolve_target(review_model)?;
        if url.contains("openrouter.ai") && key.is_none() {
            anyhow::bail!("judge needs an OpenRouter API key");
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(JUDGE_TIMEOUT_SECS))
            .build()?;
        Self::attempt(&client, &url, key.as_deref(), &model, prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ReviewJson {
        ReviewJson {
            good: vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
            bad: vec![
                "h".to_string(),
                "d".to_string(),
                "g".to_string(),
                "e".to_string(),
                "f".to_string(),
            ],
            conclusion: "ok".to_string(),
        }
    }

    #[test]
    fn test_judge_validate_accepts_variable_split() {
        let mut r = sample();
        r.good.pop();
        r.bad.push("g".to_string());
        assert!(RunJudge::validate(&r).is_ok());
        let mut r = sample();
        r.bad.push("g".to_string());
        r.bad.push("h".to_string());
        r.good.pop();
        r.good.pop();
        assert!(RunJudge::validate(&r).is_ok());
    }

    #[test]
    fn test_judge_validate_rejects_bad_shapes() {
        let mut r = sample();
        r.conclusion.clear();
        assert!(RunJudge::validate(&r).is_err());
        let mut r = sample();
        r.bad.clear();
        assert!(RunJudge::validate(&r).is_err());
        let mut r = sample();
        r.good.push("extra".to_string());
        assert!(RunJudge::validate(&r).is_err());
        let mut r = sample();
        r.good[0].clear();
        assert!(RunJudge::validate(&r).is_err());
    }

    #[test]
    fn test_judge_extract_json_handles_fences() {
        let fenced = "```json\n{\"good\": [\"a\"], \"bad\": [\"b\"], \"conclusion\": \"c\"}\n```";
        let raw = RunJudge::extract_json(fenced).unwrap();
        let parsed: ReviewJson = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.conclusion, "c");
        assert!(RunJudge::extract_json("no braces here").is_none());
    }

    #[test]
    fn test_judge_prompt_mentions_no_cross_run_comparison() {
        let p = RunJudge::build_prompt("redis", "PING must reply PONG", &[], "{}");
        assert!(p.contains("Never compare"));
        assert!(p.contains("exactly 9 bullets"));
    }

    #[test]
    fn test_judge_resolve_target_routes_models() {
        let (url, model) = RunJudge::resolve_target("openrouter/meta/model-x").unwrap();
        assert!(url.contains("openrouter.ai"));
        assert_eq!(model, "meta/model-x");
        assert!(RunJudge::resolve_target("local/qwen3").is_err());
        assert!(RunJudge::resolve_target("ollama/qwen3").is_err());
        assert!(RunJudge::resolve_target("bogus-provider/x").is_err());
    }

    #[test]
    fn test_judge_collect_workspace_skips_junk() {
        let base = std::env::temp_dir().join(format!("test_judge_ws_{}", std::process::id()));
        let _ = std::fs::create_dir_all(base.join(".hidden"));
        let _ = std::fs::create_dir_all(base.join("target"));
        std::fs::write(base.join("main.go"), "package main").unwrap();
        std::fs::write(base.join(".hidden/secret"), "x").unwrap();
        std::fs::write(base.join("target/blob"), "y").unwrap();
        let files = RunJudge::collect_workspace(&base);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "main.go");
        let _ = std::fs::remove_dir_all(&base);
    }

    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn temp_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("sdb_judge_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_manifest() -> RunManifest {
        serde_json::from_value(serde_json::json!({
            "run_id": "test_judge_run",
            "model": "fixture/model",
            "task": "redis",
            "status": "completed",
            "language": "Rust",
            "started_at": "2026-10-01T00:00:00Z",
            "completed_at": "2026-10-01T00:01:00Z",
            "duration_seconds": 61.0,
            "pass_rate": 100.0,
            "passed_stages": 4,
            "total_stages": 4,
            "stages": [{"stage": 1, "name": "boot", "passed": true, "error": null}],
            "throughput_req_sec": 1234.5,
            "tokens": {
                "prompt_tokens": 11,
                "cached_tokens": 3,
                "completion_tokens": 7,
                "total_tokens": 21
            },
            "cost_usd": 0.0123,
            "savings_percent": 0.0,
            "efficiency_score": 55.0,
            "files": []
        }))
        .unwrap()
    }

    #[test]
    fn test_judge_validate_trims_counts_and_caps_bullets() {
        let mut r = sample();
        r.good = vec!["  only strength  ".to_string()];
        r.bad = vec![
            "b1".to_string(),
            "b2".to_string(),
            "b3".to_string(),
            "b4".to_string(),
            "b5".to_string(),
            "b6".to_string(),
            "b7".to_string(),
            "b8".to_string(),
        ];
        assert!(RunJudge::validate(&r).is_ok());
        // Bullets are trimmed before the 500-char cap is applied.
        let mut r = sample();
        r.good[0] = "x".repeat(500);
        assert!(RunJudge::validate(&r).is_ok());
        r.good[0] = format!(" {} ", "x".repeat(501));
        assert!(RunJudge::validate(&r).is_err());
        // Whitespace-only conclusion is empty.
        let mut r = sample();
        r.conclusion = "  \n\t".to_string();
        assert!(RunJudge::validate(&r).is_err());
        // Nine bullets on one side is rejected (needs at least one of each).
        let mut r = sample();
        r.bad.clear();
        r.good = (0..9).map(|i| format!("g{i}")).collect();
        assert!(RunJudge::validate(&r).is_err());
    }

    #[test]
    fn test_judge_extract_json_variants() {
        assert_eq!(
            RunJudge::extract_json("prose { \"a\": 1 } more").unwrap(),
            "{ \"a\": 1 }"
        );
        assert_eq!(
            RunJudge::extract_json("  ```json\n{\"a\":1}\n```  ").unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(
            RunJudge::extract_json("```\n{\"a\":1}\n```").unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(RunJudge::extract_json("}{\"a\":1}").unwrap(), "{\"a\":1}");
        assert_eq!(RunJudge::extract_json("{ }").unwrap(), "{ }");
        assert!(RunJudge::extract_json("closing } only").is_none());
        assert!(RunJudge::extract_json("}{").is_none());
        assert!(RunJudge::extract_json("").is_none());
    }

    #[test]
    fn test_judge_collect_workspace_nested_order_and_filters() {
        let base = temp_root("ws_order");
        std::fs::create_dir_all(base.join("sub/deep")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(base.join("__pycache__")).unwrap();
        std::fs::write(base.join("b.txt"), "b").unwrap();
        std::fs::write(base.join("a.txt"), "a").unwrap();
        std::fs::write(base.join("server.log"), "noise").unwrap();
        std::fs::write(base.join(".env"), "secret").unwrap();
        std::fs::write(base.join("sub/z.txt"), "z").unwrap();
        std::fs::write(base.join("sub/m.txt"), "m").unwrap();
        std::fs::write(base.join("sub/.hidden"), "h").unwrap();
        std::fs::write(base.join("sub/deep/leaf.txt"), "leaf").unwrap();
        std::fs::write(base.join("node_modules/pkg/index.js"), "js").unwrap();
        std::fs::write(base.join("__pycache__/x.pyc"), "pyc").unwrap();
        // Entries that are neither dir nor file (dangling symlink) are skipped.
        std::os::unix::fs::symlink(base.join("gone"), base.join("dangling")).unwrap();
        let files = RunJudge::collect_workspace(&base);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "a.txt",
                "b.txt",
                "sub/deep/leaf.txt",
                "sub/m.txt",
                "sub/z.txt"
            ]
        );
        assert_eq!(files[0].1, "a");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_judge_collect_workspace_budget_truncation() {
        let base = temp_root("ws_budget");
        let big = "b".repeat(MAX_WORKSPACE_CHARS + 500);
        std::fs::write(base.join("big.txt"), &big).unwrap();
        // Sorts after big.txt, so the exhausted budget must skip it.
        std::fs::write(base.join("zz_after.txt"), "never read").unwrap();
        let files = RunJudge::collect_workspace(&base);
        assert_eq!(files.len(), 1);
        let (name, content) = &files[0];
        assert_eq!(name, "big.txt");
        assert!(content.ends_with("\n...[truncated]"));
        assert_eq!(
            content.len(),
            MAX_WORKSPACE_CHARS + "\n...[truncated]".len()
        );
        // A directory entered with an already-exhausted budget collects nothing.
        let mut out = Vec::new();
        let mut budget = 0usize;
        RunJudge::collect_dir(&base, "", &mut out, &mut budget);
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_judge_collect_workspace_multibyte_boundary() {
        let base = temp_root("ws_unicode");
        // 23_999 ASCII bytes + 2-byte é = 24_001 bytes: the 24k cutoff
        // lands in the middle of a multi-byte char and must floor.
        let content = format!("{}é", "a".repeat(MAX_WORKSPACE_CHARS - 1));
        std::fs::write(base.join("u.txt"), &content).unwrap();
        let files = RunJudge::collect_workspace(&base);
        assert_eq!(files.len(), 1);
        let (_, clipped) = &files[0];
        assert!(clipped.ends_with("\n...[truncated]"));
        assert!(!clipped.contains('é'));
        assert_eq!(
            clipped.len(),
            MAX_WORKSPACE_CHARS - 1 + "\n...[truncated]".len()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_judge_collect_workspace_skips_unreadable_and_missing_dir() {
        let base = temp_root("ws_unreadable");
        std::fs::write(base.join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(base.join("ok.txt"), "ok").unwrap();
        let files = RunJudge::collect_workspace(&base);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "ok.txt");
        assert!(RunJudge::collect_workspace(&base.join("does-not-exist")).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_judge_build_prompt_truncates_and_lists_files() {
        let task_prompt = format!("{}TAIL", "Q".repeat(MAX_PROMPT_CHARS));
        let files = vec![("src/main.rs".to_string(), "fn main() {}".to_string())];
        let p = RunJudge::build_prompt("redis", &task_prompt, &files, "{\"pass_rate\":100}");
        assert!(p.contains("Review the candidate redis server"));
        assert!(p.contains("--- src/main.rs ---\nfn main() {}"));
        assert!(p.contains("## Measured results\n{\"pass_rate\":100}"));
        assert!(p.contains(&"Q".repeat(MAX_PROMPT_CHARS)));
        assert!(!p.contains("TAIL"));
        assert!(p.contains("exactly 9 bullets"));
    }

    #[test]
    fn test_judge_resolve_target_edges() {
        let (url, model) = RunJudge::resolve_target("  openrouter/meta/llama-3  ").unwrap();
        assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(model, "meta/llama-3");
        // Bare (slash-free) model ids route to OpenRouter too.
        let (_, model) = RunJudge::resolve_target("llama-3").unwrap();
        assert_eq!(model, "llama-3");
        // A slash-bearing non-openrouter id is not a cloud judge.
        assert!(RunJudge::resolve_target("meta/llama-3").is_err());
        let e = RunJudge::resolve_target("openrouter/x:batch")
            .unwrap_err()
            .to_string();
        assert!(e.contains("batch"));
        assert!(RunJudge::resolve_target("   ").is_err());
        let e = RunJudge::resolve_target("ollama/qwen")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unsupported judge model"));
    }

    #[test]
    fn test_run_stats_block_exact_numbers() {
        let trials = vec![
            (
                "t1".to_string(),
                100.0_f64,
                10_u32,
                1000_u64,
                0.1_f64,
                60.0_f64,
            ),
            ("t2".to_string(), 50.0, 20, 3000, 0.2, 120.0),
        ];
        let failures = vec!["".to_string(), "boot: timeout".to_string()];
        let s = RunJudge::run_stats_block_with_failures(
            "run7",
            "openai/gpt",
            "redis",
            &trials,
            &failures,
        );
        let expected = concat!(
            "RUN run7 model=openai/gpt task=redis trials=2 passed=1/2 mean_pass=75.0% (range 50-100) mean_turns=15 mean_tokens=2000 mean_cost=$0.1500 mean_dur=90s\n",
            "  trial t1 pass=100% turns=10 tokens=1000 cost=$0.1000 dur=60s\n",
            "  trial t2 pass=50% turns=20 tokens=3000 cost=$0.2000 dur=120s\n",
            "    failures: boot: timeout\n",
        );
        assert_eq!(s, expected);
    }

    #[test]
    fn test_run_stats_block_empty_uses_fold_defaults() {
        let s = RunJudge::run_stats_block("run0", "m", "t", &[]);
        // Empty sums are -0.0 in Rust, and the pass-rate fold defaults to
        // (100, 0), so the range prints inverted for an empty run.
        assert_eq!(
            s,
            "RUN run0 model=m task=t trials=0 passed=0/0 mean_pass=-0.0% (range 100-0) mean_turns=-0 mean_tokens=-0 mean_cost=$-0.0000 mean_dur=-0s\n"
        );
    }

    #[test]
    fn test_run_stats_block_without_failures_omits_lines() {
        let trials = vec![("only".to_string(), 33.3_f64, 1_u32, 7_u64, 0.0_f64, 5.0_f64)];
        let s = RunJudge::run_stats_block("r", "m", "t", &trials);
        assert!(s.contains("passed=0/1"));
        assert!(s.contains("range 33-33"));
        assert!(!s.contains("failures:"));
    }

    #[test]
    fn test_build_synthesis_prompt_sections() {
        let stats = "RUN x model=m task=t trials=1 passed=1/1\n";
        let task_prompt = format!("{}TAILMARKER", "A".repeat(MAX_PROMPT_CHARS));
        let notes = vec![("t1".to_string(), "Trial one conclusion".to_string())];
        let files = vec![("main.go".to_string(), "package main".to_string())];
        let p = RunJudge::build_synthesis_prompt(stats, &task_prompt, &notes, &files);
        assert!(p.contains(stats));
        assert!(p.contains("## Run statistics (facts, do not recompute)"));
        assert!(p.contains("### t1\nTrial one conclusion\n"));
        assert!(p.contains("## Canonical best-trial workspace"));
        assert!(p.contains("### main.go\npackage main\n"));
        assert!(p.contains(&"A".repeat(MAX_PROMPT_CHARS)));
        assert!(!p.contains("TAILMARKER"));
        let p2 = RunJudge::build_synthesis_prompt(stats, "short", &[], &[]);
        assert!(!p2.contains("Canonical best-trial workspace"));
        assert!(!p2.contains("### t1"));
    }

    // ---- HTTP boundary: everything below talks to a local tokio TCP mock,
    // never to a real provider. ----

    struct MockRequest {
        headers: String,
        body: serde_json::Value,
    }

    struct JudgeMock {
        url: String,
        requests: Arc<Mutex<Vec<MockRequest>>>,
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0)
    }

    /// Scripted one-request-per-connection HTTP/1.1 mock. Each reply is
    /// `(status, body)`; requests are captured for assertions.
    async fn spawn_judge_mock(replies: Vec<(u16, String)>) -> JudgeMock {
        spawn_judge_mock_on("127.0.0.1", replies).await
    }

    /// Same, bound to an explicit loopback address. A non-`127.0.0.1`
    /// address also switches the production max-token branch.
    async fn spawn_judge_mock_on(addr: &str, replies: Vec<(u16, String)>) -> JudgeMock {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((addr, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut raw_headers = String::new();
                let raw_body = loop {
                    if let Some(pos) = find_header_end(&buf) {
                        let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let len = content_length(&headers);
                        if buf.len() >= pos + 4 + len {
                            raw_headers = headers;
                            break buf[pos + 4..pos + 4 + len].to_vec();
                        }
                    }
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break Vec::new(),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                captured.lock().unwrap().push(MockRequest {
                    headers: raw_headers,
                    body: serde_json::from_slice(&raw_body).unwrap_or(serde_json::Value::Null),
                });
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    500 => "Internal Server Error",
                    _ => "Error",
                };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        JudgeMock {
            url: format!("http://{addr}:{port}/v1/chat/completions"),
            requests,
        }
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn ok_message(payload: serde_json::Value) -> String {
        serde_json::json!({ "choices": [{ "message": payload }] }).to_string()
    }

    fn valid_review() -> ReviewJson {
        ReviewJson {
            good: ["g1", "g2", "g3", "g4"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            bad: ["b1", "b2", "b3", "b4", "b5"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            conclusion: "Solid but incomplete.".to_string(),
        }
    }

    fn valid_review_text() -> String {
        serde_json::to_string(&valid_review()).unwrap()
    }

    #[tokio::test]
    async fn test_judge_call_once_shapes_request() {
        let mock = spawn_judge_mock(vec![(
            200,
            ok_message(serde_json::json!({ "content": valid_review_text() })),
        )])
        .await;
        let client = test_client();
        let messages = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "user prompt"}),
        ];
        let text = RunJudge::call_once(
            &client,
            &mock.url,
            Some("sk-test"),
            "meta/model",
            &messages,
            99,
        )
        .await
        .unwrap();
        assert!(text.contains("\"conclusion\""));
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let body = &reqs[0].body;
        assert_eq!(body["model"], "meta/model");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["max_tokens"], 99);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        let headers = reqs[0].headers.to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer sk-test"));
        assert!(headers.contains("x-title: subdollarbench-judge"));
        assert!(headers.contains("http-referer: https://github.com/lubod/subdollarbench"));
    }

    #[tokio::test]
    async fn test_judge_attempt_content_fallbacks() {
        let client = test_client();
        // Whitespace content falls through to reasoning_content.
        let mock = spawn_judge_mock(vec![(
            200,
            ok_message(serde_json::json!({
                "content": "   ",
                "reasoning_content": valid_review_text()
            })),
        )])
        .await;
        let review = RunJudge::attempt(&client, &mock.url, None, "m", "p".into())
            .await
            .unwrap();
        assert_eq!(review.conclusion, "Solid but incomplete.");
        {
            let reqs = mock.requests.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert!(!reqs[0]
                .headers
                .to_ascii_lowercase()
                .contains("authorization:"));
        }
        // Empty reasoning_content falls through to reasoning.
        let mock = spawn_judge_mock(vec![(
            200,
            ok_message(serde_json::json!({
                "reasoning_content": "",
                "reasoning": valid_review_text()
            })),
        )])
        .await;
        let review = RunJudge::attempt(&client, &mock.url, None, "m", "p".into())
            .await
            .unwrap();
        assert_eq!(review.good.len(), 4);
    }

    #[tokio::test]
    async fn test_judge_attempt_surfaces_api_and_payload_errors() {
        let client = test_client();
        let mock = spawn_judge_mock(vec![(
            401,
            serde_json::json!({"error": {"message": "invalid api key"}}).to_string(),
        )])
        .await;
        let err = RunJudge::attempt(&client, &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("judge API error 401"), "{err}");
        assert!(err.contains("invalid api key"), "{err}");
        // Structured error without a message reads "unknown".
        let mock = spawn_judge_mock(vec![(500, "{}".to_string())]).await;
        let err = RunJudge::attempt(&client, &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown"), "{err}");
        // 200 with no usable content.
        let mock = spawn_judge_mock(vec![(200, ok_message(serde_json::json!({})))]).await;
        let err = RunJudge::attempt(&client, &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no content"), "{err}");
        // 200 with a non-JSON body.
        let mock = spawn_judge_mock(vec![(200, "this is not json".to_string())]).await;
        let err = RunJudge::attempt(&client, &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("judge response unreadable"), "{err}");
    }

    #[tokio::test]
    async fn test_judge_attempt_retries_invalid_and_missing_json() {
        let client = test_client();
        // Extractable but rule-violating JSON: one corrective retry.
        let invalid = "{\"good\": [\"only\"], \"bad\": [], \"conclusion\": \"\"}";
        let mock = spawn_judge_mock(vec![
            (200, ok_message(serde_json::json!({ "content": invalid }))),
            (
                200,
                ok_message(serde_json::json!({ "content": valid_review_text() })),
            ),
        ])
        .await;
        let review = RunJudge::attempt(&client, &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap();
        assert_eq!(review.bad.len(), 5);
        {
            let reqs = mock.requests.lock().unwrap();
            assert_eq!(reqs.len(), 2);
            let msgs = reqs[1].body["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 4);
            assert!(msgs[2]["content"].as_str().unwrap().contains("conclusion"));
            assert!(msgs[3]["content"]
                .as_str()
                .unwrap()
                .contains("That output was invalid"));
        }
        // No JSON at all on the first reply: different corrective prompt.
        let mock = spawn_judge_mock(vec![
            (
                200,
                ok_message(serde_json::json!({ "content": "I cannot review this run." })),
            ),
            (
                200,
                ok_message(serde_json::json!({ "content": valid_review_text() })),
            ),
        ])
        .await;
        let review = RunJudge::attempt(&client, &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap();
        assert_eq!(review.good.len(), 4);
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(reqs[1].body["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("No JSON object found"));
    }

    #[tokio::test]
    async fn test_judge_attempt_uses_non_local_max_tokens() {
        let mock = spawn_judge_mock_on(
            "127.0.0.2",
            vec![(
                200,
                ok_message(serde_json::json!({ "content": valid_review_text() })),
            )],
        )
        .await;
        let review = RunJudge::attempt(&test_client(), &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap();
        assert_eq!(review.bad.len(), 5);
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].body["max_tokens"], 2000);
    }

    #[tokio::test]
    async fn test_judge_attempt_exhausts_retry() {
        let invalid = "{\"good\": [], \"bad\": [], \"conclusion\": \"\"}";
        let mock = spawn_judge_mock(vec![
            (200, ok_message(serde_json::json!({ "content": invalid }))),
            (200, ok_message(serde_json::json!({ "content": invalid }))),
        ])
        .await;
        let err = RunJudge::attempt(&test_client(), &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("did not return a valid review after retry"),
            "{err}"
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
        // Two prose-only replies: the second call breaks out instead of
        // appending another corrective message.
        let mock = spawn_judge_mock(vec![
            (
                200,
                ok_message(serde_json::json!({ "content": "no json here" })),
            ),
            (
                200,
                ok_message(serde_json::json!({ "content": "still no json" })),
            ),
        ])
        .await;
        let err = RunJudge::attempt(&test_client(), &mock.url, Some("k"), "m", "p".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("did not return a valid review after retry"),
            "{err}"
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_judge_run_rejects_batch_model_before_network() {
        let workdir = temp_root("run_batch");
        let err = RunJudge::judge_run(
            Some("sk-test"),
            "openrouter/some/judge:batch",
            "redis",
            "task prompt",
            &workdir,
            &sample_manifest(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("batch API judges stall"), "{err}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[tokio::test]
    async fn test_judge_run_rejects_empty_workspace() {
        let workdir = temp_root("run_empty_ws");
        let err = RunJudge::judge_run(
            Some("sk-test"),
            "openrouter/some/judge",
            "redis",
            "task prompt",
            &workdir,
            &sample_manifest(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("no reviewable workspace files"), "{err}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn test_judge_run_requires_key_when_env_missing() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("OPENROUTER_API_KEY");
        let workdir = temp_root("run_nokey");
        // Deliberately no IO driver: even a regression that skipped the key
        // gate could not reach the network from here.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(RunJudge::judge_run(
            None,
            "openrouter/some/judge",
            "redis",
            "task prompt",
            &workdir,
            &sample_manifest(),
        ));
        match prev {
            Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
        let err = result.unwrap_err().to_string();
        assert!(err.contains("needs an OpenRouter API key"), "{err}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn test_judge_synthesis_preconditions() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("OPENROUTER_API_KEY");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let missing_key = rt.block_on(RunJudge::judge_synthesis(
            None,
            "openrouter/some/judge",
            "prompt".into(),
        ));
        match prev {
            Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
        let err = missing_key.unwrap_err().to_string();
        assert!(err.contains("needs an OpenRouter API key"), "{err}");
        // Both remaining guards fire before any client is built.
        let batch = rt
            .block_on(RunJudge::judge_synthesis(
                Some("sk-test"),
                "openrouter/some/judge:batch",
                "p".into(),
            ))
            .unwrap_err()
            .to_string();
        assert!(batch.contains("batch API judges stall"), "{batch}");
        let empty = rt
            .block_on(RunJudge::judge_synthesis(Some("sk-test"), "  ", "p".into()))
            .unwrap_err()
            .to_string();
        assert!(empty.contains("empty review model"), "{empty}");
    }
}
