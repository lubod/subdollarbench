//! R74 F1: one health counter for a phase's `live.log`.
//!
//! The same numbers feed the grid's live check (F2), `scripts/run-audit.sh`
//! (F3), the console (F5) and the phase's `campaign.json`. The parser is
//! deliberately line-oriented: the campaign logger writes the agent's
//! `[BENCH-POLICY]` telemetry, its tool calls (`$ ...` / `> ...`) and the
//! provider's errors to `live.log` exactly as they happened.
//!
//! Rules (calibrated on the real 2026-11-08 minimax and luna-pro logs):
//! - `turns`: one per `[BENCH-POLICY] turn=` line.
//! - `tool_errors`: one per `provider stream ended with an error` /
//!   `Upstream error from` line.
//! - `ref_queries`: one per tool call whose command names `ref-sql`.
//!   `ref_refused`: one per such call whose output contains
//!   `Connection refused`. A candidate's own `127.0.0.1` check is not a
//!   `ref-sql` query, so it is not counted.
//! - `turn_secs_p50`/`max`: the deltas between the session start
//!   (`[BENCH-POLICY] active maxTurns=`) and each turn marker, reset at
//!   each session start (a new ticket).
//! - `empty_responses`: a turn interval with no `[RESPONSE]` text and no
//!   `[ACTION]`/`$` tool call.
//! - `billed_ratio`: billed / list; set by the rollup writer, not the log.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PhaseHealth {
    pub turns: u64,
    pub tool_errors: u64,
    pub ref_queries: u64,
    pub ref_refused: u64,
    pub turn_secs_p50: f64,
    pub turn_secs_max: f64,
    pub empty_responses: u64,
    /// R77 F2: the transient 429s omp retried (`Retrying (n/m)`, n < m).
    #[serde(default)]
    pub rate_limited: u64,
    /// billed / list. `None` while the phase is live or when either side is
    /// zero/unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billed_ratio: Option<f64>,
}

impl PhaseHealth {
    pub fn ref_refused_ratio(&self) -> f64 {
        if self.ref_queries == 0 {
            0.0
        } else {
            self.ref_refused as f64 / self.ref_queries as f64
        }
    }

    pub fn tool_error_ratio(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.tool_errors as f64 / self.turns as f64
        }
    }

    pub fn empty_ratio(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.empty_responses as f64 / self.turns as f64
        }
    }

    /// R74 F5: the console's health colour against F2's thresholds. Red
    /// is a provider fault (the live check stops it); amber is a warning
    /// (a slow median turn or too many ref-sql refusals); green otherwise.
    pub fn severity(&self) -> &'static str {
        let tool = self.tool_errors >= 5 && self.tool_error_ratio() > 0.02;
        let empty = self.empty_responses >= 5 && self.empty_ratio() > 0.05;
        let billed = self.billed_ratio.map(|r| r > 1.25).unwrap_or(false);
        if tool || empty || billed {
            "red"
        } else if self.turn_secs_p50 > 180.0 || self.ref_refused_ratio() > 0.10 {
            "amber"
        } else {
            "green"
        }
    }

    /// The compact health cell's title: every number F1 records.
    pub fn title(&self) -> String {
        let billed = match self.billed_ratio {
            Some(r) => format!("{r:.3}"),
            None => "—".to_string(),
        };
        format!(
            "{} turns · {} tool errors · ref {}/{} · turn p50 {:.0}s max {:.0}s · {} empty · {} rate limited · billed/list {}",
            self.turns,
            self.tool_errors,
            self.ref_refused,
            self.ref_queries,
            self.turn_secs_p50,
            self.turn_secs_max,
            self.empty_responses,
            self.rate_limited,
            billed
        )
    }
}

fn clock(line: &str) -> Option<u64> {
    // `[HH:MM:SS] ...`
    let rest = line.strip_prefix('[')?;
    let (hms, _) = rest.split_once(']')?;
    let mut it = hms.split(':');
    let h: u64 = it.next()?.parse().ok()?;
    let m: u64 = it.next()?.parse().ok()?;
    let s: u64 = it.next()?.parse().ok()?;
    Some(h * 3600 + m * 60 + s)
}

fn body(line: &str) -> &str {
    match line.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .map(|(_, b)| b.trim_start())
            .unwrap_or(line),
        None => line,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Parse one phase's `live.log` text.
pub fn health_from_live_log(text: &str) -> PhaseHealth {
    let mut h = PhaseHealth::default();
    let mut session_start: Option<u64> = None;
    let mut last_turn: Option<u64> = None;
    let mut deltas: Vec<f64> = Vec::new();
    let mut ref_block = false;
    let mut ref_refused_in_block = false;
    let mut has_response = false;
    let mut has_tool = false;

    for line in text.lines() {
        if line.contains("[BENCH-POLICY] active maxTurns=") {
            // A new ticket/session: the turn clock restarts and the first
            // turn's duration is measured from here.
            session_start = clock(line);
            last_turn = None;
            ref_block = false;
            ref_refused_in_block = false;
            has_response = false;
            has_tool = false;
            continue;
        }
        if line.contains("[BENCH-POLICY] turn=") {
            h.turns += 1;
            let now = clock(line).or(last_turn).or(session_start);
            if let (Some(prev), Some(now)) = (last_turn.or(session_start), now) {
                deltas.push(((now + 86_400 - prev) % 86_400) as f64);
            }
            last_turn = now;
            if !has_response && !has_tool {
                h.empty_responses += 1;
            }
            has_response = false;
            has_tool = false;
            ref_block = false;
            ref_refused_in_block = false;
            continue;
        }
        if line.contains("provider stream ended with an error")
            || line.contains("Upstream error from")
        {
            h.tool_errors += 1;
        }
        let b = body(line);
        // R77 F2: a transient 429 (omp's retry line with n < m) is
        // counted; the classifier is the one rule F1 uses. Only lines
        // with a retry marker can be transient, so the JSON parse in the
        // classifier is not paid for every log line.
        if b.starts_with("[STDERR]")
            && b.contains("Retrying (")
            && matches!(
                crate::sandbox::omp::classify_billing_error(b),
                Some(crate::sandbox::omp::BillingSignal::RateLimited)
            )
        {
            h.rate_limited += 1;
        }
        if b.starts_with("$ ") {
            ref_block = b.contains("ref-sql");
            ref_refused_in_block = false;
            has_tool = true;
            if ref_block {
                h.ref_queries += 1;
            }
        } else if b.starts_with("> ") {
            if ref_block && !ref_refused_in_block && b.contains("Connection refused") {
                h.ref_refused += 1;
                ref_refused_in_block = true;
            }
        } else if b.starts_with("[RESPONSE]")
            || b.starts_with("[OMP]")
            || b.starts_with("[BENCH-EVENT]")
        {
            has_response = true;
        } else if b.starts_with("[ACTION]") {
            has_tool = true;
        }
    }

    h.turn_secs_max = deltas.iter().copied().fold(0.0, f64::max);
    h.turn_secs_p50 = median(deltas);
    h
}

/// Read a finished (or live) phase directory. The billed ratio is known
/// only once the rollup has both sides.
pub fn health_for_run_dir(dir: &Path, list_usd: f64, billed_usd: f64) -> PhaseHealth {
    let text = std::fs::read_to_string(dir.join("live.log")).unwrap_or_default();
    let mut h = health_from_live_log(&text);
    if list_usd > 0.0 && billed_usd > 0.0 {
        h.billed_ratio = Some(billed_usd / list_usd);
    }
    h
}
