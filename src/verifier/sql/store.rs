//! R95 F2: the rollback snapshot of the harness-owned store.
//!
//! A rollback restores the store and the sentinel chain together. Only a
//! store that passed the upgrade (sentinel) check at the end of a passed
//! ticket's verification may become that snapshot: a store that lost data
//! after its checks (the mimo `.51` case) must cost the ticket that saw
//! the loss, never the rest of the career.
//!
//! The snapshot lives beside the store in the campaign directory
//! (`sql-data.snap`, `sql-sentinels.snap`); the chain copy in
//! `main_campaign.rs` carries the same pair into the next phase.

use std::path::{Path, PathBuf};

/// Every `sentinel-<stage>.json` directly under `dir`.
pub fn sentinel_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("sentinel-") && n.ends_with(".json")
                })
                .unwrap_or(false)
        })
        .collect()
}

/// Write the store snapshot a rollback restores, only when this ticket's
/// verification passed and its closing upgrade (sentinel) read passed
/// (`verified`). The sentinel files travel with the store: a store without
/// its sentinels would fail the next ticket's upgrade check, and a store
/// with sentinels it does not hold would restore reads over absent tables.
pub fn snapshot_verified_store(cdir: &Path, verified: bool) -> Result<(), String> {
    if !verified {
        return Ok(());
    }
    let data = cdir.join("sql-data");
    if !data.is_dir() {
        return Ok(());
    }
    let snap = cdir.join("sql-data.snap");
    let sent_snap = cdir.join("sql-sentinels.snap");
    let _ = std::fs::remove_dir_all(&snap);
    crate::verifier::sql::crash::copy_tree(&data, &snap)?;
    let _ = std::fs::remove_dir_all(&sent_snap);
    std::fs::create_dir_all(&sent_snap).map_err(|e| format!("sentinel snapshot: {e}"))?;
    for f in sentinel_files(cdir) {
        if let Some(name) = f.file_name() {
            std::fs::copy(&f, sent_snap.join(name))
                .map_err(|e| format!("sentinel snapshot {}: {e}", f.display()))?;
        }
    }
    Ok(())
}

/// Restore the last verified store and its sentinels for a rollback.
/// `Ok(false)` when no verified snapshot exists yet (the first ticket of a
/// career has none), so the attempt starts from what is there.
///
/// The snapshot files stay in place, so a second retry restores the same
/// pair; only the next passed ticket's verification replaces them.
pub fn restore_store_snapshot(cdir: &Path) -> Result<bool, String> {
    let snap = cdir.join("sql-data.snap");
    if !snap.is_dir() {
        return Ok(false);
    }
    let data = cdir.join("sql-data");
    let _ = std::fs::remove_dir_all(&data);
    crate::verifier::sql::crash::copy_tree(&snap, &data)?;
    // The sentinel chain describes the state this attempt starts from:
    // restoring the store without it would leave later tickets checking
    // reads against data the rollback removed.
    for f in sentinel_files(cdir) {
        let _ = std::fs::remove_file(&f);
    }
    let sent_snap = cdir.join("sql-sentinels.snap");
    for f in sentinel_files(&sent_snap) {
        if let Some(name) = f.file_name() {
            std::fs::copy(&f, cdir.join(name))
                .map_err(|e| format!("restore sentinel {}: {e}", f.display()))?;
        }
    }
    Ok(true)
}
