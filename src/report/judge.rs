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
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
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
}
