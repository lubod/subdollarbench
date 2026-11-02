use super::*;
use std::fs;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graded_hash_{tag}_{}_{}",
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

/// R61 F2: a tests.rs change keeps the digest; a production file under
/// the same dir moves it.
#[test]
fn r61_f2_tests_rs_outside_digest() {
    let root = temp_root("outside");
    fs::create_dir_all(root.join("src/x")).unwrap();
    fs::write(root.join("src/x.rs"), "prod\n").unwrap();
    fs::write(root.join("src/x/tests.rs"), "test\n").unwrap();
    let all = files(&root, "dir src\n", false);
    assert_eq!(all, vec![PathBuf::from("src/x.rs")]);
    let before = digest_files(&root, &all);
    fs::write(root.join("src/x/tests.rs"), "changed test\n").unwrap();
    let after = digest_files(&root, &files(&root, "dir src\n", false));
    assert_eq!(before, after, "a tests.rs change must not move the digest");
    fs::write(root.join("src/x.rs"), "prod changed\n").unwrap();
    let moved = digest_files(&root, &files(&root, "dir src\n", false));
    assert_ne!(before, moved, "a production change must move the digest");
    let _ = fs::remove_dir_all(&root);
}

fn check_cfg_test_only(root: &Path, dirs: &[&str]) -> Result<(), String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(root.join(dir)) else {
            return;
        };
        for e in rd.flatten() {
            let rel = dir.join(e.file_name());
            if root.join(&rel).is_dir() {
                walk(root, &rel, out);
            } else if e.file_name() == "tests.rs" {
                out.push(rel);
            }
        }
    }
    let mut found = Vec::new();
    for d in dirs {
        walk(root, Path::new(d), &mut found);
    }
    for tests in found {
        let tests = root.join(tests);
        let dir = tests.parent().unwrap();
        let stem = dir.file_name().unwrap().to_string_lossy().to_string();
        let parent_dir = dir.parent().unwrap();
        let candidates = [
            parent_dir.join(format!("{stem}.rs")),
            parent_dir.join(&stem).join("mod.rs"),
        ];
        let mut ok = candidates.iter().any(|p| {
            fs::read_to_string(p)
                .map(|s| s.contains("#[cfg(test)]\nmod tests;"))
                .unwrap_or(false)
        });
        // A `#[path = "graded_hash.rs"]` module declares its child tests
        // beside the path file, not a same-named sibling. Accept any
        // sibling source that declares the module under cfg(test).
        if !ok {
            if let Ok(rd) = fs::read_dir(parent_dir) {
                ok = rd.flatten().any(|e| {
                    e.path().extension().and_then(|x| x.to_str()) == Some("rs")
                        && fs::read_to_string(e.path())
                            .map(|s| s.contains("#[cfg(test)]\nmod tests;"))
                            .unwrap_or(false)
                });
            }
        }
        if !ok {
            return Err(format!(
                "{} has no '#[cfg(test)] mod tests;' parent",
                tests.display()
            ));
        }
    }
    Ok(())
}

/// R61 F2: every tests.rs in the tree is declared under #[cfg(test)].
#[test]
fn r61_f2_test_files_are_cfg_test_only() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    check_cfg_test_only(manifest, &["src"]).expect("the tree must be cfg(test)-only");
    let root = temp_root("fixture");
    fs::create_dir_all(root.join("src/x")).unwrap();
    fs::write(root.join("src/x/tests.rs"), "").unwrap();
    assert!(check_cfg_test_only(&root, &["src"]).is_err());
    fs::write(root.join("src/x.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
    assert!(check_cfg_test_only(&root, &["src"]).is_ok());
    let _ = fs::remove_dir_all(&root);
}
