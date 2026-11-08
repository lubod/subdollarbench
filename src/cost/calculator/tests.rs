use super::*;

/// R51 F5: every grid model has an overrides entry (empty allowed), and
/// the lookup matches like the snapshot.
#[test]
fn r51_f5_every_grid_model_has_an_entry() {
    for m in [
        "anthropic/claude-haiku-5.5",
        "openai/gpt-6-luna",
        "deepseek/deepseek-v4.1-flash-1017",
        "xiaomi/mimo-v2.6-flash",
        "qwen/qwen3.8-flash",
        "tencent/hy4",
        "minimax/minimax-m3.1-flash",
        "openai/gpt-6-luna-pro",
        "xiaomi/mimo-v2.6-pro",
    ] {
        assert!(price_override_for(m).is_some(), "{m} has no entry");
    }
    assert_eq!(price_override_for("xiaomi/mimo-v2.6-flash"), Some(""));
    assert_eq!(price_override_for("qwen/qwen3.8-flash"), Some(""));
    assert_eq!(
        price_override_for("openrouter/OpenAI/GPT-6-LUNA"),
        Some("2× above 272k prompt tokens")
    );
    assert_eq!(price_override_for("unknown/model"), None);
    assert_eq!(price_snapshot_as_of(), "2026-11-08");
}
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The offline fallback must never price a real model at zero. muse-spark
/// is a cheap paid model with no table entry, so a regression that let an
/// unpriced model fall through to zero would silently record real runs as
/// free and flatter every score that divides by cost.
/// The regression this fixes: a zero live-delta (weekly counter lag)
/// recorded a paid model at $0.00 — 144 archived rows. Zero delta must
/// never beat a positive formula cost.
#[test]
fn test_zero_live_delta_never_zeros_a_paid_model() {
    // R24 R2: the snapshot formula is the scored cost; live deltas are
    // audit-only, whether zero, below or above the formula.
    let (c, src) = ModelPricing::resolve_api_cost(Some(0.0), 0.0175);
    assert_eq!(c, 0.0175, "the snapshot must win a zero-delta read");
    assert_eq!(src, "snapshot");

    let (c, src) = ModelPricing::resolve_api_cost(Some(0.0), 0.0);
    assert_eq!(c, 0.0);
    assert_eq!(src, "snapshot");

    let (c, src) = ModelPricing::resolve_api_cost(Some(0.0021), 0.05);
    assert_eq!(c, 0.05, "a discounted provider delta is audit-only");
    assert_eq!(src, "snapshot");

    let (c, src) = ModelPricing::resolve_api_cost(None, 0.0333);
    assert_eq!(c, 0.0333);
    assert_eq!(src, "snapshot");
}

/// A disagreement must stay visible in the record, not just the log:
/// the board reader needs to know the cost came from the formula
/// because the live counter read zero.
#[test]
fn test_live_delta_is_audit_only_not_a_second_source() {
    let (ca, a) = ModelPricing::resolve_api_cost(Some(0.0), 0.01);
    let (cb, b) = ModelPricing::resolve_api_cost(None, 0.01);
    assert_eq!((ca, a), (cb, b));
    assert_eq!(a, "snapshot");
}

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
fn r23_d3_list_price_distinguishes_unknown_from_known() {
    assert!(ModelPricing::list_price_for_model("google/gemini-2.5-flash").is_some());
    assert!(ModelPricing::list_price_for_model("some-random-unknown-model").is_none());
}

#[test]
fn test_pricing_for_known_models() {
    let gemini = ModelPricing::for_model("google/gemini-2.5-flash");
    assert_eq!(gemini.prompt_per_million, 0.30);
    assert_eq!(gemini.completion_per_million, 2.50);
    assert_eq!(gemini.cache_read_per_million, 0.03);

    let deepseek = ModelPricing::for_model("deepseek/deepseek-chat");
    assert_eq!(deepseek.prompt_per_million, 0.32);
    assert_eq!(deepseek.completion_per_million, 0.89);
    assert_eq!(deepseek.cache_read_per_million, 0.32);

    let qwen = ModelPricing::for_model("qwen/qwen-2.5-coder-32b-instruct");
    assert_eq!(qwen.prompt_per_million, 0.66);
    assert_eq!(qwen.completion_per_million, 1.00);
    assert_eq!(qwen.cache_read_per_million, 0.66);

    let llama = ModelPricing::for_model("meta-llama/llama-3.3-70b-instruct");
    assert_eq!(llama.prompt_per_million, 0.59);
    assert_eq!(llama.completion_per_million, 0.79);
    assert_eq!(llama.cache_read_per_million, 0.295);

    let gpt4o = ModelPricing::for_model("openai/gpt-4o-mini");
    assert_eq!(gpt4o.prompt_per_million, 0.15);
    assert_eq!(gpt4o.cache_read_per_million, 0.075);

    let haiku = ModelPricing::for_model("anthropic/claude-haiku-5.5");
    assert_eq!(haiku.prompt_per_million, 0.10);
    assert_eq!(haiku.completion_per_million, 0.50);

    let mistral = ModelPricing::for_model("mistralai/mistral-small-2603");
    assert_eq!(mistral.prompt_per_million, 0.15);
    assert_eq!(mistral.completion_per_million, 0.60);

    let custom = ModelPricing::from_rates(0.50, 1.50);
    assert_eq!(custom.prompt_per_million, 0.50);
    assert_eq!(custom.completion_per_million, 1.50);
    assert_eq!(custom.cache_read_per_million, 0.125);

    let unknown = ModelPricing::for_model("some-random-unknown-model");
    assert_eq!(unknown.prompt_per_million, 0.20);
    assert_eq!(unknown.completion_per_million, 0.60);
}

/// R46 F1: the five grid models are all in the committed snapshot.
#[test]
fn r46_f1_grid_models_are_priced() {
    let expect = [
        (
            "openrouter/deepseek/deepseek-v4.1-flash-1017",
            0.15,
            0.6,
            0.003,
            0.15,
        ),
        (
            "openrouter/anthropic/claude-haiku-5.5",
            0.1,
            0.5,
            0.01,
            0.125,
        ),
        ("openrouter/openai/gpt-6-luna", 0.1, 0.5, 0.01, 0.125),
        (
            "openrouter/xiaomi/mimo-v2.6-flash",
            0.14,
            0.28,
            0.0028,
            0.14,
        ),
        ("openrouter/qwen/qwen3.8-flash", 0.15, 0.47, 0.016, 0.15),
    ];
    for (model, prompt, completion, read, write) in expect {
        let r = ModelPricing::snapshot_rates(model)
            .unwrap_or_else(|| panic!("missing snapshot price for {model}"));
        assert!(
            (r.prompt_per_million - prompt).abs() < 1e-12,
            "{model} prompt"
        );
        assert!(
            (r.completion_per_million - completion).abs() < 1e-12,
            "{model} completion"
        );
        assert!(
            (r.cache_read_per_million - read).abs() < 1e-12,
            "{model} cache read"
        );
        assert!(
            (r.cache_write_per_million - write).abs() < 1e-12,
            "{model} cache write"
        );
    }
}

/// R66 F1: the next grid's four models are priced from the catalog, with
/// the two context/time overrides; the snapshot date moved to the fetch.
#[test]
fn r66_f1_new_models_priced() {
    let cases = [
        ("tencent/hy4", 0.417, 1.2505, 0.021, 0.417),
        ("minimax/minimax-m3.1-flash", 0.1125, 0.45, 0.01125, 0.1125),
        ("openai/gpt-6-luna-pro", 0.1, 0.5, 0.01, 0.125),
        ("xiaomi/mimo-v2.6-pro", 0.435, 0.87, 0.0036, 0.435),
    ];
    for (id, prompt, completion, cache_read, cache_write) in cases {
        let p = ModelPricing::list_price_for_model(id)
            .unwrap_or_else(|| panic!("{id} has no snapshot row"));
        assert!((p.prompt_per_million - prompt).abs() < 1e-9, "{id} prompt");
        assert!(
            (p.completion_per_million - completion).abs() < 1e-9,
            "{id} completion"
        );
        assert!(
            (p.cache_read_per_million - cache_read).abs() < 1e-9,
            "{id} cache read"
        );
        assert!(
            (p.cache_write_per_million - cache_write).abs() < 1e-9,
            "{id} cache write"
        );
        assert!(
            ModelPricing::list_price_for_model(&format!("openrouter/{id}")).is_some(),
            "{id} must match with the openrouter/ prefix"
        );
    }
    // R67 F1: the pinned GMICloud endpoint has no time-of-day override.
    assert_eq!(price_override_for("minimax/minimax-m3.1-flash"), Some(""));
    assert_eq!(
        price_override_for("openai/gpt-6-luna-pro"),
        Some("2× above 272k prompt tokens")
    );
    assert_eq!(price_override_for("tencent/hy4"), Some(""));
    assert_eq!(price_override_for("xiaomi/mimo-v2.6-pro"), Some(""));
}

/// R67 F1: every grid row names the OpenRouter endpoint it is pinned to,
/// and its four prices are that endpoint's. The fixture is the
/// `/api/v1/models/<id>/endpoints` reading of 2026-11-08 (per million;
/// a missing cache-write is the prompt rate, the snapshot's rule).
#[test]
fn r67_f1_rows_match_pinned_provider() {
    let endpoints: [(&str, &str, f64, f64, f64, f64); 9] = [
        (
            "anthropic/claude-haiku-5.5",
            "anthropic",
            0.10,
            0.50,
            0.01,
            0.125,
        ),
        (
            "deepseek/deepseek-v4.1-flash-1017",
            "deepseek",
            0.15,
            0.60,
            0.003,
            0.15,
        ),
        ("openai/gpt-6-luna", "openai", 0.10, 0.50, 0.01, 0.125),
        (
            "xiaomi/mimo-v2.6-flash",
            "xiaomi/fp8",
            0.14,
            0.28,
            0.0028,
            0.14,
        ),
        (
            "qwen/qwen3.8-flash",
            "alibaba/us-east-1",
            0.15,
            0.47,
            0.016,
            0.15,
        ),
        ("tencent/hy4", "tencent/fp4", 0.417, 1.2505, 0.021, 0.417),
        (
            "minimax/minimax-m3.1-flash",
            "gmicloud/fp8",
            0.1125,
            0.45,
            0.01125,
            0.1125,
        ),
        ("openai/gpt-6-luna-pro", "openai", 0.10, 0.50, 0.01, 0.125),
        (
            "xiaomi/mimo-v2.6-pro",
            "xiaomi/fp8",
            0.435,
            0.87,
            0.0036,
            0.435,
        ),
    ];
    for (id, provider, prompt, completion, cache_read, cache_write) in endpoints {
        assert_eq!(
            provider_for_model(id),
            Some(provider),
            "{id} must be pinned"
        );
        let p = ModelPricing::list_price_for_model(id)
            .unwrap_or_else(|| panic!("{id} has no snapshot row"));
        for (name, got, want) in [
            ("prompt", p.prompt_per_million, prompt),
            ("completion", p.completion_per_million, completion),
            ("cache read", p.cache_read_per_million, cache_read),
            ("cache write", p.cache_write_per_million, cache_write),
        ] {
            assert!(
                (got - want).abs() < 1e-9,
                "{id} {name}: snapshot {got} != endpoint {want}"
            );
        }
    }
    // A row without a provider is not pinned.
    assert_eq!(provider_for_model("google/gemini-2.5-flash"), None);
}

/// R24-1: the operator's test/calibration models must be launchable.
#[test]
fn r24_r1_every_test_or_calibration_model_has_a_snapshot_price() {
    for model in [
        "openrouter/deepseek/deepseek-v4.1-flash-1017",
        "openrouter/anthropic/claude-haiku-5.5",
    ] {
        assert!(
            ModelPricing::list_price_for_model(model).is_some(),
            "missing snapshot price for {model}"
        );
    }
}

/// R24-1: the test model named in the roadmap must be launchable.
#[test]
fn r24_r1_the_todo_test_model_has_a_snapshot_price() {
    let todo = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("TODO_CAMPAIGNS.md"),
    )
    .expect("TODO_CAMPAIGNS.md");
    let model = todo
        .split("the test model is")
        .nth(1)
        .and_then(|rest| rest.split('`').nth(1))
        .expect("a backticked test model after 'the test model is'");
    assert!(
        ModelPricing::list_price_for_model(model).is_some(),
        "the roadmap test model {model} has no snapshot price"
    );
}

/// R24-2: catalog ids are exact after stripping the openrouter/ prefix;
/// variants do not inherit a base model's price.
#[test]
fn r24_r2_snapshot_ids_are_exact_not_substrings() {
    assert!(ModelPricing::list_price_for_model("deepseek/deepseek-chat").is_some());
    assert!(ModelPricing::list_price_for_model("openrouter/deepseek/deepseek-chat").is_some());
    assert!(
        ModelPricing::list_price_for_model("deepseek/deepseek-chat-v3.1").is_none(),
        "a variant needs its own snapshot entry"
    );
    assert!(ModelPricing::list_price_for_model("deepseek/deepseek-chat:free").is_none());
    assert!(
        ModelPricing::list_price_for_model("mistralai/mistral-small-3.2-24b-instruct").is_none()
    );
}

/// R24-3: cache-write tokens are new context and must cost something.
#[test]
fn r24_r3_cache_write_tokens_cost_more_than_zero() {
    let p = ModelPricing::for_model("openrouter/anthropic/claude-haiku-5.5");
    let b = p.compute_cost_with_cache_and_write(0, 0, 0, 1_000_000);
    assert!(b.total_cost_usd > 0.0);
    assert!((b.total_cost_usd - p.cache_write_per_million).abs() < 1e-12);
}

#[test]
fn r24_r2_cost_reads_the_committed_snapshot() {
    let haiku = ModelPricing::for_model("openrouter/anthropic/claude-haiku-5.5");
    assert_eq!(haiku.prompt_per_million, 0.10);
    assert_eq!(haiku.completion_per_million, 0.50);
    assert_eq!(haiku.cache_read_per_million, 0.01);
    assert!(ModelPricing::list_price_for_model("openrouter/anthropic/claude-haiku-5.5").is_some());
    assert!(ModelPricing::list_price_for_model("not/a-real-model").is_none());
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
    let diff = (b.total_cost_usd - 2.80).abs();
    assert!(diff < 1e-6, "Expected 2.80, got {}", b.total_cost_usd);
    assert_eq!(b.savings_usd, 0.0);
    assert_eq!(b.savings_percent, 0.0);
}

#[test]
fn test_cost_calculation_full_prompt_cache() {
    let p = ModelPricing::for_model("google/gemini-2.5-flash");
    let b = p.compute_cost_with_cache(0, 1_000_000, 0);
    let diff = (b.total_cost_usd - 0.03).abs();
    assert!(diff < 1e-6, "Expected 0.03, got {}", b.total_cost_usd);
    let diff_savings = (b.savings_usd - 0.27).abs();
    assert!(diff_savings < 1e-6, "Expected 0.27, got {}", b.savings_usd);
    assert!((b.savings_percent - 90.0).abs() < 1e-4);
}

/// Cross-model attribution: the model in the row must be the model
/// priced. A muse-spark row priced at deepseek rates (or vice versa)
/// silently moves every score that divides by cost. Pin one exact
/// hand-computed row per priced model family.
#[test]
fn test_attribution_each_model_prices_its_own_rates() {
    // prompt=1M fresh, cached=1M, completion=1M.
    // total = prompt + 1M*cache_frac + completion.
    let cases = [
        ("google/gemini-2.5-flash", 0.30, 2.50, 0.03),
        ("deepseek/deepseek-chat", 0.32, 0.89, 0.32),
        ("qwen/qwen-2.5-coder-32b-instruct", 0.66, 1.00, 0.66),
        ("meta-llama/llama-3.3-70b-instruct", 0.59, 0.79, 0.295),
        ("openai/gpt-4o-mini", 0.15, 0.60, 0.075),
        ("anthropic/claude-haiku-5.5", 0.10, 0.50, 0.01),
        ("mistralai/mistral-small-2603", 0.15, 0.60, 0.015),
    ];
    for (model, pr, co, ca) in cases {
        let p = ModelPricing::for_model(model);
        let b = p.compute_cost_with_cache(1_000_000, 1_000_000, 1_000_000);
        let want = pr + ca + co;
        assert!(
            (b.total_cost_usd - want).abs() < 1e-9,
            "{model}: got {}, want {want}",
            b.total_cost_usd
        );
    }
    // Unknown models share one fallback; a silent per-model drift
    // (e.g. muse-spark priced as deepseek) shows up as a mismatch here.
    let fb = ModelPricing::for_model("openrouter/meta/muse-spark-1.3-contributor");
    let b = fb.compute_cost_with_cache(1_000_000, 1_000_000, 1_000_000);
    assert!((b.total_cost_usd - (0.10 + 0.002 + 0.20)).abs() < 1e-9);
}

/// Completion tokens bill at completion rates, not prompt rates. Swapping
/// the two underprices every reasoning-heavy run (completion is 2-5x
/// prompt across the table).
#[test]
fn test_attribution_completion_billed_at_completion_rate() {
    let p = ModelPricing::for_model("google/gemini-2.5-flash");
    let b = p.compute_cost_with_cache(0, 0, 1_000_000);
    assert!((b.total_cost_usd - 2.50).abs() < 1e-9);
    let b2 = p.compute_cost_with_cache(1_000_000, 0, 0);
    assert!((b2.total_cost_usd - 0.30).abs() < 1e-9);
}

/// Cache math is a discount off the prompt rate, never free and never
/// full price: 0 < cached < fresh for the same token count.
#[test]
fn test_attribution_cached_between_zero_and_fresh() {
    for model in [
        "google/gemini-2.5-flash",
        "deepseek/deepseek-chat",
        "openai/gpt-4o-mini",
        "some-random-unknown-model",
    ] {
        let p = ModelPricing::for_model(model);
        let fresh = p.compute_cost_with_cache(1_000_000, 0, 0).total_cost_usd;
        let cached = p.compute_cost_with_cache(0, 1_000_000, 0).total_cost_usd;
        // A catalog row with no cache discount (deepseek-chat today) may
        // equal the fresh rate; it is never free and never above it.
        assert!(
            cached > 0.0 && cached <= fresh,
            "{model}: cached {cached} not in (0, {fresh}]"
        );
    }
    let gem = ModelPricing::for_model("google/gemini-2.5-flash");
    let fresh = gem.compute_cost_with_cache(1_000_000, 0, 0).total_cost_usd;
    let cached = gem.compute_cost_with_cache(0, 1_000_000, 0).total_cost_usd;
    assert!(cached < fresh, "gemini keeps a cache discount");
}

/// Effective-cost formula exactness: api + time floor + fresh baseline +
/// discounted cached baseline, with cached clamped at total.
/// Live-spend precedence: when the key checkpoint reports a delta,
/// `cost_usd` is the PROVIDER's number, not the formula's. The formula
/// still governs the compute floor inside effective cost. Regression:
/// archived rows whose api cost matches neither fallback nor catalog
/// (muse-spark trial2: archived $0.035377 vs fallback $0.076029 vs
/// catalog $0.014668) are live-spend rows — effective = live + floor.
#[test]
fn test_live_spend_delta_is_audit_only() {
    // muse-spark-1.3-contributor trial2 tokens, duration 438.55s.
    let (prompt, cached, comp, total, dur) =
        (75_932u64, 900_407u64, 26_370u64, 1_002_709u64, 438.55f64);
    let snapshot = ModelPricing::for_model("openrouter/meta/muse-spark-1.3-contributor")
        .compute_cost_with_cache(prompt, cached, comp)
        .total_cost_usd;
    assert!(
        (snapshot - 0.014668014).abs() < 1e-9,
        "snapshot moved: {snapshot}"
    );
    // R24 R2: the live delta is audit-only; the snapshot is scored.
    for live in [Some(0.035377375), None] {
        let (cost_usd, source) = ModelPricing::resolve_api_cost(live, snapshot);
        assert_eq!(source, "snapshot");
        assert!((cost_usd - snapshot).abs() < 1e-12);
        let eff = ModelPricing::calculate_effective_cost(cost_usd, dur, total, cached);
        assert!(
            eff > cost_usd,
            "compute floor must add, got {eff} <= {cost_usd}"
        );
    }
}

#[test]
fn test_effective_cost_formula_exactness() {
    let eff = ModelPricing::calculate_effective_cost(1.0, 100.0, 200_000, 50_000);
    let want = 1.0
        + 100.0 * COMPUTE_COST_PER_SECOND
        + 150_000.0 * COMPUTE_COST_PER_TOKEN
        + 50_000.0 * COMPUTE_COST_PER_TOKEN * COMPUTE_COST_CACHED_FRACTION;
    assert!((eff - want).abs() < 1e-12, "got {eff}, want {want}");
    // Cached > total clamps instead of underflowing the fresh term.
    let clamped = ModelPricing::calculate_effective_cost(0.0, 0.0, 100, 999_999);
    let want_c = 100.0 * COMPUTE_COST_PER_TOKEN * COMPUTE_COST_CACHED_FRACTION;
    assert!((clamped - want_c).abs() < 1e-12);
}

#[test]
fn test_denominator_gate_proxy_pdf_unscored_despite_reference() {
    let eff = 0.05;
    for task in ["proxy", "pdf", "wal", "pubsub", "binproto"] {
        assert_eq!(
            ModelPricing::calculate_throughput_score_for_task(
                100.0,
                eff,
                Some(50_000.0),
                Some(30_000.0),
                Some(task)
            ),
            None,
            "task {task} must never score"
        );
    }
    for task in ["redis", "http"] {
        assert!(
            ModelPricing::calculate_throughput_score_for_task(
                100.0,
                eff,
                Some(30_000.0),
                Some(30_000.0),
                Some(task)
            )
            .is_some(),
            "task {task} must keep scoring"
        );
    }
    // Case-insensitive: "Proxy" from a manifest must not slip through.
    assert_eq!(
        ModelPricing::calculate_throughput_score_for_task(
            100.0,
            eff,
            Some(50_000.0),
            Some(30_000.0),
            Some("Proxy")
        ),
        None
    );
}

#[test]
fn test_cost_calculation_cached_exceeds_prompt() {
    let p = ModelPricing::for_model("google/gemini-2.5-flash");
    let b = p.compute_cost_with_cache(500_000, 1_000_000, 0);
    let diff = (b.total_cost_usd - 0.18).abs();
    assert!(diff < 1e-6, "Expected 0.18, got {}", b.total_cost_usd);
    assert!((b.savings_percent - 60.0).abs() < 1e-4);
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
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
                body.len(),
                body
            );
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
    let pricing = ModelPricing::for_model_async("unknown-provider/dummy-test").await;
    assert_eq!(pricing.prompt_per_million, 0.20);
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
    assert!((tp_score.unwrap() - (score * 1.0)).abs() < 1e-3);

    // No live reference -> no score (refless tasks rank by P@1/cost).
    for task in ["redis", "pdf", "wal", "proxy", "pubsub", "http"] {
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

    // Zero or missing candidate throughput is no measurement: None
    // (unscored), never a fabricated 0.0. A crashed candidate that never
    // produced a rate must not read as "measured slowest".
    let tp_zero = ModelPricing::calculate_throughput_score(100.0, eff, Some(0.0), Some(30_000.0));
    assert_eq!(tp_zero, None);
    let tp_none = ModelPricing::calculate_throughput_score(100.0, eff, None, Some(30_000.0));
    assert_eq!(tp_none, None);

    // Linear pricing: half-speed (=0.5x) and 2x-speed scores are the
    // base efficiency scaled by the ratio (operator decision Q5).
    let tp_half =
        ModelPricing::calculate_throughput_score(100.0, eff, Some(15_000.0), Some(30_000.0));
    assert!((tp_half.unwrap() - (score * 0.5)).abs() < 1e-3);

    let tp_double =
        ModelPricing::calculate_throughput_score(100.0, eff, Some(60_000.0), Some(30_000.0));
    assert!((tp_double.unwrap() - (score * 2.0)).abs() < 1e-3);
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
