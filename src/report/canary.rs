//! Benchmark canaries: unique marker strings embedded in every graded task
//! prompt, with a detector that scans archived workspaces for echoes.
//!
//! Why: an autonomous benchmark whose prompts circulate on the open web is
//! trainable. A unique, fixed marker per task lets us (a) detect benchmark
//! prompt text leaking into a candidate's output — a model quoting the
//! prompt verbatim is reproducing benchmark material, and (b) identify this
//! corpus if the markers ever surface in a scraped training set. The markers
//! are chosen once and frozen; rotating one is a RULES_VERSION event (the
//! prompt hash, and therefore the fingerprint, changes).
//!
//! Detection is advisory, never disqualifying: a model can legitimately
//! quote the brief it was handed. An echo is contamination *evidence* to
//! report, not a verdict to enforce.

use std::path::Path;

/// Marker prefix every canary shares (greppable in corpora).
pub const CANARY_PREFIX: &str = "sdb-canary";

/// The exact footer line appended to every graded prompt. Kept here so the
/// detectors and the docs agree with what the templates carry.
pub fn canary_footer(task: &str) -> String {
    let guid = canary_guid(task).unwrap_or("sdb-canary-UNREGISTERED");
    format!("<!-- {guid} · benchmark data; this canary identifies the SubDollarBench brief -->")
}

/// Frozen per-task markers. Never reuse a value across tasks: an echo then
/// identifies *which* brief leaked, not just that one did.
pub fn canary_guid(task: &str) -> Option<&'static str> {
    Some(match task.to_lowercase().as_str() {
        "redis" => "sdb-canary-redis-4f9d2a71",
        "http" => "sdb-canary-http-b83c05e6",
        "pdf" => "sdb-canary-pdf-1d7b64c9",
        "proxy" => "sdb-canary-proxy-a2e58d03",
        "pubsub" => "sdb-canary-pubsub-6c40f1b8",
        "wal" => "sdb-canary-wal-9b3e7c25",
        "binproto" => "sdb-canary-binproto-58af13d4",
        _ => return None,
    })
}

/// All registered canaries, as `(task, guid)`.
pub fn all_canaries() -> Vec<(&'static str, &'static str)> {
    TASKS
        .iter()
        .filter_map(|t| canary_guid(t).map(|g| (*t, g)))
        .collect()
}

const TASKS: &[&str] = &["redis", "http", "pdf", "proxy", "pubsub", "wal", "binproto"];

/// How to treat an echo when one is found. Advisory by design: the record
/// says what was seen, the human decides what it means.
pub const CANARY_POLICY: &str = "advisory";

/// Scan an archived candidate workspace for canary echoes.
///
/// Text files only (source, logs, notes); binary blobs are skipped. A hit
/// returns `(relative_path, guid)`. Bounded by file count and size so a
/// pathological workspace cannot stall a run.
pub fn scan_workspace_for_canary(workspace: &Path) -> Vec<(String, String)> {
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // Never descend into vendored deps: a marker there means a
                // dependency ships the prompt, which is not the candidate
                // echoing it.
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                if matches!(
                    name,
                    "node_modules" | ".git" | "target" | "vendor" | "__pycache__" | ".venv"
                ) {
                    continue;
                }
                stack.push(p);
                continue;
            }
            scanned += 1;
            if scanned > MAX_SCANNED_FILES {
                return hits;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            // Skip anything that is not plausibly text.
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            for (task, guid) in all_canaries() {
                if text.contains(guid) {
                    let rel = p
                        .strip_prefix(workspace)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .to_string();
                    hits.push((rel, guid.to_string()));
                    let _ = task;
                }
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

const MAX_SCANNED_FILES: usize = 20_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canaries_are_unique_and_task_scoped() {
        let guids: Vec<&str> = all_canaries().iter().map(|(_, g)| *g).collect();
        let mut sorted = guids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), guids.len(), "canaries must be unique");
        for (task, guid) in all_canaries() {
            assert!(
                guid.contains(task),
                "canary for {task} must name the task: {guid}"
            );
        }
    }

    #[test]
    fn test_unregistered_task_has_no_guid() {
        assert_eq!(canary_guid("dns"), None, "retired tasks carry no canary");
        assert_eq!(canary_guid("nonsense"), None);
    }

    /// The canary only works if the prompt actually carries it. A prompt
    /// edit that drops (or rewords) the footer silently disables detection
    /// for that task, so assert the templates match the registry verbatim.

    #[test]
    fn test_footer_carries_the_marker() {
        let f = canary_footer("redis");
        assert!(f.starts_with("<!-- sdb-canary-redis-4f9d2a71"));
        assert!(f.ends_with("-->"));
    }

    #[test]
    fn test_scan_finds_echo_in_text_file() {
        let dir = std::env::temp_dir().join(format!("canary_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "// hello\nfn main() {}\n").unwrap();
        std::fs::write(
            dir.join("notes.md"),
            format!("the brief said: {}\n", canary_guid("pdf").unwrap()),
        )
        .unwrap();
        std::fs::write(
            dir.join("blob.bin"),
            [0u8, 1, 2, canary_guid("pdf").unwrap().as_bytes()[0]],
        )
        .unwrap();

        let hits = scan_workspace_for_canary(&dir);
        assert_eq!(hits.len(), 1, "only the text file echoes: {hits:?}");
        assert!(hits[0].0.contains("notes.md"));
        assert_eq!(hits[0].1, canary_guid("pdf").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_skips_vendored_dirs() {
        let dir = std::env::temp_dir().join(format!("canary_vendor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules").join("pkg")).unwrap();
        std::fs::write(
            dir.join("node_modules").join("pkg").join("index.js"),
            format!("const c = \"{}\";", canary_guid("wal").unwrap()),
        )
        .unwrap();
        let hits = scan_workspace_for_canary(&dir);
        assert!(hits.is_empty(), "vendored deps are not candidate echoes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
