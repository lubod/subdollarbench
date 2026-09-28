use anyhow::Result;
use parking_lot::Mutex;
use tracing::warn;
use rusqlite::{params, Connection};
use std::path::Path;

use super::archive::RunManifest;
use super::leaderboard::BenchmarkRunResult;
use crate::web::server::{QueueStatus, QueuedRun};

/// SQLite-backed primary store for consolidated benchmark results.
///
/// One row per consolidated run in `parents`, one row per trial in `trials`.
/// The full [`BenchmarkRunResult`] JSON lives in `doc`; the other columns are
/// queryable projections of the same data. `results/*.json` files remain as
/// export copies for one release.
///
/// The `queue` table mirrors the in-memory web queue so pending work survives
/// restarts (memory stays dispatch truth; the DB is the durability mirror).
/// The `runs` table indexes per-trial `runs/*/manifest.json` records; logs
/// and workspaces stay on the filesystem.
pub struct Store {
    conn: Mutex<Connection>,
}

fn queue_status_str(status: &QueueStatus) -> &'static str {
    match status {
        QueueStatus::Pending => "Pending",
        QueueStatus::Running => "Running",
        QueueStatus::Done => "Done",
        QueueStatus::Cancelled => "Cancelled",
    }
}

impl Store {
    /// Primary-store path shared by every `Store::open` callsite:
    /// `$SDB_BENCH_DB` override (isolated tests) else repo-root bench.db.
    /// Blank values fall through to the repo root.
    pub fn resolve_db_path() -> std::path::PathBuf {
        match std::env::var("SDB_BENCH_DB") {
            Ok(p) if !p.trim().is_empty() => std::path::PathBuf::from(p),
            _ => crate::config::get_repo_root().join("bench.db"),
        }
    }

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
            );
            CREATE TABLE IF NOT EXISTS queue (
                id TEXT PRIMARY KEY,
                status TEXT,
                enqueued_at TEXT,
                started_at TEXT,
                finished_at TEXT,
                doc TEXT
            );
            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                model TEXT,
                task TEXT,
                status TEXT,
                pass_rate REAL,
                cost_usd REAL,
                duration_s REAL,
                completed_at TEXT,
                prompt_sha TEXT,
                throughput REAL,
                reference_tp REAL,
                doc TEXT
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert or replace the parent row plus one `trials` row per subrun
    /// (`trial_no` = subrun index + 1) in a single transaction.
    ///
    /// Per-trial `status` has no dedicated field on [`BenchmarkRunResult`],
    /// so it is derived from the subrun's own signal: `pass_rate >= 100.0`
    /// stores `"passed"`, anything lower stores `"failed"`. Rows written
    /// before this derivation keep `""` until re-migrated.
    /// Per-subrun pass/fail verdict used for `trials.status` (see
    /// [`Store::upsert_parent`]): full pass rate means passed, else failed.
    pub fn trial_status(pass_rate: f64) -> &'static str {
        if pass_rate >= 100.0 {
            "passed"
        } else {
            "failed"
        }
    }

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
                    Self::trial_status(sub.pass_rate),
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
        let mut stmt = match conn.prepare("SELECT id, doc FROM parents") {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows
            .filter_map(|row| row.ok())
            .filter_map(|(id, doc)| match serde_json::from_str(&doc) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!("store: skipping corrupt parent row {id}: {e}");
                    None
                }
            })
            .collect()
    }

    pub fn parent_count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM parents", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }

    /// Orphan trials: rows whose parent row is missing (double-spawn
    /// debris, killed-run leftovers). migrate --check reports them.
    pub fn orphan_trial_count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM trials WHERE parent_id NOT IN (SELECT id FROM parents)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    }

    /// Junk parents: test_ probes and 0-trial rows that slipped past the
    /// write-time guards. migrate --check reports them; finalize filters them.
    pub fn junk_parent_count(&self) -> usize {
        let conn = self.conn.lock();
        let test_rows: i64 = conn
            .query_row(r"SELECT COUNT(*) FROM parents WHERE id LIKE 'test\_%' ESCAPE '\'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0);
        // 0-trial rows: no subruns evidence. Doc scan (small table, rare op).
        let mut zero_trial = 0i64;
        if let Ok(mut stmt) = conn.prepare("SELECT doc FROM parents") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for row in rows.flatten() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row) {
                        let n = v.get("total_trials").and_then(|x| x.as_u64()).unwrap_or(
                            v.get("subruns").and_then(|x| x.as_array()).map(|a| a.len() as u64).unwrap_or(0),
                        );
                        if n == 0 {
                            zero_trial += 1;
                        }
                    }
                }
            }
        }
        (test_rows + zero_trial) as usize
    }

    /// Row-file drift: (db parents, results/*.json files). Migrate --check
    /// reports mismatch; files stay authoritative.
    pub fn parent_file_drift(&self, results_dir: &std::path::Path) -> (usize, usize) {
        let db = self.parent_count();
        let files = std::fs::read_dir(results_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                    .count()
            })
            .unwrap_or(0);
        (db, files)
    }

    /// True when a parent row with this id exists (migrate delta check).
    pub fn parent_exists(&self, id: &str) -> bool {
        let conn = self.conn.lock();
        conn.query_row("SELECT 1 FROM parents WHERE id = ?1", params![id], |_| Ok(()))
            .is_ok()
    }

    /// Mirror a freshly enqueued item; dispatch-time fields start NULL.
    pub fn enqueue_run(&self, q: &QueuedRun) -> Result<()> {
        let doc = serde_json::to_string(q)?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO queue
                (id, status, enqueued_at, started_at, finished_at, doc)
             VALUES (?1, ?2, ?3, NULL, NULL, ?4)",
            params![q.id, queue_status_str(&q.status), q.enqueued_at, doc],
        )?;
        Ok(())
    }

    /// Mirror a dispatch/completion transition. `started_at`/`finished_at`
    /// pass through only when `Some`; a `None` keeps the stored value.
    pub fn update_queue_status(
        &self,
        id: &str,
        status: &str,
        started_at: Option<&str>,
        finished_at: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE queue SET status = ?2,
                started_at = COALESCE(?3, started_at),
                finished_at = COALESCE(?4, finished_at)
             WHERE id = ?1",
            params![id, status, started_at, finished_at],
        )?;
        Ok(())
    }

    /// Load queued items in enqueue order by parsing the stored `doc` JSON;
    /// bad rows are skipped. Without `include_done`, finished history
    /// (`Done`, `Cancelled`) is excluded — the replay path for restarts.
    pub fn load_queue(&self, include_done: bool) -> Vec<QueuedRun> {
        let conn = self.conn.lock();
        let sql = if include_done {
            "SELECT doc FROM queue ORDER BY rowid"
        } else {
            "SELECT doc FROM queue WHERE status NOT IN ('Done', 'Cancelled') ORDER BY rowid"
        };
        let mut stmt = match conn.prepare(sql) {
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

    /// Drop one queue row (used when a pending item is removed or the
    /// in-memory history bound evicts a finished entry).
    pub fn remove_queued(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM queue WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Delete finished history (`Done`, `Cancelled`); returns rows removed.
    pub fn clear_finished_queue(&self) -> Result<usize> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM queue WHERE status IN ('Done', 'Cancelled')",
            [],
        )?;
        Ok(conn.changes() as usize)
    }

    /// Index one `runs/*/manifest.json` record; logs/workspaces stay on FS.
    pub fn upsert_run_manifest(&self, m: &RunManifest) -> Result<()> {
        let doc = serde_json::to_string(m)?;
        let conn = self.conn.lock();
        // Lazily add probe-era columns to pre-existing DBs.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN prompt_sha TEXT", []);
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN throughput REAL", []);
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN reference_tp REAL", []);
        conn.execute(
            "INSERT OR REPLACE INTO runs
                (run_id, model, task, status, pass_rate, cost_usd, duration_s, completed_at, prompt_sha, throughput, reference_tp, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                m.run_id,
                m.model,
                m.task,
                m.status,
                m.pass_rate,
                m.cost_usd,
                m.duration_seconds,
                m.completed_at,
                m.prompt_sha,
                m.throughput_req_sec,
                m.reference_throughput_req_sec,
                doc,
            ],
        )?;
        Ok(())
    }

    /// Load every indexed run manifest (`runs` table), newest first by
    /// `completed_at`; bad rows are skipped. Empty until `migrate` runs.
    pub fn load_run_manifests(&self) -> Vec<RunManifest> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare("SELECT run_id, doc FROM runs ORDER BY completed_at DESC") {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows
            .filter_map(|row| row.ok())
            .filter_map(|(id, doc)| match serde_json::from_str(&doc) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!("store: skipping corrupt run row {id}: {e}");
                    None
                }
            })
            .collect()
    }

    /// Fetch one indexed run manifest by id; None when absent or corrupt.
    pub fn get_run_manifest(&self, run_id: &str) -> Option<RunManifest> {
        let conn = self.conn.lock();
        let doc: String = conn
            .query_row(
                "SELECT doc FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .ok()?;
        serde_json::from_str(&doc).ok()
    }

    pub fn run_manifest_count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM runs", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }

    /// Delete one indexed run manifest row; missing rows are a no-op.
    pub fn delete_run_manifest(&self, run_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM runs WHERE run_id = ?1", params![run_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::server::{QueueKind, RunRequest};

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

    fn sample_queue_item(id: &str) -> QueuedRun {
        QueuedRun {
            id: id.to_string(),
            enqueued_at: "2026-09-25T00:00:00Z".to_string(),
            status: QueueStatus::Pending,
            kind: QueueKind::Run(RunRequest {
                model: "test-model".to_string(),
                task: "dns".to_string(),
                api_key: None,
                budget_usd: 0.10,
                max_turns: 5,
                eval_only: true,
                effort: Some("low".to_string()),
                timeout_min: Some(1),
                trials: 1,
                extend_if_unstable: false,
                max_trials: 9,
                save_results: None,
                review_model: None,
            }),
        }
    }

    fn sample_manifest(run_id: &str) -> RunManifest {
        serde_json::from_value(serde_json::json!({
            "run_id": run_id,
            "model": "test-model",
            "task": "dns",
            "status": "passed",
            "language": "Go",
            "started_at": "2026-09-25T00:00:00Z",
            "completed_at": "2026-09-25T00:01:00Z",
            "duration_seconds": 60.0,
            "pass_rate": 100.0,
            "passed_stages": 6,
            "total_stages": 6,
            "stages": [],
            "throughput_req_sec": null,
            "tokens": {
                "prompt_tokens": 100,
                "cached_tokens": 0,
                "completion_tokens": 50,
                "total_tokens": 150
            },
            "cost_usd": 0.01,
            "savings_percent": 90.0,
            "efficiency_score": 1.0,
            "files": []
        }))
        .expect("sample manifest")
    }

    #[test]
    fn round_trip_parent_with_subruns() {
        let db = std::env::temp_dir().join(format!("store-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Store::open(&db).expect("open");
        let mut parent = sample_result("parent-1");
        let mut failing = sample_result("trial-2");
        failing.pass_rate = 50.0;
        parent.subruns = vec![
            sample_result("trial-1"),
            failing,
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
        let statuses: Vec<String> = store
            .conn
            .lock()
            .prepare("SELECT status FROM trials WHERE parent_id = 'parent-1' ORDER BY trial_no")
            .expect("prepare statuses")
            .query_map([], |row| row.get(0))
            .expect("query statuses")
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            statuses,
            vec![
                "passed".to_string(),
                "failed".to_string(),
                "passed".to_string()
            ]
        );
        // Delete removes the parent and its trials.
        store.delete_parent("parent-1").expect("delete");
        assert_eq!(store.parent_count(), 0);
        assert!(store.load_all_parents().is_empty());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn queue_lifecycle_and_runs_index() {
        let db = std::env::temp_dir().join(format!("store-queue-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Store::open(&db).expect("open");
        assert!(store.load_queue(false).is_empty());

        store.enqueue_run(&sample_queue_item("q1")).expect("enqueue q1");
        store.enqueue_run(&sample_queue_item("q2")).expect("enqueue q2");
        let pending = store.load_queue(false);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, "q1");
        assert_eq!(pending[1].id, "q2");
        assert!(pending.iter().all(|q| q.status == QueueStatus::Pending));

        store
            .update_queue_status("q1", "Running", Some("2026-09-25T00:00:01Z"), None)
            .expect("mark running");
        store
            .update_queue_status("q1", "Done", None, Some("2026-09-25T00:02:00Z"))
            .expect("mark done");
        // Pending-only load hides the finished item; full load keeps it.
        let pending = store.load_queue(false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "q2");
        assert_eq!(store.load_queue(true).len(), 2);

        store.remove_queued("q2").expect("remove q2");
        assert!(store.load_queue(false).is_empty());
        let cleared = store.clear_finished_queue().expect("clear finished");
        assert_eq!(cleared, 1);
        assert!(store.load_queue(true).is_empty());

        let manifest = sample_manifest("run-1");
        store.upsert_run_manifest(&manifest).expect("upsert run");
        assert_eq!(store.run_manifest_count(), 1);
        // Re-upsert replaces without duplicating.
        store.upsert_run_manifest(&manifest).expect("re-upsert run");
        assert_eq!(store.run_manifest_count(), 1);
        // Readers serve the indexed rows; unknown ids miss.
        let got = store.get_run_manifest("run-1").expect("get run-1");
        assert_eq!(
            serde_json::to_value(&got).expect("to_value"),
            serde_json::to_value(&manifest).expect("to_value")
        );
        assert!(store.get_run_manifest("no-such-run").is_none());
        let all = store.load_run_manifests();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].run_id, "run-1");
        // Delete removes the row; deleting a missing id is a no-op.
        store.delete_run_manifest("run-1").expect("delete run-1");
        assert_eq!(store.run_manifest_count(), 0);
        assert!(store.get_run_manifest("run-1").is_none());
        store.delete_run_manifest("no-such-run").expect("delete missing ok");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn corrupt_rows_skipped_others_load() {
        let db = std::env::temp_dir().join(format!("store-corrupt-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Store::open(&db).expect("open");
        store.upsert_parent(&sample_result("good-1")).expect("upsert");
        store.upsert_parent(&sample_result("good-2")).expect("upsert");
        store
            .conn
            .lock()
            .execute("INSERT INTO parents (id, doc) VALUES ('bad-1', '{not json')", [])
            .expect("inject bad parent");
        assert!(store.parent_exists("good-1"));
        assert!(!store.parent_exists("nope"));
        let loaded = store.load_all_parents();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|r| r.id == "good-1"));
        assert!(loaded.iter().any(|r| r.id == "good-2"));

        store
            .upsert_run_manifest(&sample_manifest("run-ok"))
            .expect("upsert run");
        store
            .conn
            .lock()
            .execute("INSERT INTO runs (run_id, doc) VALUES ('run-bad', 'zzz')", [])
            .expect("inject bad run");
        let runs = store.load_run_manifests();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "run-ok");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn migrate_delta_idempotent_with_preexisting() {
        // Simulates `migrate --yes`: delta (after - before) must equal the
        // number of newly added ids, both with pre-existing rows and on re-run.
        let db = std::env::temp_dir().join(format!("store-delta-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Store::open(&db).expect("open");
        store.upsert_parent(&sample_result("pre-existing")).expect("upsert");
        let before = store.parent_count();

        let file_runs = vec![sample_result("pre-existing"), sample_result("new-1")];
        let mut added = 0usize;
        for run in &file_runs {
            if !store.parent_exists(&run.id) {
                added += 1;
            }
            store.upsert_parent(run).expect("upsert");
        }
        assert_eq!(added, 1);
        assert_eq!(store.parent_count() - before, added);

        // Re-run over the same files: nothing new, delta zero.
        let before2 = store.parent_count();
        let mut added2 = 0usize;
        for run in &file_runs {
            if !store.parent_exists(&run.id) {
                added2 += 1;
            }
            store.upsert_parent(run).expect("upsert");
        }
        assert_eq!(added2, 0);
        assert_eq!(store.parent_count() - before2, added2);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn resolve_db_path_respects_env_override() {
        let custom = std::env::temp_dir().join("sdb-custom-override.db");
        std::env::set_var("SDB_BENCH_DB", &custom);
        assert_eq!(Store::resolve_db_path(), custom);
        std::env::set_var("SDB_BENCH_DB", "   ");
        assert_eq!(
            Store::resolve_db_path(),
            crate::config::get_repo_root().join("bench.db")
        );
        std::env::remove_var("SDB_BENCH_DB");
        assert_eq!(
            Store::resolve_db_path(),
            crate::config::get_repo_root().join("bench.db")
        );
    }
}
