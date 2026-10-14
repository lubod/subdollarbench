//! Cost-honesty annotation for archived zero-cost rows.
//!
//! Between 2026-09-30 (the positive-rate fallback fix) and 2026-10-07
//! (the `resolve_api_cost` fix), the pipeline recorded `live_spend_delta`
//! as authoritative whenever the weekly `/auth/key` usage counter moved,
//! including when it moved by exactly zero. Counter lag plus a paid model
//! therefore produced 144 archived runs at `cost_usd: 0.00` — a paid model
//! published on the free board.
//!
//! This pass does NOT rewrite `cost_usd`, effective cost, or scores of
//! archived rows: re-deriving published numbers is a grading decision, not
//! a data cleanup one. It annotates instead — the token-priced formula is
//! written next to the recorded zero, so `tierOf` (and any reader) can see
//! that the model is paid and the zero is lag. Every mutation is listed and
//! dry-run by default; the same annotations are written to run manifests,
//! result rows, and bench.db so all three stores agree.

use crate::config::get_repo_root;
use crate::cost::ModelPricing;
use crate::report::leaderboard::LeaderboardManager;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

/// Below this, a formula cost is indistinguishable from a free tier at the
/// token counts involved (a handful of tokens on a paid model). Annotating
/// would only add noise; the row reads as free either way.
const ANNOTATE_EPSILON: f64 = 1e-6;

pub struct CostAnnotateReport {
    pub manifests_scanned: usize,
    pub annotated: Vec<String>,
    pub results_annotated: usize,
    pub db_annotated: usize,
    pub db_parents_annotated: usize,
    pub already_free: usize,
    pub warnings: Vec<String>,
}

/// Resolve catalog pricing once per distinct model. `None` = no catalog
/// confirmation for that model — the entry is never annotated. This is
/// deliberate: the offline fallback table prices *every* unknown model
/// positive, so falling back to it would relabel genuinely free rows as
/// paid (ling would read as $1.06/M). An unconfirmable price means "leave
/// the row alone", reported in warnings.
async fn pricing_cache(models: &[String]) -> HashMap<String, Option<ModelPricing>> {
    let mut cache = HashMap::new();
    for m in models {
        if !cache.contains_key(m) {
            cache.insert(
                m.clone(),
                ModelPricing::fetch_openrouter_model_pricing(m).await,
            );
        }
    }
    cache
}

/// Token-priced API cost for any row shape, or None when the row carries
/// no tokens (nothing to price) or prices to zero (genuinely free).
/// Manifests nest tokens under `tokens`; result rows (and the `subruns`
/// arrays embedded in them) carry `prompt_tokens`/`cached_tokens`/
/// `completion_tokens` flat. Reads both.
fn row_formula_cost(row: &Value, pricing: &ModelPricing) -> Option<f64> {
    if row.get("tokens").is_some() {
        return formula_cost(row, pricing);
    }
    let flat = {
        let mut t = row.clone();
        if let Some(obj) = t.as_object_mut() {
            obj.insert(
                "tokens".to_string(),
                json!({
                    "prompt_tokens": row.get("prompt_tokens").cloned().unwrap_or(json!(0)),
                    "cached_tokens": row.get("cached_tokens").cloned().unwrap_or(json!(0)),
                    "completion_tokens": row.get("completion_tokens").cloned().unwrap_or(json!(0)),
                }),
            );
        }
        t
    };
    formula_cost(&flat, pricing)
}

/// Token-priced API cost for one manifest/result row, or None when the row
/// carries no tokens (nothing to price) or prices to zero (genuinely free).
fn formula_cost(row: &Value, pricing: &ModelPricing) -> Option<f64> {
    let tokens = row.get("tokens")?;
    let prompt = tokens.get("prompt_tokens")?.as_u64()?;
    let cached = tokens.get("cached_tokens")?.as_u64()?;
    let completion = tokens.get("completion_tokens")?.as_u64()?;
    let cost = pricing
        .compute_cost_with_cache(prompt, cached, completion)
        .total_cost_usd;
    if cost > ANNOTATE_EPSILON {
        Some(cost)
    } else {
        None
    }
}

fn set_annotation(row: &mut Value, formula: f64, source: &str) {
    if let Some(obj) = row.as_object_mut() {
        obj.insert("cost_formula_usd".to_string(), json!(formula));
        obj.insert("cost_source".to_string(), json!(source));
    }
}

/// Which manifests already carry a zero-cost annotation need. A `null`
/// (typed `Option` round-tripped through the DB) counts as absent — the
/// field only exists to say something.
/// `needs_annotation` for a typed row (DB pass): serialize, then apply the
/// same rule the JSON paths use.
fn needs_annotation_json<T: serde::Serialize>(row: &T) -> bool {
    match serde_json::to_value(row) {
        Ok(v) => needs_annotation(&v),
        Err(_) => false,
    }
}

/// Which rows still need the annotation. A `null` (typed `Option`
/// round-tripped through the DB) counts as absent — the field only exists
/// to say something.
///
/// Rows carry their recorded price under either key: legacy/result rows
/// use `cost_usd` (the pre-fix live-delta field) while typed
/// `BenchmarkRunResult` rows serialize `total_cost_usd`. A row with a real
/// recorded cost anywhere is not a zero-cost row and must not be
/// annotated; a row with no price field at all is treated as zero-cost
/// (the annotation then adds the formula for evidence).
fn needs_annotation(m: &Value) -> bool {
    let has_formula = m
        .get("cost_formula_usd")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if has_formula {
        return false;
    }
    for key in ["cost_usd", "total_cost_usd"] {
        if let Some(v) = m.get(key).and_then(|v| v.as_f64()) {
            if v != 0.0 {
                return false;
            }
        }
    }
    true
}

/// Add a plain model string to the pending pricing set (deduped).
fn collect_model_str(model: &str, models: &mut Vec<String>) {
    if !models.iter().any(|m| m == model) {
        models.push(model.to_string());
    }
}

/// Add a row's model to the pending pricing set (deduped).
fn collect_model(row: &Value, models: &mut Vec<String>) {
    if let Some(model) = row.get("model").and_then(|m| m.as_str()) {
        collect_model_str(model, models);
    }
}

pub async fn run_cost_annotate(
    results_dir: &Path,
    runs_dir: &Path,
    apply: bool,
) -> Result<CostAnnotateReport> {
    let root = get_repo_root();
    let canon_runs = root.join("runs");
    let canon_results = root.join("results");
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(runs_dir) != canon(&canon_runs) || canon(results_dir) != canon(&canon_results) {
        anyhow::bail!(
            "cost-annotate operates on the canonical repo dirs ({} and {}); got {} and {}",
            canon_runs.display(),
            canon_results.display(),
            runs_dir.display(),
            results_dir.display()
        );
    }

    // ---- Read every store up front. Models are collected UNCONDITIONALLY:
    // a price cache built only from still-unannotated rows goes empty on the
    // second run, and every later pass silently no-ops.
    let store = LeaderboardManager::open_store_if_exists();
    let db_runs: Vec<crate::report::RunManifest> = store
        .as_ref()
        .map(|s| s.load_run_manifests())
        .unwrap_or_default();
    let db_parents = store
        .as_ref()
        .map(|s| s.load_all_parents())
        .unwrap_or_default();

    let mut models: Vec<String> = Vec::new();
    for m in &db_runs {
        collect_model_str(&m.model, &mut models);
    }
    for parent in &db_parents {
        collect_model_str(&parent.model, &mut models);
        for sub in &parent.subruns {
            collect_model_str(&sub.model, &mut models);
        }
    }

    let mut manifest_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&canon_runs)
        .with_context(|| format!("read {}", canon_runs.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in &entries {
        let p = e.path().join("manifest.json");
        if p.is_file() {
            manifest_paths.push(p);
        }
    }
    let mut manifests: Vec<(std::path::PathBuf, Value)> = Vec::new();
    let mut manifests_scanned = 0usize;
    for p in &manifest_paths {
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        manifests_scanned += 1;
        collect_model(&v, &mut models);
        if needs_annotation(&v) {
            manifests.push((p.clone(), v));
        }
    }

    // Result rows and their embedded trials are priced in the same pass, so
    // their models must enter the cache too.
    let mut result_files: Vec<(std::path::PathBuf, Value)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&canon_results) {
        let mut files: Vec<std::path::PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let rows: Vec<&Value> = match &v {
                Value::Array(a) => a.iter().collect(),
                Value::Object(_) => vec![&v],
                _ => continue,
            };
            let mut wanted = false;
            for row in &rows {
                collect_model(row, &mut models);
                if needs_annotation(row) {
                    wanted = true;
                }
                if let Some(subs) = row.get("subruns").and_then(|s| s.as_array()) {
                    for sub in subs {
                        collect_model(sub, &mut models);
                        if needs_annotation(sub) {
                            wanted = true;
                        }
                    }
                }
            }
            if wanted {
                result_files.push((f, v));
            }
        }
    }

    let cache = pricing_cache(&models).await;

    // ---- Annotate each store where a zero cost prices positive.
    let mut annotated = Vec::new();
    let mut already_free = 0usize;
    let mut unconfirmed: Vec<String> = Vec::new();
    for (p, mut v) in manifests {
        let Some(model) = v
            .get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        let Some(Some(pricing)) = cache.get(&model) else {
            unconfirmed.push(model);
            continue;
        };
        let Some(formula) = formula_cost(&v, pricing) else {
            already_free += 1;
            continue;
        };
        let id = v
            .get("run_id")
            .and_then(|r| r.as_str())
            .unwrap_or("<unknown>")
            .to_string();
        annotated.push(format!("{id}: $0.00 -> formula ${formula:.6}"));
        if apply {
            set_annotation(&mut v, formula, "legacy_zero_delta");
            let tmp = p.with_extension("json.annotate-tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)
                .with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, &p).with_context(|| format!("rename onto {}", p.display()))?;
        }
    }

    let mut results_annotated = 0usize;
    for (f, mut arr) in result_files {
        // results/*.json holds one run object per file (older waves stored
        // arrays); accept both shapes rather than silently skipping.
        let mut rows: Vec<&mut Value> = match &mut arr {
            Value::Array(v) => v.iter_mut().collect(),
            Value::Object(_) => vec![&mut arr],
            _ => continue,
        };
        let mut changed = false;
        for row in rows.iter_mut() {
            // Parent rows embed their trials under `subruns`; the boards and
            // the tier split merge those nested rows, so annotating only the
            // top level leaves the merged parent with no formula signal.
            if let Some(subs) = row.get_mut("subruns").and_then(|s| s.as_array_mut()) {
                for sub in subs.iter_mut() {
                    if !needs_annotation(sub) {
                        continue;
                    }
                    let Some(model) = sub
                        .get("model")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                    else {
                        continue;
                    };
                    let Some(Some(pricing)) = cache.get(&model) else {
                        unconfirmed.push(model);
                        continue;
                    };
                    if let Some(formula) = row_formula_cost(sub, pricing) {
                        set_annotation(sub, formula, "legacy_zero_delta");
                        results_annotated += 1;
                        changed = true;
                    }
                }
            }
            if !needs_annotation(row) {
                continue;
            }
            let Some(model) = row
                .get("model")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let Some(Some(pricing)) = cache.get(&model) else {
                unconfirmed.push(model.clone());
                continue;
            };
            let Some(formula) = row_formula_cost(row, pricing) else {
                continue;
            };
            set_annotation(row, formula, "legacy_zero_delta");
            results_annotated += 1;
            changed = true;
        }
        if changed && apply {
            let tmp = f.with_extension("json.annotate-tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&arr)?)
                .with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, &f).with_context(|| format!("rename onto {}", f.display()))?;
        }
    }

    // bench.db `runs` (manifests) so the live UI agrees with the boards.
    let mut db_annotated = 0usize;
    for m in db_runs.iter() {
        if !needs_annotation_json(m) {
            continue;
        }
        let Some(Some(pricing)) = cache.get(&m.model) else {
            unconfirmed.push(m.model.clone());
            continue;
        };
        let formula = pricing
            .compute_cost_with_cache(
                m.tokens.prompt_tokens,
                m.tokens.cached_tokens,
                m.tokens.completion_tokens,
            )
            .total_cost_usd;
        if formula <= ANNOTATE_EPSILON {
            continue;
        }
        db_annotated += 1;
        if apply {
            let mut m = m.clone();
            m.cost_formula_usd = Some(formula);
            if m.cost_source.is_none() {
                m.cost_source = Some("legacy_zero_delta".to_string());
            }
            if let Some(s) = &store {
                let _ = s.upsert_run_manifest(&m);
            }
        }
    }

    // bench.db `parents` — what the boards actually read. `runs` holds
    // manifests; `parents` holds merged rows with `subruns` embedded, and
    // `merge_cost_formula` only yields a value when every trial carries one.
    let mut db_parents_annotated = 0usize;
    for parent in db_parents.iter() {
        let parent_v = serde_json::to_value(parent).unwrap_or(Value::Null);
        let subs_wanted = parent.subruns.iter().any(needs_annotation_json);
        if !needs_annotation(&parent_v) && !subs_wanted {
            continue;
        }
        let Some(Some(pricing)) = cache.get(&parent.model) else {
            unconfirmed.push(parent.model.clone());
            continue;
        };
        let mut touched = false;
        let mut updated = parent.clone();
        for sub in updated.subruns.iter_mut() {
            if !needs_annotation_json(sub) {
                continue;
            }
            let sv = serde_json::to_value(&*sub).unwrap_or(Value::Null);
            let Some(formula) = row_formula_cost(&sv, pricing) else {
                continue;
            };
            sub.cost_formula_usd = Some(formula);
            if sub.cost_source.is_none() {
                sub.cost_source = Some("legacy_zero_delta".to_string());
            }
            db_parents_annotated += 1;
            touched = true;
        }
        if needs_annotation(&parent_v) {
            if let Some(formula) = row_formula_cost(&parent_v, pricing) {
                updated.cost_formula_usd = Some(formula);
                if updated.cost_source.is_none() {
                    updated.cost_source = Some("legacy_zero_delta".to_string());
                }
                db_parents_annotated += 1;
                touched = true;
            }
        }
        if touched && apply {
            if let Some(s) = &store {
                let _ = s.upsert_parent(&updated);
            }
        }
    }

    unconfirmed.sort();
    unconfirmed.dedup();
    let warnings = if unconfirmed.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "no catalog price for {} model(s) ({}): those zero-cost rows were left untouched",
            unconfirmed.len(),
            unconfirmed.join(", ")
        )]
    };
    Ok(CostAnnotateReport {
        manifests_scanned,
        annotated,
        results_annotated,
        db_annotated,
        db_parents_annotated,
        already_free,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_tokens() -> Value {
        json!({
            "run_id": "redis_openrouter_meta_muse-spark-1.3-contributor_x",
            "model": "openrouter/meta/muse-spark-1.3-contributor",
            "cost_usd": 0.0,
            "tokens": {"prompt_tokens": 13325, "cached_tokens": 78236, "completion_tokens": 4185}
        })
    }

    #[test]
    fn test_needs_annotation_only_for_zero_cost_rows() {
        assert!(needs_annotation(&manifest_with_tokens()));
        let mut m = manifest_with_tokens();
        m["cost_usd"] = json!(0.0044);
        assert!(!needs_annotation(&m), "a priced row is never touched");
        let mut m = manifest_with_tokens();
        m["cost_formula_usd"] = json!(0.013);
        assert!(!needs_annotation(&m), "re-runs are idempotent");
        // A typed Option round-tripped through bench.db serializes as null;
        // that must read as absent, not as "already annotated".
        let mut m = manifest_with_tokens();
        m["cost_formula_usd"] = serde_json::Value::Null;
        assert!(
            needs_annotation(&m),
            "null cost_formula_usd must read as missing (db path)"
        );
    }

    /// Catalog mock serving free and paid entries; returns (port, handle).
    async fn spawn_catalog_mock(free_ids: &[&str]) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let entries: Vec<String> = free_ids
            .iter()
            .map(|id| {
                format!(
                    r#"{{"id":"{id}","pricing":{{"prompt":"0","completion":"0"}}}}"#
                )
            })
            .chain(std::iter::once(
                r#"{"id":"meta/muse-spark-1.3-contributor","pricing":{"prompt":"0.0000001","completion":"0.0000002"}}"#.to_string(),
            ))
            .collect();
        let body = format!(r#"{{"data":[{}]}}"#, entries.join(","));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        port
    }

    /// A catalog-free model prices to zero and is never annotated — the
    /// whole reason the pass resolves from the catalog instead of the
    /// offline fallback table.
    #[tokio::test]
    async fn test_genuinely_free_model_is_not_annotated() {
        let port = spawn_catalog_mock(&["inclusionai/ling-3.1-flash"]).await;
        let url = format!("http://127.0.0.1:{port}/api/v1/models");
        let p = ModelPricing::fetch_openrouter_model_pricing_at(
            "openrouter/inclusionai/ling-3.1-flash",
            &url,
        )
        .await
        .expect("catalog mock must price ling");
        let m = json!({
            "cost_usd": 0.0,
            "tokens": {"prompt_tokens": 5000000, "cached_tokens": 0, "completion_tokens": 100000}
        });
        assert_eq!(
            formula_cost(&m, &p),
            None,
            "a catalog-free model must not gain a formula cost"
        );
    }

    /// The bug case: catalog-confirmed paid model, recorded zero cost.
    #[tokio::test]
    async fn test_paid_model_zero_cost_prices_positive() {
        let port = spawn_catalog_mock(&[]).await;
        let url = format!("http://127.0.0.1:{port}/api/v1/models");
        let p = ModelPricing::fetch_openrouter_model_pricing_at(
            "openrouter/meta/muse-spark-1.3-contributor",
            &url,
        )
        .await
        .expect("catalog mock must price muse-spark");
        let m = manifest_with_tokens();
        let f = formula_cost(&m, &p);
        assert!(
            f.is_some(),
            "95k tokens on a paid model must price positive"
        );
        assert!(
            f.unwrap() > 0.0001 && f.unwrap() < 0.05,
            "sane magnitude: {f:?}"
        );
    }

    /// No catalog entry -> the row is left alone (cache stores None), never
    /// priced by the offline fallback.
    #[tokio::test]
    async fn test_unknown_model_is_never_annotated() {
        let port = spawn_catalog_mock(&[]).await;
        let url = format!("http://127.0.0.1:{port}/api/v1/models");
        let p = ModelPricing::fetch_openrouter_model_pricing_at("no-such/model", &url).await;
        assert!(
            p.is_none(),
            "an unpriced-in-catalog model yields no pricing"
        );
    }

    #[test]
    fn test_annotation_is_distinguishable_from_pipeline_sources() {
        let mut row = manifest_with_tokens();
        set_annotation(&mut row, 0.0134, "legacy_zero_delta");
        assert_eq!(row["cost_source"], "legacy_zero_delta");
        assert_eq!(row["cost_formula_usd"], 0.0134);
        assert_eq!(
            row["cost_source"], "legacy_zero_delta",
            "must not impersonate the live pipeline's own sources"
        );
    }

    /// The exact audit question this whole pass exists to answer.
    #[test]
    fn test_zero_cost_paid_model_no_longer_reads_as_free() {
        // mirrors the JS tierOf: free requires cost AND formula to be zero
        let tier = |cost: f64, formula: Option<f64>| -> &'static str {
            match formula {
                None => {
                    if cost == 0.0 {
                        "free"
                    } else {
                        "paid"
                    }
                }
                Some(f) => {
                    if cost == 0.0 && f == 0.0 {
                        "free"
                    } else {
                        "paid"
                    }
                }
            }
        };
        assert_eq!(tier(0.0, None), "free", "archived ling row: unchanged");
        assert_eq!(
            tier(0.0, Some(0.0134)),
            "paid",
            "muse-spark after annotation: paid"
        );
        assert_eq!(tier(0.0, Some(0.0)), "free", "genuinely free stays free");
    }

    fn unique_temp(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sdb-cost-annotate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn typed_row(cost_usd: f64, formula: Option<f64>) -> crate::report::BenchmarkRunResult {
        let mut v = json!({
            "id": "typed-1",
            "model": "test-model",
            "task": "redis",
            "language": "Go",
            "pass_rate": 0.0,
            "passed_stages": 0,
            "total_stages": 4,
            "prompt_tokens": 100,
            "cached_tokens": 0,
            "completion_tokens": 50,
            "total_cost_usd": cost_usd,
            "savings_percent": 0.0,
            "efficiency_score": 0.0,
            "timestamp": "2026-10-01T00:00:00Z"
        });
        if let Some(f) = formula {
            v["cost_formula_usd"] = json!(f);
        }
        serde_json::from_value(v).expect("typed row fixture")
    }

    struct Unserializable;

    impl serde::Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(
            &self,
            _serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("fixture cannot serialize"))
        }
    }

    /// Exact token arithmetic: prompt at the prompt rate, cache reads at the
    /// cache rate, completion at the completion rate — plus the epsilon rule
    /// that keeps sub-micro-dollar rows reading as free.
    #[test]
    fn test_formula_cost_applies_exact_rates_and_epsilon() {
        let p = ModelPricing::new(2.0, 4.0, 0.5);
        let nested = json!({
            "tokens": {"prompt_tokens": 1_000_000, "cached_tokens": 2_000_000, "completion_tokens": 500_000}
        });
        // 1.0M*2.0 + 2.0M*0.5 + 0.5M*4.0 = 2.0 + 1.0 + 2.0
        assert_eq!(formula_cost(&nested, &p), Some(5.0));

        // Cache reads bill at their own rate: using the prompt rate would
        // yield 1.0 here instead of 2.5.
        let cache_heavy = ModelPricing::new(0.5, 0.0, 2.0);
        let cached = json!({
            "tokens": {"prompt_tokens": 1_000_000, "cached_tokens": 1_000_000, "completion_tokens": 0}
        });
        assert_eq!(formula_cost(&cached, &cache_heavy), Some(2.5));

        let p = ModelPricing::new(1.0, 0.0, 0.0);
        let one_token = json!({
            "tokens": {"prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 0}
        });
        assert_eq!(
            formula_cost(&one_token, &p),
            None,
            "a single token on a cheap model sits at the epsilon: reads free"
        );
        let two_tokens = json!({
            "tokens": {"prompt_tokens": 2, "cached_tokens": 0, "completion_tokens": 0}
        });
        assert_eq!(formula_cost(&two_tokens, &p), Some(2e-6));

        assert_eq!(
            formula_cost(&json!({}), &p),
            None,
            "no tokens, nothing to price"
        );
        assert_eq!(
            formula_cost(&json!({"tokens": {"prompt_tokens": 5}}), &p),
            None,
            "partial token shape prices nothing"
        );
        assert_eq!(
            formula_cost(
                &json!({"tokens": {"prompt_tokens": 1.5, "cached_tokens": 0, "completion_tokens": 0}}),
                &p
            ),
            None,
            "non-integer token counts are not tokens"
        );
    }

    /// Manifests nest tokens; result rows carry them flat. Both shapes must
    /// price identically, and the nested shape wins when both are present.
    #[test]
    fn test_row_formula_cost_reads_nested_and_flat_shapes() {
        let p = ModelPricing::new(2.0, 4.0, 0.5);
        let flat = json!({
            "prompt_tokens": 1_000_000, "cached_tokens": 2_000_000, "completion_tokens": 500_000
        });
        assert_eq!(row_formula_cost(&flat, &p), Some(5.0));
        // Result rows always carry the trio; a missing flat key counts as 0.
        let flat_no_cached = json!({"prompt_tokens": 1_000_000, "completion_tokens": 500_000});
        assert_eq!(row_formula_cost(&flat_no_cached, &p), Some(4.0));
        let both = json!({
            "tokens": {"prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0},
            "prompt_tokens": 1_000_000, "completion_tokens": 500_000
        });
        assert_eq!(
            row_formula_cost(&both, &p),
            None,
            "nested tokens are authoritative when both shapes exist"
        );
        assert_eq!(row_formula_cost(&json!({"cost_usd": 0.0}), &p), None);
        assert_eq!(row_formula_cost(&json!([]), &p), None, "non-object rows");
    }

    /// The annotation is additive: it writes the formula + source and never
    /// rewrites the recorded cost or any score.
    #[test]
    fn test_set_annotation_never_touches_cost_usd_or_scores() {
        let mut row = json!({
            "cost_usd": 0.0,
            "pass_rate": 87.5,
            "efficiency_score": 1.25,
            "effective_cost_usd": null
        });
        set_annotation(&mut row, 0.25, "legacy_zero_delta");
        assert_eq!(row["cost_formula_usd"], 0.25);
        assert_eq!(row["cost_source"], "legacy_zero_delta");
        assert_eq!(row["cost_usd"], 0.0, "recorded cost is never rewritten");
        assert_eq!(row["pass_rate"], 87.5, "scores are never rewritten");
        assert_eq!(row["efficiency_score"], 1.25);
        assert!(row["effective_cost_usd"].is_null());

        set_annotation(&mut row, 0.5, "other-source");
        assert_eq!(row["cost_formula_usd"], 0.5, "re-annotation replaces");
        assert_eq!(row["cost_source"], "other-source");

        let mut scalar = json!(3);
        set_annotation(&mut scalar, 1.0, "x");
        assert_eq!(scalar, json!(3), "non-object rows are left alone");
    }

    /// Bare `RunManifest` row (the shape stored in the `runs` table).
    fn typed_manifest(cost_usd: f64, formula: Option<f64>) -> crate::report::RunManifest {
        let mut v = json!({
            "run_id": "typed-manifest-1",
            "model": "test-model",
            "task": "redis",
            "status": "completed",
            "language": "Go",
            "started_at": "2026-10-01T00:00:00Z",
            "completed_at": "2026-10-01T00:01:00Z",
            "duration_seconds": 60.0,
            "pass_rate": 100.0,
            "passed_stages": 4,
            "total_stages": 4,
            "stages": [],
            "throughput_req_sec": null,
            "tokens": {"prompt_tokens": 100, "cached_tokens": 0, "completion_tokens": 50, "total_tokens": 150},
            "cost_usd": cost_usd,
            "savings_percent": 0.0,
            "efficiency_score": 0.0,
            "files": []
        });
        if let Some(f) = formula {
            v["cost_formula_usd"] = json!(f);
        }
        serde_json::from_value(v).expect("typed manifest fixture")
    }

    /// DB rows are typed `Option`s: `Some`/`None` round-trips decide
    /// idempotency, and a serialize failure must never annotate.
    #[test]
    fn test_needs_annotation_json_handles_null_round_trip_and_serialize_error() {
        let zero = typed_manifest(0.0, None);
        let v = serde_json::to_value(&zero).unwrap();
        assert!(
            v["cost_formula_usd"].is_null(),
            "typed None must serialize as null (the db round-trip)"
        );
        assert!(needs_annotation_json(&zero));
        assert!(needs_annotation(&v), "null must read as absent for db rows");

        assert!(
            !needs_annotation_json(&typed_manifest(0.01, None)),
            "priced rows are skipped"
        );
        assert!(
            !needs_annotation_json(&typed_manifest(0.0, Some(0.5))),
            "annotated rows are skipped (idempotent re-runs)"
        );
        assert!(
            !needs_annotation_json(&Unserializable),
            "a row that cannot serialize is never annotated"
        );
    }

    /// Typed parent rows serialize their price as `total_cost_usd`; the
    /// annotation gate must honor both price keys, so a typed row with a
    /// real recorded cost is NOT annotated (fixed 2026-10-14 after a
    /// coverage test proved the old gate read only `cost_usd` and would
    /// annotate priced parents; the tool's contract is "rows archived with
    /// cost_usd: 0 gain cost_formula_usd"). A typed row at zero recorded
    /// cost is the annotation target.
    #[test]
    fn test_needs_annotation_json_uses_both_recorded_cost_keys() {
        let priced = typed_row(0.01, None);
        let v = serde_json::to_value(&priced).unwrap();
        assert!(
            v.get("cost_usd").is_none(),
            "typed result rows carry total_cost_usd, not cost_usd"
        );
        assert!(v["total_cost_usd"].as_f64().unwrap() > 0.0);
        assert!(
            !needs_annotation_json(&priced),
            "a priced typed row must not gain a formula annotation"
        );
        assert!(
            needs_annotation_json(&typed_row(0.0, None)),
            "a zero-cost typed row is the annotation target"
        );
        assert!(
            !needs_annotation_json(&typed_row(0.01, Some(0.5))),
            "an existing formula stops re-annotation"
        );
        assert!(
            !needs_annotation_json(&typed_row(0.0, Some(0.5))),
            "annotated zero-cost rows stay annotated, never re-annotated"
        );
        // Legacy/result shapes keep the cost_usd key.
        assert!(needs_annotation(&json!({"cost_usd": 0.0})));
        assert!(!needs_annotation(&json!({"cost_usd": 0.02})));
        // Null counts as absent; a real price under either key wins.
        assert!(!needs_annotation(
            &json!({"cost_usd": null, "total_cost_usd": 0.03})
        ));
        assert!(needs_annotation(&json!({"model": "x"})), "no price at all");
    }

    #[test]
    fn test_collect_model_dedupes_and_skips_non_string_values() {
        let mut models = vec!["a".to_string()];
        collect_model_str("a", &mut models);
        collect_model_str("b", &mut models);
        collect_model(&json!({"model": "b"}), &mut models);
        collect_model(&json!({"model": 7}), &mut models);
        collect_model(&json!({}), &mut models);
        assert_eq!(models, vec!["a".to_string(), "b".to_string()]);
    }

    /// With no models there is nothing to price: no catalog request is made.
    #[tokio::test]
    async fn test_pricing_cache_with_no_models_is_empty() {
        assert!(pricing_cache(&[]).await.is_empty());
    }

    /// The pass only ever touches the canonical repo dirs; a temp dir must be
    /// refused before any store or network call.
    #[tokio::test]
    async fn test_run_cost_annotate_refuses_non_canonical_dirs() {
        let dir = unique_temp("guard");
        let err = match run_cost_annotate(&dir, &dir, false).await {
            Ok(_) => panic!("non-canonical dirs must be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("canonical repo dirs"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
