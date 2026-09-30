use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub total_cost_usd: f64,
    pub un_cached_cost_usd: f64,
    pub savings_usd: f64,
    pub savings_percent: f64,
}

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub prompt_per_million: f64,
    pub completion_per_million: f64,
    pub cache_read_per_million: f64,
}

/// Equivalent compute runtime rate in USD per second ($0.00003/sec ~= $0.108/hour).
pub const COMPUTE_COST_PER_SECOND: f64 = 0.00003;

/// Baseline token volume processing rate in USD per token ($0.05 per 1M tokens).
pub const COMPUTE_COST_PER_TOKEN: f64 = 0.00000005;

/// Fraction of the baseline charged for a cache-read token.
///
/// The compute floor is a proxy for the work of moving the context, and a
/// cache read is explicitly cheaper than a fresh token everywhere else in this
/// file: `ModelPricing::cache_read_per_million` is 75-90% below
/// `prompt_per_million` for every model in the table, and the API-cost path
/// applies that discount. Charging cached tokens the full baseline here made
/// the *effective* cost - the denominator of the headline efficiency score -
/// ignore the very discount the adjacent "Cache Savings" column reports, so
/// context-heavy runs were billed twice for tokens the provider already
/// discounted. One recorded run carried 109689 cached tokens of 127229 total.
pub const COMPUTE_COST_CACHED_FRACTION: f64 = 0.25;

/// Key balance snapshot: cumulative usage + optional credit ceiling.
/// `limit` is None for uncapped keys (fall back to caller threshold).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyBalance {
    pub usage: f64,
    pub limit: Option<f64>,
}

impl KeyBalance {
    /// Remaining credit, if the key reports a ceiling.
    pub fn remaining(&self) -> Option<f64> {
        self.limit.map(|l| (l - self.usage).max(0.0))
    }
}

pub async fn query_openrouter_key_usage_at(api_key: &str, url: &str) -> Option<f64> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .ok()?;

    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("data")?.get("usage")?.as_f64()
}

impl ModelPricing {
    pub async fn query_openrouter_key_usage(api_key: &str) -> Option<f64> {
        query_openrouter_key_usage_at(api_key, "https://openrouter.ai/api/v1/auth/key").await
    }

    /// Balance snapshot (usage + limit/remaining) for the billing tripwire.
    /// Non-success HTTP (401/402/…) or unparsable body yields None = unknown.
    pub async fn query_key_balance(api_key: &str) -> Option<KeyBalance> {
        Self::query_key_balance_at(api_key, "https://openrouter.ai/api/v1/auth/key").await
    }

    pub async fn query_key_balance_at(api_key: &str, url: &str) -> Option<KeyBalance> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .ok()?;
        let resp = client
            .get(url)
            .header("Authorization", format!("Bearer {}", api_key.trim()))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        let data = body.get("data")?;
        let usage = data.get("usage")?.as_f64()?;
        let limit = data.get("limit").and_then(|v| v.as_f64());
        let remaining = data.get("limit_remaining").and_then(|v| v.as_f64());
        // Prefer explicit remaining when present; else limit - usage.
        let limit = limit.or_else(|| remaining.map(|r| r + usage));
        Some(KeyBalance { usage, limit })
    }
    /// Calculates the equivalent compute cost from execution duration and
    /// token volume, charging cache-read tokens at the discounted baseline.
    pub fn compute_cost(duration_seconds: f64, total_tokens: u64, cached_tokens: u64) -> f64 {
        let cached = cached_tokens.min(total_tokens);
        let fresh = total_tokens - cached;
        (duration_seconds * COMPUTE_COST_PER_SECOND)
            + (fresh as f64 * COMPUTE_COST_PER_TOKEN)
            + (cached as f64 * COMPUTE_COST_PER_TOKEN * COMPUTE_COST_CACHED_FRACTION)
    }

    /// Calculates effective total cost: API spend + compute runtime floor.
    ///
    /// `total_tokens` is the whole context (fresh + cached + completion) and
    /// `cached_tokens` is the discounted portion of it; cached must not exceed
    /// total, and is clamped rather than trusted.
    pub fn calculate_effective_cost(
        api_cost_usd: f64,
        duration_seconds: f64,
        total_tokens: u64,
        cached_tokens: u64,
    ) -> f64 {
        api_cost_usd + Self::compute_cost(duration_seconds, total_tokens, cached_tokens)
    }

    /// Calculates Engineering Efficiency Score (pts/¢): Pass Rate (%) / (Effective Cost in Cents)
    /// Proportional credit: a 98% run earns 98% of a 100% run's score at
    /// the same cost. (Pre-2026-09-24 scoring gated on strictly 100% and
    /// zeroed everything below; see RULES_VERSION history.)
    pub fn calculate_efficiency_score(pass_rate: f64, effective_cost_usd: f64) -> f64 {
        if pass_rate <= 0.0 || effective_cost_usd <= 0.0 {
            0.0
        } else {
            let cost_cents = (effective_cost_usd * 100.0).max(0.0001);
            pass_rate.clamp(0.0, 100.0) / cost_cents
        }
    }

    /// Calculates the Throughput-Adjusted Score.
    /// Score = Base Efficiency * speed_ratio * (1.0 + 2.0 * speed_ratio)
    /// where speed_ratio = candidate_tp / reference_tp.
    ///
    /// At speed_ratio = 1.0 (matching reference baseline), multiplier is 3.0x.
    /// At speed_ratio = 0.0 (crash / 0 req/s), multiplier is 0.0x.
    /// Sub-reference throughput drops off quadratically to penalize slow implementations,
    /// while high throughput earns superlinear rewards.
    pub fn calculate_throughput_score(
        pass_rate: f64,
        effective_cost_usd: f64,
        throughput_req_sec: Option<f64>,
        reference_throughput_req_sec: Option<f64>,
    ) -> Option<f64> {
        Self::calculate_throughput_score_for_task(
            pass_rate,
            effective_cost_usd,
            throughput_req_sec,
            reference_throughput_req_sec,
            None,
        )
    }

    pub fn calculate_throughput_score_for_task(
        pass_rate: f64,
        effective_cost_usd: f64,
        throughput_req_sec: Option<f64>,
        reference_throughput_req_sec: Option<f64>,
        _task: Option<&str>,
    ) -> Option<f64> {
        let base_eff = Self::calculate_efficiency_score(pass_rate, effective_cost_usd);
        if base_eff <= 0.0 {
            return Some(0.0);
        }
        // No live reference, no score. Fallback baselines were unlabeled
        // constants unmeasured on this testbed; scoring against them made
        // refless tasks look comparable to anchored ones. Callers display
        // "--" and rank by P@1/cost instead.
        let ref_tp = match reference_throughput_req_sec {
            Some(v) if v > 0.0 => v,
            _ => return None,
        };
        let cand_tp = throughput_req_sec.unwrap_or(0.0).max(0.0);
        let speed_ratio = cand_tp / ref_tp;
        Some(base_eff * speed_ratio * (1.0 + 2.0 * speed_ratio))
    }

    pub fn new(
        prompt_per_million: f64,
        completion_per_million: f64,
        cache_read_per_million: f64,
    ) -> Self {
        Self {
            prompt_per_million,
            completion_per_million,
            cache_read_per_million,
        }
    }

    pub fn from_rates(prompt_per_m: f64, completion_per_m: f64) -> Self {
        Self {
            prompt_per_million: prompt_per_m,
            completion_per_million: completion_per_m,
            cache_read_per_million: prompt_per_m * 0.25,
        }
    }

    pub fn for_model(model_name: &str) -> Self {
        let m = model_name.to_lowercase();
        // NOTE: no muse-spark entry on purpose. It is a real (if cheap) paid
        // model, and one recorded run is 12073 prompt / 109689 cached / 5467
        // completion for $0.00504 - not enough to separate the prompt,
        // completion and cache-read rates. `for_model_async` resolves this
        // from the live OpenRouter catalog first; this table is only the
        // offline fallback, and a wrong entry here would silently mis-price
        // a real run whenever that query fails. Add the real rates once known.
        if m.contains("gemini-3.8-flash") {
            ModelPricing {
                prompt_per_million: 0.75,
                completion_per_million: 3.75,
                cache_read_per_million: 0.075,
            }
        } else if m.contains("gemini-2.5-flash") {
            ModelPricing {
                prompt_per_million: 0.15,
                completion_per_million: 0.60,
                cache_read_per_million: 0.0375, // 75% discount
            }
        } else if m.contains("deepseek-chat") || m.contains("deepseek-v3") {
            ModelPricing {
                prompt_per_million: 0.14,
                completion_per_million: 0.28,
                cache_read_per_million: 0.014, // 90% discount
            }
        } else if m.contains("qwen-2.5-coder-32b") {
            ModelPricing {
                prompt_per_million: 0.06,
                completion_per_million: 0.15,
                cache_read_per_million: 0.015, // 75% discount
            }
        } else if m.contains("llama-3.3-70b") {
            ModelPricing {
                prompt_per_million: 0.12,
                completion_per_million: 0.30,
                cache_read_per_million: 0.030, // 75% discount
            }
        } else if m.contains("gpt-4o-mini") {
            ModelPricing {
                prompt_per_million: 0.15,
                completion_per_million: 0.60,
                cache_read_per_million: 0.075, // 50% discount
            }
        } else if m.contains("claude-3-haiku") {
            ModelPricing {
                prompt_per_million: 0.80,
                completion_per_million: 4.00,
                cache_read_per_million: 0.08, // 90% discount
            }
        } else if m.contains("mistral-small") {
            ModelPricing {
                prompt_per_million: 0.10,
                completion_per_million: 0.30,
                cache_read_per_million: 0.025, // 75% discount
            }
        } else {
            ModelPricing {
                prompt_per_million: 0.20,
                completion_per_million: 0.60,
                cache_read_per_million: 0.050, // 75% discount fallback
            }
        }
    }

    pub fn compute_cost_with_cache(
        &self,
        prompt_tokens: u64,
        cached_tokens: u64,
        completion_tokens: u64,
    ) -> CostBreakdown {
        let fresh_cost = (prompt_tokens as f64 / 1_000_000.0) * self.prompt_per_million;
        let cached_cost = (cached_tokens as f64 / 1_000_000.0) * self.cache_read_per_million;
        let comp_cost = (completion_tokens as f64 / 1_000_000.0) * self.completion_per_million;

        let total_cost_usd = fresh_cost + cached_cost + comp_cost;
        let un_cached_cost_usd = ((prompt_tokens + cached_tokens) as f64 / 1_000_000.0)
            * self.prompt_per_million
            + comp_cost;

        let savings_usd = (un_cached_cost_usd - total_cost_usd).max(0.0);
        let savings_percent = if un_cached_cost_usd > 0.0 {
            (savings_usd / un_cached_cost_usd) * 100.0
        } else {
            0.0
        };

        CostBreakdown {
            total_cost_usd,
            un_cached_cost_usd,
            savings_usd,
            savings_percent,
        }
    }

    /// Query OpenRouter /api/v1/auth/key to get the exact live cumulative dollar usage
    /// Query OpenRouter /api/v1/models to get live catalog pricing for a model
    pub async fn fetch_openrouter_model_pricing(model_name: &str) -> Option<ModelPricing> {
        Self::fetch_openrouter_model_pricing_at(model_name, "https://openrouter.ai/api/v1/models")
            .await
    }

    pub async fn fetch_openrouter_model_pricing_at(
        model_name: &str,
        url: &str,
    ) -> Option<ModelPricing> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .ok()?;

        let resp = client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }

        let body: serde_json::Value = resp.json().await.ok()?;
        let data = body.get("data")?.as_array()?;

        let norm_model = model_name.to_lowercase();
        let matched = data.iter().find(|entry| {
            if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                let id_lower = id.to_lowercase();
                id_lower == norm_model
                    || norm_model.ends_with(&format!("/{}", id_lower))
                    || id_lower.ends_with(&format!("/{}", norm_model))
                    || id_lower == norm_model.trim_start_matches("openrouter/")
            } else {
                false
            }
        })?;

        let pricing = matched.get("pricing")?;
        let prompt_str = pricing.get("prompt").and_then(|v| v.as_str())?;
        let completion_str = pricing.get("completion").and_then(|v| v.as_str())?;

        let prompt_per_tok = prompt_str.parse::<f64>().ok()?;
        let comp_per_tok = completion_str.parse::<f64>().ok()?;

        Some(Self::from_rates(
            prompt_per_tok * 1_000_000.0,
            comp_per_tok * 1_000_000.0,
        ))
    }

    /// Asynchronously resolve pricing, querying OpenRouter live catalog first with fallback to hardcoded rates
    pub async fn for_model_async(model_name: &str) -> Self {
        if let Some(pricing) = Self::fetch_openrouter_model_pricing(model_name).await {
            pricing
        } else {
            Self::for_model(model_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// The offline fallback must never price a real model at zero. muse-spark
    /// is a cheap paid model with no table entry, so a regression that let an
    /// unpriced model fall through to zero would silently record real runs as
    /// free and flatter every score that divides by cost.
    #[test]
    fn test_unpriced_model_falls_back_to_a_positive_rate() {
        let p = ModelPricing::for_model("openrouter/meta/muse-spark-1.3-contributor");
        assert!(
            p.prompt_per_million > 0.0,
            "fallback prompt rate must be positive"
        );
        assert!(
            p.completion_per_million > 0.0,
            "fallback completion rate must be positive"
        );
        let b = p.compute_cost_with_cache(1_000_000, 1_000_000, 1_000_000);
        assert!(
            b.total_cost_usd > 0.0,
            "a priced model must not compute to zero"
        );
    }
    #[test]
    fn test_pricing_for_known_models() {
        let gemini = ModelPricing::for_model("google/gemini-2.5-flash");
        assert_eq!(gemini.prompt_per_million, 0.15);
        assert_eq!(gemini.completion_per_million, 0.60);
        assert_eq!(gemini.cache_read_per_million, 0.0375);

        let deepseek = ModelPricing::for_model("deepseek/deepseek-chat");
        assert_eq!(deepseek.prompt_per_million, 0.14);
        assert_eq!(deepseek.completion_per_million, 0.28);
        assert_eq!(deepseek.cache_read_per_million, 0.014);

        let qwen = ModelPricing::for_model("qwen/qwen-2.5-coder-32b-instruct");
        assert_eq!(qwen.prompt_per_million, 0.06);
        assert_eq!(qwen.completion_per_million, 0.15);
        assert_eq!(qwen.cache_read_per_million, 0.015);

        let llama = ModelPricing::for_model("meta-llama/llama-3.3-70b-instruct");
        assert_eq!(llama.prompt_per_million, 0.12);

        let gpt4o = ModelPricing::for_model("openai/gpt-4o-mini");
        assert_eq!(gpt4o.prompt_per_million, 0.15);
        assert_eq!(gpt4o.cache_read_per_million, 0.075);

        let haiku = ModelPricing::for_model("anthropic/claude-3-haiku");
        assert_eq!(haiku.prompt_per_million, 0.80);
        assert_eq!(haiku.completion_per_million, 4.00);

        let mistral = ModelPricing::for_model("mistralai/mistral-small");
        assert_eq!(mistral.prompt_per_million, 0.10);
        assert_eq!(mistral.completion_per_million, 0.30);

        let custom = ModelPricing::from_rates(0.50, 1.50);
        assert_eq!(custom.prompt_per_million, 0.50);
        assert_eq!(custom.completion_per_million, 1.50);
        assert_eq!(custom.cache_read_per_million, 0.125);

        let unknown = ModelPricing::for_model("some-random-unknown-model");
        assert_eq!(unknown.prompt_per_million, 0.20);
        assert_eq!(unknown.completion_per_million, 0.60);
    }

    #[test]
    fn test_cost_calculation_zero_tokens() {
        let p = ModelPricing::for_model("google/gemini-2.5-flash");
        let b = p.compute_cost_with_cache(0, 0, 0);
        assert_eq!(b.total_cost_usd, 0.0);
        assert_eq!(b.un_cached_cost_usd, 0.0);
        assert_eq!(b.savings_usd, 0.0);
        assert_eq!(b.savings_percent, 0.0);
    }

    #[test]
    fn test_cost_calculation_no_cache() {
        let p = ModelPricing::for_model("google/gemini-2.5-flash");
        let b = p.compute_cost_with_cache(1_000_000, 0, 1_000_000);
        let diff = (b.total_cost_usd - 0.75).abs();
        assert!(diff < 1e-6, "Expected 0.75, got {}", b.total_cost_usd);
        assert_eq!(b.savings_usd, 0.0);
        assert_eq!(b.savings_percent, 0.0);
    }

    #[test]
    fn test_cost_calculation_full_prompt_cache() {
        let p = ModelPricing::for_model("google/gemini-2.5-flash");
        let b = p.compute_cost_with_cache(0, 1_000_000, 0);
        let diff = (b.total_cost_usd - 0.0375).abs();
        assert!(diff < 1e-6, "Expected 0.0375, got {}", b.total_cost_usd);
        let diff_savings = (b.savings_usd - 0.1125).abs();
        assert!(
            diff_savings < 1e-6,
            "Expected 0.1125, got {}",
            b.savings_usd
        );
        assert!((b.savings_percent - 75.0).abs() < 1e-4);
    }

    #[test]
    fn test_cost_calculation_cached_exceeds_prompt() {
        let p = ModelPricing::for_model("google/gemini-2.5-flash");
        let b = p.compute_cost_with_cache(500_000, 1_000_000, 0);
        let diff = (b.total_cost_usd - 0.1125).abs();
        assert!(diff < 1e-6, "Expected 0.1125, got {}", b.total_cost_usd);
        assert!((b.savings_percent - 50.0).abs() < 1e-4);
    }

    #[tokio::test]
    async fn test_key_balance_limit_and_remaining() {
        use tokio::net::TcpListener;
        // Mock: usage 8.0, limit 10.0 -> remaining 2.0.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                use tokio::io::AsyncReadExt;
                let _ = sock.read(&mut buf).await;
                let body = "{\"data\":{\"usage\":8.0,\"limit\":10.0}}";
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}", body.len(), body);
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let url = format!("http://127.0.0.1:{}/api/v1/auth/key", port);
        let b = ModelPricing::query_key_balance_at("k", &url).await.unwrap();
        assert_eq!(b.usage, 8.0);
        assert_eq!(b.limit, Some(10.0));
        assert_eq!(b.remaining(), Some(2.0));
    }

    #[tokio::test]
    async fn test_query_openrouter_key_usage_mock() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let body = r#"{"data":{"usage": 0.4567}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let url = format!("http://127.0.0.1:{}/api/v1/auth/key", port);
        let usage = query_openrouter_key_usage_at("test-key", &url).await;
        assert_eq!(usage, Some(0.4567));
    }

    #[tokio::test]
    async fn test_query_openrouter_key_usage_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let url = format!("http://127.0.0.1:{}/api/v1/auth/key", port);
        let usage = query_openrouter_key_usage_at("bad-key", &url).await;
        assert_eq!(usage, None);
    }

    #[tokio::test]
    async fn test_fetch_openrouter_model_pricing_at_mock() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let body = r#"{"data":[{"id":"custom/super-model","pricing":{"prompt":"0.0000015","completion":"0.0000045"}}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let url = format!("http://127.0.0.1:{}/api/v1/models", port);
        let pricing = ModelPricing::fetch_openrouter_model_pricing_at("custom/super-model", &url)
            .await
            .unwrap();
        assert!((pricing.prompt_per_million - 1.50).abs() < 1e-6);
        assert!((pricing.completion_per_million - 4.50).abs() < 1e-6);
        assert!((pricing.cache_read_per_million - 0.375).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_for_model_async_fallback() {
        // Fallback to hardcoded when model not in openrouter or request fails
        let pricing =
            ModelPricing::for_model_async("unknown-provider/dummy-gemini-2.5-flash-test").await;
        assert_eq!(pricing.prompt_per_million, 0.15);
        assert_eq!(pricing.completion_per_million, 0.60);

        let unknown = ModelPricing::for_model_async("completely-unknown-xyz").await;
        assert_eq!(unknown.prompt_per_million, 0.20);
        assert_eq!(unknown.completion_per_million, 0.60);
    }

    #[test]
    fn test_compute_cost_and_efficiency_score() {
        let comp = ModelPricing::compute_cost(100.0, 200_000, 0);
        assert!((comp - 0.013).abs() < 1e-9);

        let eff = ModelPricing::calculate_effective_cost(0.0, 100.0, 200_000, 0);
        assert!((eff - 0.013).abs() < 1e-9);

        let score = ModelPricing::calculate_efficiency_score(100.0, eff);
        assert!((score - (100.0 / 1.3)).abs() < 1e-3);

        assert_eq!(ModelPricing::calculate_efficiency_score(0.0, eff), 0.0);
        // Proportional credit (no 100%-or-zero cliff).
        let half = ModelPricing::calculate_efficiency_score(50.0, eff);
        assert!((half - (50.0 / 1.3)).abs() < 1e-3);
        let near = ModelPricing::calculate_efficiency_score(98.0, eff);
        assert!((near - (98.0 / 1.3)).abs() < 1e-3);

        let tp_score =
            ModelPricing::calculate_throughput_score(100.0, eff, Some(30_000.0), Some(30_000.0));
        assert!((tp_score.unwrap() - (score * 3.0)).abs() < 1e-3);

        // No live reference -> no score (refless tasks rank by P@1/cost).
        for task in ["redis", "dns", "pdf", "wal", "proxy", "pubsub", "http"] {
            assert_eq!(
                ModelPricing::calculate_throughput_score_for_task(
                    100.0,
                    eff,
                    Some(50_000.0),
                    None,
                    Some(task)
                ),
                None,
                "task {task} without a live ref must not score"
            );
        }

        // Zero throughput or missing throughput yields 0.0
        let tp_zero =
            ModelPricing::calculate_throughput_score(100.0, eff, Some(0.0), Some(30_000.0));
        assert_eq!(tp_zero, Some(0.0));
        let tp_none = ModelPricing::calculate_throughput_score(100.0, eff, None, Some(30_000.0));
        assert_eq!(tp_none, Some(0.0));

        // Sub-reference performance (half-speed: ratio 0.5 => multiplier 0.5 * (1 + 1) = 1.0)
        let tp_half =
            ModelPricing::calculate_throughput_score(100.0, eff, Some(15_000.0), Some(30_000.0));
        assert!((tp_half.unwrap() - (score * 1.0)).abs() < 1e-3);

        // Superlinear high performance (2x speed: ratio 2.0 => multiplier 2.0 * (1 + 4) = 10.0)
        let tp_double =
            ModelPricing::calculate_throughput_score(100.0, eff, Some(60_000.0), Some(30_000.0));
        assert!((tp_double.unwrap() - (score * 10.0)).abs() < 1e-3);
    }

    /// The regression this fixes: the compute floor is the denominator of the
    /// headline efficiency score, and it used to charge cache-read tokens the
    /// full baseline rate while the API-cost path discounted them 75-90% and
    /// the adjacent "Cache Savings" column reported that discount. A
    /// context-heavy run was therefore billed twice for tokens the provider
    /// had already discounted, and the models this benchmark targets - cheap
    /// ones with long contexts - were the ones hurt.
    #[test]
    fn test_cached_tokens_are_not_double_charged() {
        let duration = 100.0;
        let total = 200_000u64;
        let all_fresh = ModelPricing::calculate_effective_cost(0.0, duration, total, 0);
        let all_cached = ModelPricing::calculate_effective_cost(0.0, duration, total, total);

        assert!(
            all_cached < all_fresh,
            "a fully cache-read context must cost less than a fresh one"
        );
        // Time term is identical in both, so the difference is exactly the
        // token discount: (1 - 0.25) * 200k * $0.05/M.
        let expected_delta =
            total as f64 * COMPUTE_COST_PER_TOKEN * (1.0 - COMPUTE_COST_CACHED_FRACTION);
        assert!(
            ((all_fresh - all_cached) - expected_delta).abs() < 1e-12,
            "expected a {} discount, got {}",
            expected_delta,
            all_fresh - all_cached
        );

        // A cached count larger than the total is clamped, not trusted.
        let clamped = ModelPricing::calculate_effective_cost(0.0, duration, total, total * 10);
        assert!(
            (clamped - all_cached).abs() < 1e-12,
            "cached must clamp to total"
        );

        // And the discount must actually move the headline score.
        let cheap = ModelPricing::calculate_efficiency_score(100.0, all_cached);
        let dear = ModelPricing::calculate_efficiency_score(100.0, all_fresh);
        assert!(
            cheap > dear,
            "a cached run must score better than an identical fresh one"
        );
    }
}
