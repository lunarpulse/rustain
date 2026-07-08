//! Integration tests for `rustain session list` (Stories 13.5a / 13.5a-1).
//!
//! Covers offline-safe operation, read-only behavior, flat+dir dedup, cross-
//! workspace `--all`, and the shared `build_session_rows` core over real
//! `FileSystemStorage`.

#![cfg(feature = "test-instrumentation")]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use assert_cmd::Command;
use async_trait::async_trait;
use predicates::str::contains;
use rustain::adapters::cli::session::list::run_session_list;
use rustain::adapters::cli::session::rows::build_session_rows;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::models::session_meta::SessionMeta;
use rustain::domain::ports::{StoragePort, WorkspaceRegistryReaderPort};
use rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT;
use serial_test::serial;

fn rustain_cmd() -> Command {
    Command::cargo_bin("rustain").unwrap()
}

struct EmptyWorkspaceReader;

#[async_trait]
impl WorkspaceRegistryReaderPort for EmptyWorkspaceReader {
    async fn live_workspaces(
        &self,
    ) -> Result<
        Vec<rustain::domain::ports::WorkspaceEntry>,
        rustain::domain::ports::WorkspaceRegistryError,
    > {
        Ok(vec![])
    }
}

/// Build a `SessionMeta` fixture.
fn meta(_id: &str, title: &str, updated_at: i64, message_count: usize) -> SessionMeta {
    SessionMeta {
        version: 1,
        title: title.to_string(),
        created_at: updated_at - 10,
        updated_at,
        message_count,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        plan_slug: None,
        extra: serde_json::Map::new(),
    }
}

/// Write a directory-layout session fixture.
async fn write_dir_session(sessions_dir: &Path, id: &str, meta: &SessionMeta) {
    let dir = sessions_dir.join(id);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("meta.json"), serde_json::to_string(meta).unwrap())
        .await
        .unwrap();
}

/// Write a flat-layout session fixture (SessionMeta sidecar only is sufficient).
async fn write_flat_session(sessions_dir: &Path, id: &str, meta: &SessionMeta) {
    tokio::fs::write(
        sessions_dir.join(format!("{}.session.json", id)),
        serde_json::to_string(meta).unwrap(),
    )
    .await
    .unwrap();
}

/// Snapshot the content hashes + path set of a directory recursively.
fn snapshot_dir(
    dir: &Path,
) -> (
    HashSet<std::path::PathBuf>,
    Vec<(std::path::PathBuf, String)>,
) {
    fn walk(
        dir: &Path,
        base: &Path,
        paths: &mut HashSet<std::path::PathBuf>,
        hashes: &mut Vec<(std::path::PathBuf, String)>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() {
                let rel = path.strip_prefix(base).unwrap().to_path_buf();
                let content = std::fs::read(&path).unwrap();
                let hash = blake3::hash(&content).to_hex().to_string();
                paths.insert(rel.clone());
                hashes.push((rel, hash));
            } else if path.is_dir() {
                walk(&path, base, paths, hashes);
            }
        }
    }
    let mut paths = HashSet::new();
    let mut hashes = vec![];
    walk(dir, dir, &mut paths, &mut hashes);
    hashes.sort_by(|a, b| a.0.cmp(&b.0));
    (paths, hashes)
}

fn registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join("workspaces.json")
}

fn write_registry(data_dir: &Path, workspaces: &[(&Path, i64)]) {
    let rows: Vec<_> = workspaces
        .iter()
        .map(|(path, last_seen)| {
            serde_json::json!({
                "path": path.canonicalize().unwrap_or_else(|_| (*path).to_path_buf()),
                "last_seen": last_seen,
            })
        })
        .collect();
    std::fs::write(
        registry_path(data_dir),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": "1.0",
            "workspaces": rows,
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn make_workspace_fixture(
    dir: &Path,
    id: &str,
    title: &str,
    updated_at: i64,
    message_count: usize,
) {
    let sessions_dir = dir.join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        id,
        &meta(id, title, updated_at, message_count),
    )
    .await;
}

// =========================================================================
// P0-12: offline-safe + no provider construction
// =========================================================================

#[tokio::test]
#[serial(session_cli)]
async fn p0_12_run_session_list_all_does_not_construct_provider() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir,
        dir.path().to_path_buf(),
    ));
    let reader: Arc<dyn WorkspaceRegistryReaderPort> = Arc::new(EmptyWorkspaceReader);

    PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    run_session_list(false, true, dir.path(), &storage, &reader)
        .await
        .unwrap();

    let count = PROVIDER_CTOR_COUNT.load(Ordering::SeqCst);
    assert_eq!(
        count, 0,
        "session list --all must not construct any provider, got {count}"
    );
}

// =========================================================================
// P0-13: read-only — no new files, no content changes, registry unchanged
// =========================================================================

#[tokio::test]
#[serial(session_cli)]
async fn p0_13_session_list_all_is_read_only() {
    let data_dir = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", data_dir.path());
    }

    make_workspace_fixture(current.path(), "curr-sess", "Current", 300, 2).await;
    make_workspace_fixture(other.path(), "other-sess", "Other", 200, 1).await;
    write_registry(data_dir.path(), &[(other.path(), 200)]);

    let current_sessions = current.path().join(".claude").join("sessions");
    let other_sessions = other.path().join(".claude").join("sessions");
    let (current_paths_before, current_hashes_before) = snapshot_dir(&current_sessions);
    let (other_paths_before, other_hashes_before) = snapshot_dir(&other_sessions);
    let registry_before = std::fs::read_to_string(registry_path(data_dir.path())).unwrap();
    let registry_mtime_before = std::fs::metadata(registry_path(data_dir.path()))
        .unwrap()
        .modified()
        .unwrap();

    let mut cmd = rustain_cmd();
    cmd.current_dir(current.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .arg("session")
        .arg("list")
        .arg("--all");
    cmd.assert().success();

    let (current_paths_after, current_hashes_after) = snapshot_dir(&current_sessions);
    let (other_paths_after, other_hashes_after) = snapshot_dir(&other_sessions);
    assert_eq!(current_paths_before, current_paths_after);
    assert_eq!(current_hashes_before, current_hashes_after);
    assert_eq!(other_paths_before, other_paths_after);
    assert_eq!(other_hashes_before, other_hashes_after);
    assert_eq!(
        registry_before,
        std::fs::read_to_string(registry_path(data_dir.path())).unwrap()
    );
    assert_eq!(
        registry_mtime_before,
        std::fs::metadata(registry_path(data_dir.path()))
            .unwrap()
            .modified()
            .unwrap()
    );
}

// =========================================================================
// P0-10: flat+dir dedup characterization
// =========================================================================

#[tokio::test]
#[serial(session_cli)]
async fn p0_10_flat_and_dir_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let id = "shared-id";
    let m = meta(id, "Shared", 300, 3);
    write_dir_session(&sessions_dir, id, &m).await;
    write_flat_session(&sessions_dir, id, &m).await;

    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir,
        dir.path().to_path_buf(),
    ));
    let summaries = storage.list_conversations().await.unwrap();
    let rows = build_session_rows(summaries);

    assert_eq!(rows.len(), 1, "same id in flat+dir must dedup to one row");
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].index, 1);
}

// =========================================================================
// End-to-end CLI tests
// =========================================================================

#[tokio::test]
#[serial(session_cli)]
async fn e2e_session_list_human_table() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    write_dir_session(&sessions_dir, "sess-a", &meta("sess-a", "Alpha", 200, 2)).await;
    write_dir_session(&sessions_dir, "sess-b", &meta("sess-b", "Beta", 300, 1)).await;

    let mut cmd = rustain_cmd();
    cmd.current_dir(dir.path()).arg("session").arg("list");
    cmd.assert().success().stdout(contains("Alpha"));

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Beta"));
    assert!(stdout.contains("sess-b"));
    assert!(stdout.contains("* = resumes by default (most recent)"));
}

#[tokio::test]
#[serial(session_cli)]
async fn e2e_session_list_json_envelope_includes_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    write_dir_session(
        &sessions_dir,
        "json-sess",
        &meta("json-sess", "JSON", 400, 1),
    )
    .await;

    let mut cmd = rustain_cmd();
    cmd.current_dir(dir.path())
        .arg("session")
        .arg("list")
        .arg("--json");
    cmd.assert().success();

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.1");
    let sessions = parsed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"].as_str().unwrap(), "json-sess");
    assert_eq!(
        sessions[0]["workspace"].as_str().unwrap(),
        dir.path().canonicalize().unwrap().to_string_lossy()
    );
}

#[tokio::test]
#[serial(session_cli)]
async fn e2e_session_list_all_human_table() {
    let data_dir = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let dead = tempfile::tempdir().unwrap();

    make_workspace_fixture(current.path(), "curr-sess", "Current", 300, 2).await;
    make_workspace_fixture(other.path(), "other-sess", "Other", 200, 1).await;
    write_registry(
        data_dir.path(),
        &[
            (current.path(), 300),
            (other.path(), 200),
            (dead.path(), 100),
        ],
    );

    let mut cmd = rustain_cmd();
    cmd.current_dir(current.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .arg("session")
        .arg("list")
        .arg("--all");
    cmd.assert().success();

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("WORKSPACE"));
    assert!(stdout.contains("Current"));
    assert!(stdout.contains("Other"));
    assert!(
        stdout.contains(
            current
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(stdout.contains(other.path().file_name().unwrap().to_string_lossy().as_ref()));
    assert!(!stdout.contains(dead.path().file_name().unwrap().to_string_lossy().as_ref()));
    assert!(stdout.contains("* = resumes by default (most recent in current workspace"));
}

#[tokio::test]
#[serial(session_cli)]
async fn e2e_session_list_all_json_composite_workspace_address() {
    let data_dir = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();

    make_workspace_fixture(current.path(), "same-id", "Current Same", 300, 2).await;
    make_workspace_fixture(other.path(), "same-id", "Other Same", 200, 1).await;
    write_registry(
        data_dir.path(),
        &[(current.path(), 300), (other.path(), 200)],
    );

    let mut cmd = rustain_cmd();
    cmd.current_dir(current.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .arg("session")
        .arg("list")
        .arg("--all")
        .arg("--json");
    cmd.assert().success();

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = parsed["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        2,
        "current workspace must not be double-listed"
    );
    assert_eq!(sessions[0]["id"].as_str().unwrap(), "same-id");
    assert_eq!(sessions[1]["id"].as_str().unwrap(), "same-id");
    assert_ne!(
        sessions[0]["workspace"].as_str().unwrap(),
        sessions[1]["workspace"].as_str().unwrap()
    );
}

#[tokio::test]
#[serial(session_cli)]
async fn e2e_session_list_empty_state() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let mut cmd = rustain_cmd();
    cmd.current_dir(dir.path()).arg("session").arg("list");
    cmd.assert()
        .success()
        .stdout(contains("No saved sessions in this workspace yet."));

    let mut cmd_json = rustain_cmd();
    cmd_json
        .current_dir(dir.path())
        .arg("session")
        .arg("list")
        .arg("--json");
    cmd_json.assert().success();

    let output = cmd_json.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.1");
    assert_eq!(parsed["sessions"].as_array().unwrap().len(), 0);
}

// =========================================================================
// Column/field regression tests
// =========================================================================

#[tokio::test]
#[serial(session_cli)]
async fn e2e_session_list_shows_full_id_and_message_count() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let id = "very-long-session-id-12345";
    write_dir_session(&sessions_dir, id, &meta(id, "Count", 500, 7)).await;

    let mut cmd = rustain_cmd();
    cmd.current_dir(dir.path()).arg("session").arg("list");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(id));
    assert!(stdout.contains('7'));
}
