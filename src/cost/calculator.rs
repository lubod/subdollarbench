use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

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
    /// R24 R3: new context written into the prompt cache. Catalog
    /// `input_cache_write`, or the prompt rate when the catalog has none.
    pub cache_write_per_million: f64,
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

    /// Resolve the recorded API cost from the two measurements we take.
    ///
    /// The live key-usage delta is ground truth *when it moves*: it is the
    /// only value that reflects promotions, credits, and provider-specific
    /// billing. But `/auth/key` reports weekly usage with variable lag, so
    /// `delta == 0` is not evidence of zero spend — it is equally consistent
    /// with "the counter has not caught up yet". 144 archived muse-spark
    /// rows (2026-10-06) recorded exactly that: a paid model at $0.00,
    /// which both flattered its efficiency score and parked a paid model on
    /// the free board. A zero delta therefore never overrides a positive
    /// formula cost; the formula (token-count based, conservative) wins
    /// that disagreement, and the disagreement is recorded rather than
    /// silently averaged away.
    ///
    /// Returns `(cost, source)` where source is `"live"`, `"formula"`, or
    /// `"formula_zero_delta"` (the disagreement case above).
    /// R24 R2: the list-price snapshot is the scored cost. The live key
    /// delta is recorded for audit only; promotions/provider routing cannot
    /// silently move a score.
    pub fn resolve_api_cost(_live_delta: Option<f64>, formula_cost: f64) -> (f64, &'static str) {
        (formula_cost, "snapshot")
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

    /// Calculates Engineering Efficiency Score (pts/¢): Pass@1 (%) / (Effective Cost in Cents)
    /// Expected utility: P@1 scales the 100%-at-that-cost score; mean pass rate is reported but does NOT enter the score.
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
    /// Score = Base Efficiency * speed_ratio (linear; operator decision
    /// 2026-10-14, Q5: speed priced proportionally until a wave shows
    /// r > 1.5 is a reliable quality signal).
    /// where speed_ratio = candidate_tp / reference_tp.
    ///
    /// At speed_ratio = 1.0 (matching reference baseline), the score equals
    /// base efficiency. At speed_ratio = 0.0 (crash / 0 req/s), 0.0x.
    /// Superlinear curves were retired: r(1+2r) let a 2x-faster model cost
    /// 10x per solve and still tie.
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

    /// Tasks with no valid throughput denominator, enforced by task name
    /// rather than by trusting callers to pass reference=None:
    ///
    /// - proxy: divided by DIRECT nginx while it must forward through a
    ///   three-upstream pool — capped below 1.0 by construction.
    /// - pdf: the reference is our own slow parity oracle, expected SLOWER
    ///   than candidates — the ratio rewards distance from ourselves.
    /// - wal/pubsub/binproto: verifier-timed, no external reference.
    ///
    /// The pipeline already discards these denominators before scoring; this
    /// is the second gate so a future caller cannot re-mint a proxy/pdf
    /// score by passing a reference anyway.
    pub fn has_valid_throughput_denominator(task: Option<&str>) -> bool {
        matches!(
            task.map(|t| t.to_lowercase()).as_deref(),
            Some("redis") | Some("http")
        )
    }

    pub fn calculate_throughput_score_for_task(
        pass_rate: f64,
        effective_cost_usd: f64,
        throughput_req_sec: Option<f64>,
        reference_throughput_req_sec: Option<f64>,
        task: Option<&str>,
    ) -> Option<f64> {
        if !Self::has_valid_throughput_denominator(task) && task.is_some() {
            return None;
        }
        // No evidence, no score: a failed trial (pass<=0, no throughput,
        // no reference) must be None (unscored, ranks by P@1/cost), never
        // Some(0.0) — a fabricated zero that reads as "measured slowest"
        // and outranks nothing honestly. 58 archived trial rows carry
        // throughput_score:0.0 with null throughput AND null reference.
        let base_eff = Self::calculate_efficiency_score(pass_rate, effective_cost_usd);
        if base_eff <= 0.0 {
            return None;
        }
        // No live reference, no score. Fallback baselines were unlabeled
        // constants unmeasured on this testbed; scoring against them made
        // refless tasks look comparable to anchored ones. Callers display
        // "--" and rank by P@1/cost instead.
        let ref_tp = match reference_throughput_req_sec {
            Some(v) if v > 0.0 => v,
            _ => return None,
        };
        // Zero or missing candidate throughput is no measurement either:
        // a crashed candidate never produced a rate. None, not a zero that
        // reads as "measured slowest". Applies regardless of task — the
        // unqualified wrapper (task=None) must not re-mint zeros either.
        let cand_tp = throughput_req_sec.unwrap_or(0.0).max(0.0);
        if cand_tp <= 0.0 {
            return None;
        }
        let speed_ratio = cand_tp / ref_tp;
        Some(base_eff * speed_ratio)
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
            cache_write_per_million: prompt_per_million,
        }
    }

    pub fn from_rates(prompt_per_m: f64, completion_per_m: f64) -> Self {
        Self {
            prompt_per_million: prompt_per_m,
            completion_per_million: completion_per_m,
            cache_read_per_million: prompt_per_m * 0.25,
            cache_write_per_million: prompt_per_m,
        }
    }

    /// R24 R2: the committed list-price snapshot. It is part of the rules
    /// fingerprint; a provider catalog change does not move a score until
    /// this file is edited and the rules bump.
    pub fn snapshot_rates(model_name: &str) -> Option<Self> {
        let m = model_name.to_lowercase();
        let m = m.strip_prefix("openrouter/").unwrap_or(&m);
        snapshot_entries().iter().find_map(|e| {
            e.id.eq_ignore_ascii_case(m).then_some(Self {
                prompt_per_million: e.prompt_per_million,
                completion_per_million: e.completion_per_million,
                cache_read_per_million: e.cache_read_per_million,
                cache_write_per_million: e.cache_write_per_million,
            })
        })
    }

    fn fallback() -> Self {
        Self {
            prompt_per_million: 0.20,
            completion_per_million: 0.60,
            cache_read_per_million: 0.05,
            cache_write_per_million: 0.20,
        }
    }

    /// The offline fallback is only for unknown models outside a paid
    /// campaign; a paid launch and the scored cost require a snapshot entry.
    pub fn for_model(model_name: &str) -> Self {
        Self::snapshot_rates(model_name).unwrap_or_else(Self::fallback)
    }

    pub fn compute_cost_with_cache(
        &self,
        prompt_tokens: u64,
        cached_tokens: u64,
        completion_tokens: u64,
    ) -> CostBreakdown {
        self.compute_cost_with_cache_and_write(prompt_tokens, cached_tokens, completion_tokens, 0)
    }

    /// R24 R3: cache-write tokens are new context and must be billed at the
    /// cache-write rate, never at $0.
    pub fn compute_cost_with_cache_and_write(
        &self,
        prompt_tokens: u64,
        cached_tokens: u64,
        completion_tokens: u64,
        cache_write_tokens: u64,
    ) -> CostBreakdown {
        let fresh_cost = (prompt_tokens as f64 / 1_000_000.0) * self.prompt_per_million;
        let cached_cost = (cached_tokens as f64 / 1_000_000.0) * self.cache_read_per_million;
        let write_cost = (cache_write_tokens as f64 / 1_000_000.0) * self.cache_write_per_million;
        let comp_cost = (completion_tokens as f64 / 1_000_000.0) * self.completion_per_million;
        let total_cost_usd = fresh_cost + cached_cost + write_cost + comp_cost;
        let un_cached_cost_usd = ((prompt_tokens + cached_tokens + cache_write_tokens) as f64
            / 1_000_000.0)
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
    /// R23 D3/R24 R2: the committed snapshot price, or None when the model
    /// has only the anonymous fallback. The fallback is never a list price.
    pub fn list_price_for_model(model_name: &str) -> Option<Self> {
        Self::snapshot_rates(model_name)
    }

    /// Kept for callers/tests that used the catalog URL. R24 R2 makes the
    /// snapshot the only source; the URL is ignored.
    pub async fn list_price_for_model_async_at(model_name: &str, _url: &str) -> Option<Self> {
        Self::list_price_for_model(model_name)
    }

    pub async fn list_price_for_model_async(model_name: &str) -> Option<Self> {
        Self::list_price_for_model(model_name)
    }

    pub async fn for_model_async(model_name: &str) -> Self {
        Self::for_model(model_name)
    }
}

#[derive(Debug, Deserialize)]
struct PriceEntry {
    id: String,
    prompt_per_million: f64,
    completion_per_million: f64,
    cache_read_per_million: f64,
    cache_write_per_million: f64,
}

#[derive(Debug, Deserialize)]
struct PriceSnapshot {
    as_of: String,
    models: Vec<PriceEntry>,
}

fn snapshot() -> &'static PriceSnapshot {
    static SNAPSHOT: OnceLock<PriceSnapshot> = OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        serde_json::from_str(include_str!("model_prices.json"))
            .expect("the committed model price snapshot must parse")
    })
}

fn snapshot_entries() -> &'static [PriceEntry] {
    snapshot().models.as_slice()
}

/// R51 F5 (Q8 a): provider price overrides applied on top of the snapshot's
/// base rates. The snapshot cannot express them (they depend on context
/// size or the hour), so they live in this one table next to the snapshot
/// reader, outside the graded JSON. An empty detail means no override.
pub const PRICE_OVERRIDES: &[(&str, &str)] = &[
    (
        "anthropic/claude-haiku-5.5",
        "5× the prompt rate above 100k prompt tokens",
    ),
    ("openai/gpt-6-luna", "2× above 272k prompt tokens"),
    (
        "deepseek/deepseek-v4.1-flash-1017",
        "2× in weekday peak hours",
    ),
    ("xiaomi/mimo-v2.6-flash", ""),
    ("qwen/qwen3.8-flash", ""),
];

/// The override for a model, matched like the snapshot (openrouter/ prefix
/// ignored, case-insensitive). `Some("")` means the model is priced but has
/// no override; `None` means the model is not in the grid table.
pub fn price_override_for(model: &str) -> Option<&'static str> {
    let m = model.to_lowercase();
    let m = m.strip_prefix("openrouter/").unwrap_or(&m);
    PRICE_OVERRIDES
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(m))
        .map(|(_, detail)| *detail)
}

/// The snapshot's date, for the results page's Prices row.
pub fn price_snapshot_as_of() -> &'static str {
    snapshot().as_of.as_str()
}

#[cfg(test)]
mod tests;
