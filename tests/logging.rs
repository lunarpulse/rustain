//! Logging tests — verify AC5 constraints.

use rustain::infrastructure::paths;
use std::path::Path;

/// AC5: Log file path resolves to ~/.rustain/rustain.log.
#[test]
fn test_log_file_path() {
    let path = paths::log_file_path().unwrap();
    assert!(
        path.ends_with("rustain.log"),
        "Expected path ending in rustain.log, got: {:?}",
        path
    );
    assert!(
        path.parent().unwrap().ends_with(".rustain"),
        "Expected log file under ~/.rustain/, got: {:?}",
        path
    );
}

/// AC5: Data directory is created when resolving log path.
#[test]
fn test_data_dir_created() {
    let dir = paths::data_dir().unwrap();
    assert!(
        Path::new(&dir).exists(),
        "~/.rustain/ directory should exist after data_dir() call"
    );
}

/// AC5: Logging init does not panic and creates the log file.
/// Note: Can only init tracing once per process — this test verifies
/// the log directory setup, not the full tracing init (which would
/// conflict with other tests that also init tracing).
#[test]
fn test_log_directory_writable() {
    let dir = paths::data_dir().unwrap();
    let test_file = dir.join("test-write-check.tmp");
    std::fs::write(&test_file, b"test").unwrap();
    std::fs::remove_file(&test_file).unwrap();
}
