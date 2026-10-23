//! Build-time half of the rules stamp (R27 F1).
//!
//! Compiled-in graded inputs are hashed here with SHA-256 and emitted as
//! `SDB_COMPILED_RULES_DIGEST`. Runtime stage prompts under SDB_HOME are
//! hashed by `report::rules::fingerprint_root`; this script deliberately
//! skips the `runtime-dir` entries. `src/report/graded-files.txt` is the
//! one list both halves read.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let manifest = root.join("src/report/graded-files.txt");
    println!("cargo:rerun-if-changed=src/report/graded-files.txt");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));

    let mut files = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rel) = line.strip_prefix("file ") {
            files.push(PathBuf::from(rel));
        } else if let Some(rel) = line.strip_prefix("dir ") {
            let path = root.join(rel);
            println!("cargo:rerun-if-changed={rel}");
            collect_dir(&path, Path::new(rel), &mut files);
        } else if line.starts_with("runtime-dir ") || line.starts_with("runtime-file ") {
            // Hashed at run time, not built in.
        } else {
            panic!("unknown graded-files.txt entry: {line}");
        }
    }
    files.sort();
    files.dedup();

    let mut hasher = Sha256::new();
    for rel in &files {
        let path = root.join(rel);
        let content = fs::read(&path)
            .unwrap_or_else(|e| panic!("graded file {} is unreadable: {e}", path.display()));
        println!("cargo:rerun-if-changed={}", rel.display());
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&content);
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=SDB_COMPILED_RULES_DIGEST={hex}");
}

fn collect_dir(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("graded dir {} is unreadable: {e}", dir.display()));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        let child = dir.join(&name);
        if child.is_dir() {
            collect_dir(&child, &child_rel, out);
        } else if child.is_file() {
            out.push(child_rel);
        }
    }
}
