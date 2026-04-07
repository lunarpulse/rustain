use rustain::adapters::file_scanner::scan_workspace_files;

// === File scanner tests (Story 3.2, Task 3) ===

// Covers: AC3 — scan returns files from workspace
#[test]
fn test_scan_finds_files() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "").unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src").join("app.rs"), "").unwrap();

    let results = scan_workspace_files(tmp.path(), "", 50);
    assert!(results.len() >= 3);
    let paths: Vec<&str> = results.iter().map(|r| r.relative_path.as_str()).collect();
    assert!(paths.iter().any(|p| *p == "main.rs"));
    assert!(paths.iter().any(|p| *p == "lib.rs"));
    assert!(paths.iter().any(|p| *p == "src/app.rs"));
}

// Covers: AC3 — scan filters by prefix
#[test]
fn test_scan_filters_by_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.rs"), "").unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
    std::fs::write(tmp.path().join("README.md"), "").unwrap();

    let results = scan_workspace_files(tmp.path(), "car", 50);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].relative_path, "Cargo.toml");
}

// Covers: AC3 — excluded directories are skipped
#[test]
fn test_scan_excludes_target() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("target")).unwrap();
    std::fs::write(tmp.path().join("target").join("hidden.rs"), "").unwrap();
    std::fs::write(tmp.path().join("main.rs"), "").unwrap();

    let results = scan_workspace_files(tmp.path(), "", 50);
    let paths: Vec<&str> = results.iter().map(|r| r.relative_path.as_str()).collect();
    assert!(!paths.iter().any(|p| p.contains("target")));
    assert!(paths.contains(&"main.rs"));
}

// Covers: AC3 — excludes .git directory
#[test]
fn test_scan_excludes_git() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    std::fs::write(tmp.path().join(".git").join("config"), "").unwrap();
    std::fs::write(tmp.path().join("app.rs"), "").unwrap();

    let results = scan_workspace_files(tmp.path(), "", 50);
    let paths: Vec<&str> = results.iter().map(|r| r.relative_path.as_str()).collect();
    assert!(!paths.iter().any(|p| p.contains(".git")));
}

// Covers: AC3 — depth limit respected
#[test]
fn test_scan_depth_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let deep = tmp.path().join("a").join("b").join("c").join("d").join("e");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("deep.rs"), "").unwrap();
    std::fs::write(tmp.path().join("shallow.rs"), "").unwrap();

    let results = scan_workspace_files(tmp.path(), "", 50);
    let paths: Vec<&str> = results.iter().map(|r| r.relative_path.as_str()).collect();
    assert!(paths.contains(&"shallow.rs"));
    // deep.rs is at depth 5, max is 4 — should not be found
    assert!(!paths.iter().any(|p| p.contains("deep.rs")));
}

// Covers: AC3 — result limit respected
#[test]
fn test_scan_result_limit() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..20 {
        std::fs::write(tmp.path().join(format!("file{}.rs", i)), "").unwrap();
    }

    let results = scan_workspace_files(tmp.path(), "", 5);
    assert_eq!(results.len(), 5);
}

// Covers: AC3 — directories are flagged as is_dir
#[test]
fn test_scan_marks_directories() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("main.rs"), "").unwrap();

    let results = scan_workspace_files(tmp.path(), "", 50);
    let src = results.iter().find(|r| r.relative_path.contains("src"));
    assert!(src.is_some());
    assert!(src.unwrap().is_dir);
    assert!(src.unwrap().relative_path.ends_with('/'));

    let main = results
        .iter()
        .find(|r| r.relative_path == "main.rs")
        .unwrap();
    assert!(!main.is_dir);
}

// Covers: AC3 — empty workspace returns empty
#[test]
fn test_scan_empty_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let results = scan_workspace_files(tmp.path(), "", 50);
    assert!(results.is_empty());
}
