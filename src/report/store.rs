use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::Path;

use super::leaderboard::BenchmarkRunResult;

/// SQLite-backed primary store for consolidated benchmark results.
///
/// One row per consolidated run in `parents`, one row per trial in `trials`.
/// The full [`BenchmarkRunResult`] JSON lives in `doc`; the other columns are
/// queryable projections of the same data. `results/*.json` files remain as
/// export copies for one release.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS parents (
                id TEXT PRIMARY KEY,
                model TEXT,
                task TEXT,
                effort TEXT,
                language TEXT,
                pass_rate REAL,
                avg_pass_rate REAL,
                pass_at_1 REAL,
                stability TEXT,
                efficiency_score REAL,
                cost_usd REAL,
                duration_s REAL,
                total_tokens INTEGER,
                turns INTEGER,
                rules_version TEXT,
                timestamp TEXT,
                doc TEXT
            );
            CREATE TABLE IF NOT EXISTS trials (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                trial_no INTEGER,
                pass_rate REAL,
                status TEXT,
                cost_usd REAL,
                duration_s REAL,
                turns INTEGER,
                doc TEXT
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert or replace the parent row plus one `trials` row per subrun
    /// (`trial_no` = subrun index + 1) in a single transaction.
    pub fn upsert_parent(&self, result: &BenchmarkRunResult) -> Result<()> {
        let doc = serde_json::to_string(result)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO parents
                (id, model, task, effort, language, pass_rate, avg_pass_rate, pass_at_1,
                 stability, efficiency_score, cost_usd, duration_s, total_tokens, turns,
                 rules_version, timestamp, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                result.id,
                result.model,
                result.task,
                result.effort,
                result.language,
                result.pass_rate,
                result.avg_pass_rate,
                result.pass_at_1,
                result.stability,
                result.efficiency_score,
                result.total_cost_usd,
                result.duration_seconds,
                result.total_tokens as i64,
                result.turns.map(|t| t as i64),
                result.rules_version,
                result.timestamp,
                doc,
            ],
        )?;
        tx.execute("DELETE FROM trials WHERE parent_id = ?1", params![result.id])?;
        for (i, sub) in result.subruns.iter().enumerate() {
            let sub_doc = serde_json::to_string(sub)?;
            tx.execute(
                "INSERT OR REPLACE INTO trials
                    (id, parent_id, trial_no, pass_rate, status, cost_usd, duration_s, turns, doc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    sub.id,
                    result.id,
                    i as i64 + 1,
                    sub.pass_rate,
                    "",
                    sub.total_cost_usd,
                    sub.duration_seconds,
                    sub.turns.map(|t| t as i64),
                    sub_doc,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete one parent row and its trials (used when loose trial files are
    /// consolidated and removed from the export copies).
    pub fn delete_parent(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM trials WHERE parent_id = ?1", params![id])?;
        tx.execute("DELETE FROM parents WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Load every parent by parsing the stored `doc` JSON; bad rows are skipped.
    pub fn load_all_parents(&self) -> Vec<BenchmarkRunResult> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare("SELECT doc FROM parents") {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows
            .filter_map(|row| row.ok())
            .filter_map(|doc| serde_json::from_str(&doc).ok())
            .collect()
    }

    pub fn parent_count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM parents", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result(id: &str) -> BenchmarkRunResult {
        BenchmarkRunResult {
            id: id.to_string(),
            model: "test-model".to_string(),
            task: "test-task".to_string(),
            language: "rust".to_string(),
            effort: Some("low".to_string()),
            pass_rate: 100.0,
            passed_stages: 3,
            total_stages: 3,
            throughput_req_sec: None,
            reference_throughput_req_sec: None,
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            prompt_tokens: 100,
            cached_tokens: 0,
            completion_tokens: 50,
            total_tokens: 150,
            total_cost_usd: 0.01,
            effective_cost_usd: None,
            duration_seconds: Some(1.5),
            turns: Some(2),
            savings_percent: 90.0,
            efficiency_score: 1.0,
            throughput_score: None,
            timestamp: "2026-09-25T00:00:00Z".to_string(),
            method_version: None,
            rules_version: Some("test-rules".to_string()),
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
        }
    }

    #[test]
    fn round_trip_parent_with_subruns() {
        let db = std::env::temp_dir().join(format!("store-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Store::open(&db).expect("open");
        let mut parent = sample_result("parent-1");
        parent.subruns = vec![
            sample_result("trial-1"),
            sample_result("trial-2"),
            sample_result("trial-3"),
        ];
        parent.total_trials = Some(3);
        parent.avg_pass_rate = Some(100.0);
        parent.pass_at_1 = Some(100.0);
        parent.stability = Some("stable".to_string());
        store.upsert_parent(&parent).expect("upsert");
        assert_eq!(store.parent_count(), 1);
        let loaded = store.load_all_parents();
        assert_eq!(loaded.len(), 1);
        let want = serde_json::to_value(&parent).expect("to_value");
        let got = serde_json::to_value(&loaded[0]).expect("to_value");
        assert_eq!(got, want);
        // Re-upsert replaces the parent and its trials without duplicating.
        store.upsert_parent(&parent).expect("re-upsert");
        assert_eq!(store.parent_count(), 1);
        let trial_count: i64 = store
            .conn
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM trials WHERE parent_id = 'parent-1'",
                [],
                |row| row.get(0),
            )
            .expect("count trials");
        assert_eq!(trial_count, 3);
        // Delete removes the parent and its trials.
        store.delete_parent("parent-1").expect("delete");
        assert_eq!(store.parent_count(), 0);
        assert!(store.load_all_parents().is_empty());
        let _ = std::fs::remove_file(&db);
    }
}
