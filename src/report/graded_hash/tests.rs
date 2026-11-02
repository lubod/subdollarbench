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

/// R61-4: `#[cfg(test)]` must be one of the attribute lines directly
/// above `mod tests;`, not merely somewhere in the parent. A `#[path]`
/// line may sit between them.
fn declares_cfg_test_tests(src: &str) -> bool {
    let lines: Vec<&str> = src.lines().map(str::trim).collect();
    for (i, line) in lines.iter().enumerate() {
        if *line != "mod tests;" {
            continue;
        }
        let mut saw_cfg_test = false;
        let mut j = i;
        while j > 0 {
            let prev = lines[j - 1];
            if !(prev.starts_with("#[") && prev.ends_with(']')) {
                break;
            }
            if prev == "#[cfg(test)]" {
                saw_cfg_test = true;
            }
            j -= 1;
        }
        if saw_cfg_test {
            return true;
        }
    }
    false
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
        let ok = candidates.iter().any(|p| {
            fs::read_to_string(p)
                .map(|s| declares_cfg_test_tests(&s))
                .unwrap_or(false)
        });
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
    fs::create_dir_all(root.join("src/a/x")).unwrap();
    fs::write(root.join("src/a/x/tests.rs"), "").unwrap();
    fs::write(root.join("src/a/x.rs"), "pub fn x() {}\n").unwrap();
    fs::write(root.join("src/a/y.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
    assert!(check_cfg_test_only(&root, &["src"]).is_err());
    fs::write(root.join("src/a/x.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
    assert!(check_cfg_test_only(&root, &["src"]).is_ok());
    // R61-4: an unrelated #[cfg(test)] item above a bare mod tests; is not
    // a declaration of the test module.
    fs::create_dir_all(root.join("src/a/z")).unwrap();
    fs::write(root.join("src/a/z/tests.rs"), "").unwrap();
    fs::write(
        root.join("src/a/z.rs"),
        "#[cfg(test)]\nfn helper() {}\nmod tests;\n",
    )
    .unwrap();
    assert!(check_cfg_test_only(&root, &["src"]).is_err());
    fs::write(root.join("src/a/z.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
    assert!(check_cfg_test_only(&root, &["src"]).is_ok());
    let _ = fs::remove_dir_all(&root);
}
