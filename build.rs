//! Build-time half of the rules stamp (R27 F1).
//!
//! Compiled-in graded inputs are hashed with SHA-256 and emitted as
//! `SDB_COMPILED_RULES_DIGEST`. Runtime files under SDB_HOME are hashed by
//! `report::rules::fingerprint_root`; this script skips the runtime
//! entries. `src/report/graded-files.txt` is the one list both halves
//! read, and `src/report/graded_hash.rs` is the one hashing implementation
//! (R28 F4).

#[path = "src/report/graded_hash.rs"]
mod graded_hash;

use graded_hash::Kind;
use std::fs;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let manifest = root.join("src/report/graded-files.txt");
    println!("cargo:rerun-if-changed=src/report/graded-files.txt");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));

    for (kind, rel) in graded_hash::parse(&text) {
        match kind {
            Kind::File => println!("cargo:rerun-if-changed={rel}"),
            Kind::Dir => println!("cargo:rerun-if-changed={rel}"),
            Kind::RuntimeFile | Kind::RuntimeDir => {}
        }
    }
    for rel in graded_hash::files(&root, &text, false) {
        println!("cargo:rerun-if-changed={}", rel.display());
    }

    let digest = graded_hash::compiled_digest(&root, &text);
    println!("cargo:rustc-env=SDB_COMPILED_RULES_DIGEST={digest}");
}
