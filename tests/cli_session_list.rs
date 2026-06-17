//! Integration tests for `rustain session list` (Story 13.5a).
//!
//! Covers offline-safe operation, read-only behavior, flat+dir dedup,
//! end-to-end CLI output, and the shared `build_session_rows` core over
//! real `FileSystemStorage`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use assert_cmd::Command;
use predicates::str::contains;
use rustain::adapters::cli::session::list::run_session_list;
use rustain::adapters::cli::session::rows::build_session_rows;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::models::session_meta::SessionMeta;
use rustain::domain::ports::StoragePort;
use rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT;

fn rustain_cmd() -> Command {
    Command::cargo_bin("rustain").unwrap()
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

// =========================================================================
// P0-10/11: offline-safe + no provider construction
// =========================================================================

#[tokio::test]
async fn p0_11_run_session_list_does_not_construct_provider() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir,
        dir.path().to_path_buf(),
    ));

    PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    run_session_list(false, &storage).await.unwrap();

    let count = PROVIDER_CTOR_COUNT.load(Ordering::SeqCst);
    assert_eq!(
        count, 0,
        "session list must not construct any provider, got {count}"
    );
}

// =========================================================================
// P0-12: read-only — no new files, content hashes unchanged
// =========================================================================

#[tokio::test]
async fn p0_12_session_list_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    write_dir_session(&sessions_dir, "dir-sess", &meta("dir-sess", "Dir", 200, 2)).await;
    write_flat_session(
        &sessions_dir,
        "flat-sess",
        &meta("flat-sess", "Flat", 100, 1),
    )
    .await;
    // Legacy fork fixture: list_conversations() would backfill this sidecar on read.
    tokio::fs::write(
        sessions_dir.join("legacy-fork.meta.json"),
        serde_json::to_string(&serde_json::json!({
            "id": "legacy-fork",
            "title": "Legacy Fork",
            "messages": [],
            "turns": [],
            "createdAt": 90,
            "updatedAt": 91,
            "forkSource": {
                "conversationId": "parent-id",
                "messageIndex": 3,
                "checkpointId": 3
            },
            "cleanExit": true
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::fs::write(
        sessions_dir.join("legacy-fork.session.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "title": "Legacy Fork",
            "createdAt": 90,
            "updatedAt": 91,
            "messageCount": 0,
            "bookmarks": []
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let (paths_before, hashes_before) = snapshot_dir(&sessions_dir);

    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir.clone(),
        dir.path().to_path_buf(),
    ));
    run_session_list(false, &storage).await.unwrap();

    let (paths_after, hashes_after) = snapshot_dir(&sessions_dir);
    assert_eq!(
        paths_before, paths_after,
        "session list must not add or remove files"
    );
    assert_eq!(
        hashes_before, hashes_after,
        "session list must not modify any file content"
    );
}

// =========================================================================
// P0-10: flat+dir dedup characterization
// =========================================================================

#[tokio::test]
async fn p0_10_flat_and_dir_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let id = "shared-id";
    let m = meta(id, "Shared", 300, 3);
    // Create both layouts for the same id.
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
async fn e2e_session_list_json_envelope() {
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
    assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.0");
    let sessions = parsed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"].as_str().unwrap(), "json-sess");
}

#[tokio::test]
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
    assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.0");
    assert_eq!(parsed["sessions"].as_array().unwrap().len(), 0);
}

// =========================================================================
// Column/field regression tests
// =========================================================================

#[tokio::test]
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
