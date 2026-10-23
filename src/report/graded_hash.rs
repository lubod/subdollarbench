#![allow(dead_code)]
//! Shared graded-digest code (R28 F4).
//!
//! `build.rs` and `report::rules` both include this module: the build emits
//! `SDB_COMPILED_RULES_DIGEST`, and the test recomputes it from the same
//! manifest and the same path/content hash. The module deliberately uses
//! only std + sha2 so it works in the build script too.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    RuntimeFile,
    RuntimeDir,
}

/// Parse the one manifest. Comments and blank lines are ignored.
pub fn parse(manifest: &str) -> Vec<(Kind, &str)> {
    manifest
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (kind, rel) = line.split_once(' ')?;
            let kind = match kind {
                "file" => Kind::File,
                "dir" => Kind::Dir,
                "runtime-file" => Kind::RuntimeFile,
                "runtime-dir" => Kind::RuntimeDir,
                _ => panic!("unknown graded-files.txt entry: {line}"),
            };
            (!rel.is_empty()).then_some((kind, rel))
        })
        .collect()
}

pub fn collect_dir(root: &Path, rel_dir: &Path, out: &mut Vec<PathBuf>) {
    let dir = root.join(rel_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let rel = rel_dir.join(entry.file_name());
        let path = root.join(&rel);
        if path.is_dir() {
            collect_dir(root, &rel, out);
        } else if path.is_file() {
            out.push(rel);
        }
    }
}

/// Files covered by the manifest. `include_runtime` selects whether
/// `runtime-file`/`runtime-dir` entries are included.
pub fn files(root: &Path, manifest: &str, include_runtime: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (kind, rel) in parse(manifest) {
        match kind {
            Kind::File => out.push(PathBuf::from(rel)),
            Kind::Dir => collect_dir(root, Path::new(rel), &mut out),
            Kind::RuntimeFile if include_runtime => out.push(PathBuf::from(rel)),
            Kind::RuntimeDir if include_runtime => collect_dir(root, Path::new(rel), &mut out),
            Kind::RuntimeFile | Kind::RuntimeDir => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn runtime_files(root: &Path, manifest: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (kind, rel) in parse(manifest) {
        match kind {
            Kind::RuntimeFile => out.push(PathBuf::from(rel)),
            Kind::RuntimeDir => collect_dir(root, Path::new(rel), &mut out),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// SHA-256 over sorted `(path, content)` pairs, full hex. This is the
/// build's compiled digest and the test's independent recomputation.
pub fn digest_files(root: &Path, files: &[PathBuf]) -> String {
    let mut files = files.to_vec();
    files.sort();
    files.dedup();
    let mut hasher = Sha256::new();
    for rel in files {
        let path = root.join(&rel);
        let content = fs::read(&path)
            .unwrap_or_else(|e| panic!("graded file {} is unreadable: {e}", path.display()));
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&content);
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn collect_dir_strict(root: &Path, rel_dir: &Path, out: &mut Vec<PathBuf>) {
    let dir = root.join(rel_dir);
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("graded dir {} is unreadable: {e}", dir.display()));
    for entry in entries.flatten() {
        let rel = rel_dir.join(entry.file_name());
        let path = root.join(&rel);
        if path.is_dir() {
            collect_dir_strict(root, &rel, out);
        } else if path.is_file() {
            out.push(rel);
        }
    }
}

/// Build-time files. Unlike the runtime `files` helper, a missing `dir`
/// entry is a build error: a renamed/mistyped directory must not hash as
/// nothing.
pub fn compiled_files(root: &Path, manifest: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (kind, rel) in parse(manifest) {
        match kind {
            Kind::File => out.push(PathBuf::from(rel)),
            Kind::Dir => collect_dir_strict(root, Path::new(rel), &mut out),
            Kind::RuntimeFile | Kind::RuntimeDir => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn compiled_digest(root: &Path, manifest: &str) -> String {
    let files = compiled_files(root, manifest);
    digest_files(root, &files)
}
