//! R85 F2: model-level bills for careers that ran before the per-call
//! bill (round 85 F1). Each campaign's `live.log` records the key's spend
//! at its start (`[KEY] initial account-key spend: $X USD`). One model
//! runs at a time and the free track bills $0, so a model's bill is the
//! next model's first reading minus its own (the free track is a $0
//! boundary), or a supplied after-grid reading for the last model. The
//! result is `runs/billed-models.json`, which the board reads; it is a
//! whole-model figure, never split across careers.

use std::path::{Path, PathBuf};

const KEY_PREFIX: &str = "[KEY] initial account-key spend: $";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct KeyReading {
    pub campaign: String,
    pub spend_usd: f64,
    /// The exact log line, so a reader can find it.
    pub line: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ModelBill {
    pub model: String,
    /// None when the last model's grid has no after-grid reading yet.
    pub billed_usd: Option<f64>,
    pub whole_model: bool,
    pub evidence: Vec<KeyReading>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct BilledFile {
    pub source: String,
    pub models: Vec<ModelBill>,
}

struct Campaign {
    dir: String,
    model: String,
    started: String,
    finished: String,
    spend: f64,
    line: String,
}

fn first_key_reading(live_log: &Path) -> Option<(f64, String)> {
    let text = std::fs::read_to_string(live_log).ok()?;
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once(KEY_PREFIX) {
            let spend = rest.split_whitespace().next()?.parse::<f64>().ok()?;
            return Some((spend, line.trim().to_string()));
        }
    }
    None
}

fn read_campaigns(runs: &Path) -> Vec<Campaign> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(runs) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("campaign_") || !e.path().is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path().join("campaign.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if v.get("rules_version").and_then(|r| r.as_str())
            != Some(crate::report::rules::RULES_VERSION)
        {
            continue;
        }
        let model = v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        if model.is_empty() {
            continue;
        }
        let Some((spend, line)) = first_key_reading(&e.path().join("live.log")) else {
            continue;
        };
        out.push(Campaign {
            dir: name,
            model,
            started: v
                .get("started_at")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            finished: v
                .get("finished_at")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            spend,
            line,
        });
    }
    out.sort_by(|a, b| a.started.cmp(&b.started));
    out
}

fn is_free(model: &str) -> bool {
    model.contains(":free")
}

fn reading(c: &Campaign) -> KeyReading {
    KeyReading {
        campaign: c.dir.clone(),
        spend_usd: c.spend,
        line: c.line.clone(),
    }
}

/// Compute each paid model's bill from the `.47` campaigns under `runs`.
/// `after` closes the last model's grid (the key's spend after it ended);
/// without it the last model is left unmeasured (None).
pub fn backfill(runs: &Path, after: Option<f64>) -> Result<Vec<ModelBill>, String> {
    let campaigns = read_campaigns(runs);
    if campaigns.is_empty() {
        return Err("no .47 campaigns with a key reading under the runs dir".to_string());
    }
    // One first reading per model, in start order.
    let mut first: Vec<&Campaign> = Vec::new();
    for c in &campaigns {
        if !first.iter().any(|f| f.model == c.model) {
            first.push(c);
        }
    }
    // One model runs at a time: two paid models' campaigns may not
    // overlap, or the key readings cannot be attributed.
    for (i, a) in campaigns.iter().enumerate() {
        if is_free(&a.model) {
            continue;
        }
        for b in campaigns.iter().skip(i + 1) {
            if is_free(&b.model) || a.model == b.model {
                continue;
            }
            if a.started < b.finished && b.started < a.finished {
                return Err(format!(
                    "two paid models' campaigns overlap in time: {} ({}..{}) and {} ({}..{})",
                    a.model, a.started, a.finished, b.model, b.started, b.finished
                ));
            }
        }
    }
    let mut bills = Vec::new();
    for (i, c) in first.iter().enumerate() {
        if is_free(&c.model) {
            continue;
        }
        let end = match first.get(i + 1) {
            Some(n) => Some((n.spend, n.dir.clone(), n.line.clone())),
            None => after.map(|a| {
                (
                    a,
                    "(after the grid)".to_string(),
                    format!("{KEY_PREFIX}{a:.4} USD (after the grid)"),
                )
            }),
        };
        let Some((end_spend, end_dir, end_line)) = end else {
            bills.push(ModelBill {
                model: c.model.clone(),
                billed_usd: None,
                whole_model: true,
                evidence: vec![reading(c)],
            });
            continue;
        };
        bills.push(ModelBill {
            model: c.model.clone(),
            billed_usd: Some((end_spend - c.spend).max(0.0)),
            whole_model: true,
            evidence: vec![
                reading(c),
                KeyReading {
                    campaign: end_dir,
                    spend_usd: end_spend,
                    line: end_line,
                },
            ],
        });
    }
    Ok(bills)
}

pub fn write(runs: &Path, bills: &[ModelBill]) -> Result<PathBuf, String> {
    let file = BilledFile {
        source: "key readings at each model's first campaign start".to_string(),
        models: bills.to_vec(),
    };
    let path = runs.join("billed-models.json");
    let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// The board's reader: the file, or an empty one when it is absent.
pub fn load(runs: &Path) -> BilledFile {
    std::fs::read_to_string(runs.join("billed-models.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}
