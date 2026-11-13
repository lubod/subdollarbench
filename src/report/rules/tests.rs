use super::*;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rules_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(root: &Path, rel: &str, content: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    PathBuf::from(rel)
}

#[test]
fn test_fingerprint_starts_with_version() {
    let fp = fingerprint();
    assert!(
        fp.starts_with(&format!("{RULES_VERSION}+")),
        "fingerprint {fp} must start with RULES_VERSION"
    );
}

/// R27 F1 acceptance: SHA-256 is deterministic across processes. The
/// expected value is pinned for a one-file fixture tree.
#[test]
fn r27_f1_stamp_is_stable_across_processes() {
    let root = temp_root("sha");
    let rel = put(&root, "src/scoring.rs", "fixture\n");
    let fp = fingerprint_files(&root, std::slice::from_ref(&rel));
    assert_eq!(
        fp, "2026-11-13.47+0727ce2a1151d09e6da58edc480fc29b",
        "the SHA-256 stamp must be stable"
    );
    assert_eq!(fp, fingerprint_files(&root, &[rel]));
    let _ = fs::remove_dir_all(&root);
}

/// R27 F1 acceptance: a compiled-in file changed on disk after the
/// build does not move the binary's stamp; a rebuild (simulated by
/// fingerprint_files over the compiled list) does.
#[test]
fn r27_f1_compiled_in_files_hash_the_build() {
    assert_eq!(COMPILED_GRADED_DIGEST.len(), 64);
    assert!(COMPILED_GRADED_DIGEST
        .chars()
        .all(|c| c.is_ascii_hexdigit()));
    let root = temp_root("compiled");
    let rel = put(&root, "src/scoring.rs", "one\n");
    let compiled_a = fingerprint_files(&root, std::slice::from_ref(&rel));
    let runtime_a = fingerprint_root(&root);
    put(&root, "src/scoring.rs", "two\n");
    let compiled_b = fingerprint_files(&root, &[rel]);
    let runtime_b = fingerprint_root(&root);
    assert_eq!(
        runtime_a, runtime_b,
        "a compiled file on disk must not change the built binary's stamp"
    );
    assert_ne!(
        compiled_a, compiled_b,
        "a rebuild over the changed file must move the stamp"
    );
    let _ = fs::remove_dir_all(&root);
}

/// R27 F3 acceptance: the rules page and proposal state the cap's
/// list-price basis and its snapshot source.
#[test]
fn r27_f3_rules_page_states_list_price() {
    assert!(CAP_PRICE_BASIS.contains("list price"));
    assert!(CAP_PRICE_BASIS.contains("src/cost/model_prices.json"));
    let proposal = include_str!("../../../docs/PROPOSAL-sdb-sql.md");
    assert!(
        proposal.contains("list price"),
        "proposal missing list price"
    );
    assert!(
        proposal.contains("src/cost/model_prices.json"),
        "proposal missing the snapshot source"
    );
}

fn normalize_include(base: &Path, target: &str) -> PathBuf {
    let joined = base.join(target);
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn include_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for prefix in ["include_str!(\"", "include_bytes!(\""] {
        let mut rest = text;
        while let Some(at) = rest.find(prefix) {
            let start = at + prefix.len();
            let Some(end) = rest[start..].find('"') else {
                break;
            };
            out.push(rest[start..start + end].to_string());
            rest = &rest[start + end..];
        }
    }
    out
}

/// R27 F1/R27-2 acceptance: every source under the verifier/prompts/
/// cost directories and every `include_str!`/`include_bytes!` target
/// under pipeline/sandbox/cost/prompts is covered by the one list.
#[test]
fn r27_f1_every_graded_source_is_listed() {
    let root = crate::config::get_repo_root();
    let files = graded_files(&root);
    for dir in ["src/verifier/sql", "src/prompts", "src/cost"] {
        let mut inner = Vec::new();
        graded_hash::collect_dir(&root, Path::new(dir), &mut inner);
        assert!(!inner.is_empty(), "no files under {dir}");
        for rel in inner {
            assert!(
                files.contains(&rel),
                "{rel:?} under {dir} is missing from graded-files.txt"
            );
        }
    }
    let mut sources = vec![PathBuf::from("src/pipeline.rs")];
    for dir in ["src/sandbox", "src/cost", "src/prompts"] {
        graded_hash::collect_dir(&root, Path::new(dir), &mut sources);
    }
    for source in sources {
        let Ok(text) = fs::read_to_string(root.join(&source)) else {
            continue;
        };
        for target in include_targets(&text) {
            let rel = normalize_include(source.parent().unwrap_or(Path::new("")), &target);
            assert!(
                files.contains(&rel),
                "{source:?} includes {target:?} -> {rel:?}, which is missing from graded-files.txt"
            );
        }
    }
}

/// R28-1 acceptance: recorded run values win; a pre-`.39` row gets no
/// list-price label even though today's binary uses the snapshot.
#[test]
fn r28_r1_recorded_oracle_and_cap_basis() {
    assert_eq!(recorded_sqlite_version(Some("3.0.0")), "3.0.0");
    assert_eq!(recorded_sqlite_version(None), "—");
    assert_eq!(
        recorded_cap_basis("2026-10-19.40", None),
        Some(CAP_PRICE_BASIS)
    );
    assert_eq!(recorded_cap_basis("2026-10-19.38", None), None);
    assert_eq!(
        recorded_cap_basis("2026-10-19.38", Some("recorded")),
        Some("recorded")
    );
}

/// R29 F1 acceptance: rules versions compare as (date, number).
#[test]
fn r29_f1_rules_versions_compare_numerically() {
    assert!(rules_version_at_least("2026-10-19.100", "2026-10-19.39"));
    assert!(!rules_version_at_least("2026-10-18.50", "2026-10-19.39"));
    assert!(rules_version_at_least("2026-10-19.39", "2026-10-19.39"));
    assert!(!rules_version_at_least("legacy-pre-cheat", "2026-10-19.39"));
    assert_eq!(
        recorded_cap_basis("2026-10-19.38", None),
        None,
        "the old string comparison must not make pre-.39 look priced"
    );
}

/// R29 F1 acceptance: a missing compiled `dir` entry fails the build
/// digest instead of hashing as nothing.
#[test]
fn r29_f1_missing_graded_dir_fails() {
    let root = temp_root("missingdir");
    let result = std::panic::catch_unwind(|| {
        graded_hash::compiled_digest(&root, "dir definitely_missing\n")
    });
    assert!(result.is_err(), "a missing dir must fail compiled_digest");
    let _ = fs::remove_dir_all(&root);
}

/// R30-1 acceptance: the pinned toolchain is in the graded manifest.
#[test]
fn r30_r1_toolchain_is_a_graded_input() {
    let root = crate::config::get_repo_root();
    let files = graded_files(&root);
    assert!(
        files.iter().any(|p| p == Path::new("rust-toolchain.toml")),
        "graded_files misses rust-toolchain.toml"
    );
    assert!(
        GRADED_MANIFEST
            .lines()
            .any(|line| line.trim() == "file rust-toolchain.toml"),
        "graded-files.txt misses the toolchain entry"
    );
}

/// R28 F1 acceptance: both the linked SQLite version and the compiled
/// crate digest move the stamp.
#[test]
fn r28_f1_oracle_version_moves_the_stamp() {
    let root = temp_root("oracle");
    let a = stamp_with_oracle(&root, "compiled-a", "3.45.0");
    let b = stamp_with_oracle(&root, "compiled-a", "3.45.1");
    let c = stamp_with_oracle(&root, "compiled-b", "3.45.0");
    assert_ne!(a, b, "the SQLite version must move the stamp");
    assert_ne!(a, c, "the crate digest must move the stamp");
    assert!(!sqlite_version().is_empty());
    let _ = fs::remove_dir_all(&root);
}

/// R28 F4 acceptance: the test recomputes the build digest from the
/// same source tree, manifest and hashing module and compares it with
/// the compile-time constant.
#[test]
fn r28_f4_build_digest_matches_the_tree() {
    let root = crate::config::get_repo_root();
    assert_eq!(
        graded_hash::compiled_digest(&root, GRADED_MANIFEST),
        COMPILED_GRADED_DIGEST,
        "build.rs and the test must hash the same compiled inputs"
    );
    let different_manifest = "file src/scoring.rs\n";
    assert_ne!(
        graded_hash::compiled_digest(&root, different_manifest),
        COMPILED_GRADED_DIGEST,
        "a manifest that disagrees with the build must produce a different digest"
    );
}

/// R27-1 acceptance: the campaign spec is read from SDB_HOME at run
/// time, so editing it must move the runtime half of the stamp.
#[test]
fn r27_r1_campaign_json_under_sdb_home_changes_the_stamp() {
    let root = temp_root("campaignhome");
    let rel = put(
        &root,
        "campaigns/sdb-sql/campaign.json",
        r#"{"name":"sdb-sql","stages":[]}"#,
    );
    let a = fingerprint_root(&root);
    put(
        &root,
        rel.to_str().unwrap(),
        r#"{"name":"sdb-sql","stages":[{}]}"#,
    );
    let b = fingerprint_root(&root);
    assert_ne!(a, b, "a runtime campaign.json edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

/// R27 F1 acceptance: a stage prompt under SDB_HOME still moves the
/// runtime half of the stamp.
#[test]
fn r27_f1_a_stage_under_sdb_home_changes_the_stamp() {
    let root = temp_root("stagehome");
    let rel = put(&root, "campaigns/sdb-sql/stages/0_genesis/prompt.md", "one");
    let a = fingerprint_root(&root);
    put(&root, rel.to_str().unwrap(), "two");
    let b = fingerprint_root(&root);
    assert_ne!(a, b, "a runtime stage edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn r24_r1_the_real_tree_covers_the_sdb_sql_inputs() {
    let files = graded_files(&crate::config::get_repo_root());
    for rel in [
        "campaigns/sdb-sql/campaign.json",
        "campaigns/sdb-sql/stages/0_genesis/prompt.md",
        "src/verifier/sql/wire.rs",
        "src/scoring.rs",
        "src/cost/model_prices.json",
        "src/prompts/session_budget.txt",
        "src/prompts/omp_effective.md",
        "src/prompts/opencode_agents.md",
        "tasks/sql/prompt.md",
    ] {
        assert!(
            files.iter().any(|p| p == Path::new(rel)),
            "graded_files misses {rel}: {files:?}"
        );
    }
}

#[test]
fn r24_r2_editing_the_price_snapshot_changes_the_fingerprint() {
    let root = temp_root("prices");
    let rel = put(&root, "src/cost/model_prices.json", r#"{"models":[]}"#);
    let a = fingerprint_files(&root, std::slice::from_ref(&rel));
    put(&root, "src/cost/model_prices.json", r#"{"models":[1]}"#);
    let b = fingerprint_files(&root, &[rel]);
    assert_ne!(a, b, "a price-snapshot edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn r24_r1_editing_a_stage_prompt_changes_the_fingerprint() {
    let root = temp_root("stage");
    let rel = put(
        &root,
        "campaigns/sdb-sql/stages/99_new/prompt.md",
        "first prompt",
    );
    let a = fingerprint_files(&root, std::slice::from_ref(&rel));
    put(
        &root,
        "campaigns/sdb-sql/stages/99_new/prompt.md",
        "second prompt",
    );
    let b = fingerprint_files(&root, &[rel]);
    assert_ne!(a, b, "a stage prompt edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn r24_r1_editing_an_sql_verifier_changes_the_fingerprint() {
    let root = temp_root("sql");
    let rel = put(&root, "src/verifier/sql/wire.rs", "one verifier");
    let a = fingerprint_files(&root, std::slice::from_ref(&rel));
    put(&root, "src/verifier/sql/wire.rs", "two verifier");
    let b = fingerprint_files(&root, &[rel]);
    assert_ne!(a, b, "an SQL verifier edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn r24_r1_editing_a_scoring_constant_changes_the_fingerprint() {
    let root = temp_root("scoring");
    let rel = put(&root, "src/scoring.rs", "pub const TURN_GUARD: u32 = 200;");
    let a = fingerprint_files(&root, std::slice::from_ref(&rel));
    put(&root, "src/scoring.rs", "pub const TURN_GUARD: u32 = 201;");
    let b = fingerprint_files(&root, &[rel]);
    assert_ne!(a, b, "a scoring constant edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

/// R26 R23-7 acceptance: the remaining turn rule is a graded input, so
/// editing how the guard is shared moves the stamp.
#[test]
fn r26_editing_the_turn_rule_changes_the_fingerprint() {
    let root = temp_root("turnrule");
    let rel = put(
        &root,
        "src/scoring.rs",
        "pub fn initial_turn_limit(max_turns: u32) -> Option<u32> { (max_turns > 0).then_some(max_turns) }",
    );
    let a = fingerprint_files(&root, std::slice::from_ref(&rel));
    put(
        &root,
        "src/scoring.rs",
        "pub fn initial_turn_limit(max_turns: u32) -> Option<u32> { (max_turns > 1).then_some(max_turns) }",
    );
    let b = fingerprint_files(&root, &[rel]);
    assert_ne!(a, b, "a turn-rule edit must move the stamp");
    let _ = fs::remove_dir_all(&root);
}

/// R43 F4: the gate's and flake bar's scope list
/// (`scripts/graded-paths.txt`) covers every input the fingerprint
/// hashes (`graded-files.txt`), so an edit can never move the stamp
/// while the gate calls it non-graded.
#[test]
fn r43_f4_graded_lists_agree() {
    let root = crate::config::get_repo_root();
    let paths = fs::read_to_string(root.join("scripts/graded-paths.txt"))
        .expect("scripts/graded-paths.txt is readable");
    let mut bases: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    for line in paths.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("grader") | Some("plumbing") => {
                if let Some(path) = fields.next() {
                    bases.push(path.to_string());
                }
            }
            Some("exclude-name") => {
                if let Some(name) = fields.next() {
                    excludes.push(name.to_string());
                }
            }
            _ => {}
        }
    }
    assert!(!bases.is_empty(), "graded-paths.txt has no entries");
    let covered = |rel: &str| {
        bases
            .iter()
            .any(|b| rel == b || rel.starts_with(&format!("{b}/")))
    };
    let mut checked = 0;
    for line in GRADED_MANIFEST.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kind, rel) = ["runtime-file", "runtime-dir", "file", "dir"]
            .iter()
            .find_map(|kind| line.strip_prefix(&format!("{kind} ")).map(|r| (*kind, r)))
            .unwrap_or_else(|| panic!("unrecognised graded entry {line:?}"));
        checked += 1;
        if kind == "file" || kind == "runtime-file" {
            assert!(
                covered(rel),
                "graded-files entry {rel:?} is not under any graded path"
            );
            continue;
        }
        // R68 F1: a directory entry (src/sandbox) may sit above a split of
        // graded paths. Every non-test file under it must still be covered;
        // a tests.rs is outside the graded list by the shared rule.
        let mut stack = vec![root.join(rel)];
        while let Some(dir) = stack.pop() {
            let entries = fs::read_dir(&dir).unwrap_or_else(|e| {
                panic!("graded-files dir {} is unreadable: {e}", dir.display())
            });
            for entry in entries {
                let path = entry.expect("graded-files dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if excludes.iter().any(|e| e == name) {
                    continue;
                }
                let relpath = path
                    .strip_prefix(&root)
                    .expect("graded-files path under the repo root")
                    .to_string_lossy()
                    .replace('\\', "/");
                assert!(
                    covered(&relpath),
                    "graded-files entry {relpath:?} is not under any graded path: {bases:?}"
                );
            }
        }
    }
    assert!(checked >= 10, "only {checked} graded entries parsed");
}

#[test]
fn r24_r1_a_new_stage_directory_is_covered() {
    let root = temp_root("newstage");
    put(
        &root,
        "campaigns/sdb-sql/stages/99_new/prompt.md",
        "new stage",
    );
    let files = graded_files(&root);
    assert!(
        files
            .iter()
            .any(|p| p == Path::new("campaigns/sdb-sql/stages/99_new/prompt.md")),
        "a new stage directory must be swept: {files:?}"
    );
    let _ = fs::remove_dir_all(&root);
}
