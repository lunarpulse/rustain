//! Integration tests for `rustain session delete` (Story 13.5b).
//!
//! Mirrors `tests/cli_session_list.rs`: real `FileSystemStorage`, tempdir
//! workspaces, offline-safe, with injected `SessionHolderPort` fakes for the
//! in-use guard.

#![cfg(feature = "test-instrumentation")]
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use async_trait::async_trait;
use rustain::adapters::cli::session::delete::run_session_delete;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::models::session_meta::SessionMeta;
use rustain::domain::ports::{
    HeldSession, HolderState, SessionHolderPort, StoragePort, WorkspaceEntry,
    WorkspaceRegistryError, WorkspaceRegistryReaderPort,
};
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
    ) -> Result<Vec<WorkspaceEntry>, rustain::domain::ports::WorkspaceRegistryError> {
        Ok(vec![])
    }
}

fn storage_for_path(path: &Path) -> Arc<dyn StoragePort> {
    Arc::new(FileSystemStorage::with_workspace_root(
        path.join(".claude").join("sessions"),
        path.to_path_buf(),
    ))
}

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

async fn write_dir_session(sessions_dir: &Path, id: &str, m: &SessionMeta) {
    let dir = sessions_dir.join(id);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("meta.json"), serde_json::to_string(m).unwrap())
        .await
        .unwrap();
    let conv = serde_json::json!({
        "id": id,
        "title": m.title,
        "messages": [],
        "createdAt": m.created_at,
        "updatedAt": m.updated_at,
    });
    tokio::fs::write(
        dir.join("conversation.json"),
        serde_json::to_string(&conv).unwrap(),
    )
    .await
    .unwrap();
}

#[allow(dead_code)]
async fn write_flat_session(sessions_dir: &Path, id: &str, m: &SessionMeta) {
    tokio::fs::write(
        sessions_dir.join(format!("{}.session.json", id)),
        serde_json::to_string(m).unwrap(),
    )
    .await
    .unwrap();
}

/// Snapshot the content hashes + path set of a directory recursively.
fn snapshot_dir(
    dir: &Path,
) -> (
    std::collections::HashSet<std::path::PathBuf>,
    Vec<(std::path::PathBuf, String)>,
) {
    fn walk(
        dir: &Path,
        base: &Path,
        paths: &mut std::collections::HashSet<std::path::PathBuf>,
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
    let mut paths = std::collections::HashSet::new();
    let mut hashes = vec![];
    walk(dir, dir, &mut paths, &mut hashes);
    hashes.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    (paths, hashes)
}

// ---------------------------------------------------------------------------
// Fakes for the in-use guard
// ---------------------------------------------------------------------------

struct FakeHolder {
    state: HolderState,
    calls: AtomicUsize,
}

impl FakeHolder {
    fn new(state: HolderState) -> Self {
        Self {
            state,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionHolderPort for FakeHolder {
    async fn live_holder(&self, _workspace: &Path) -> HolderState {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.state.clone()
    }
}

fn no_daemon() -> FakeHolder {
    FakeHolder::new(HolderState::NoDaemon)
}

fn held(id: &str, pid: u32) -> FakeHolder {
    FakeHolder::new(HolderState::HeldBy(HeldSession {
        conversation_id: id.to_string(),
        pid,
        channels: vec![rustain::domain::models::channel_kind::ChannelKind::Terminal],
    }))
}

fn held_other(id: &str, pid: u32) -> FakeHolder {
    FakeHolder::new(HolderState::HeldBy(HeldSession {
        conversation_id: id.to_string(),
        pid,
        channels: vec![rustain::domain::models::channel_kind::ChannelKind::Terminal],
    }))
}

fn unknown() -> FakeHolder {
    FakeHolder::new(HolderState::Unknown)
}

// ---------------------------------------------------------------------------
// P0-1: keystone — deleted id == confirmed id
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_1_deleted_id_matches_confirmed_id() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "sess-abc",
        &meta("sess-abc", "Alpha", 300, 2),
    )
    .await;

    let mut input = Cursor::new("y\n");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("sess-abc".to_string()),
        false,
        false,
        None,
        false,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        true,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let stdout = String::from_utf8_lossy(&output);
    assert!(stdout.contains("sess-abc"));
    assert!(stdout.contains("Alpha"));

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert!(summaries.is_empty());
}

// ---------------------------------------------------------------------------
// P0-2: prefix resolution
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_2_exact_id_wins_over_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "abc", &meta("abc", "Short", 300, 1)).await;
    write_dir_session(&sessions_dir, "abcdef", &meta("abcdef", "Long", 400, 1)).await;

    let mut input = Cursor::new("y\n");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("abc".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        true,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "abcdef");
}

#[tokio::test]
#[serial(session_cli)]
async fn p0_2_ambiguous_prefix_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "abc1", &meta("abc1", "One", 300, 1)).await;
    write_dir_session(&sessions_dir, "abc2", &meta("abc2", "Two", 400, 1)).await;

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("abc".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_err());

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert_eq!(summaries.len(), 2);
}

#[tokio::test]
#[serial(session_cli)]
async fn p0_2_not_found_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("missing".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
#[serial(session_cli)]
async fn p0_2_empty_session_deletes_by_full_id() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "empty-sess",
        &meta("empty-sess", "Empty", 300, 0),
    )
    .await;

    let mut input = Cursor::new("y\n");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("empty-sess".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        true,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert!(summaries.is_empty());
}

// ---------------------------------------------------------------------------
// P0-3: in-use guard
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_3_held_target_refused_even_with_force() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "held-id", &meta("held-id", "Held", 300, 1)).await;

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let holder = held("held-id", 12345);
    let result = run_session_delete(
        Some("held-id".to_string()),
        false,
        false,
        None,
        true, // force
        false,
        false,
        dir.path(),
        storage_for_path,
        &holder,
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_err());

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert_eq!(summaries.len(), 1);

    let stdout = String::from_utf8_lossy(&output);
    assert!(stdout.contains("in use"));
    assert!(stdout.contains("12345"));
}

#[tokio::test]
#[serial(session_cli)]
async fn p0_3_held_other_target_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "target-id",
        &meta("target-id", "Target", 300, 1),
    )
    .await;
    write_dir_session(
        &sessions_dir,
        "other-id",
        &meta("other-id", "Other", 400, 1),
    )
    .await;

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let holder = held_other("other-id", 12345);
    let result = run_session_delete(
        Some("target-id".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &holder,
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "other-id");
}

#[tokio::test]
#[serial(session_cli)]
async fn p0_3b_unknown_fail_closed_force_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "unknown-id",
        &meta("unknown-id", "Unknown", 300, 1),
    )
    .await;

    // Without force: refused.
    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("unknown-id".to_string()),
        false,
        false,
        None,
        false,
        false,
        false,
        dir.path(),
        storage_for_path,
        &unknown(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_err());

    // With force: deletes.
    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("unknown-id".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &unknown(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert!(summaries.is_empty());
}

// ---------------------------------------------------------------------------
// P0-4: offline-safe — no provider construction
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_4_delete_does_not_construct_provider() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "offline-id",
        &meta("offline-id", "Offline", 300, 1),
    )
    .await;

    PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("offline-id".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let count = PROVIDER_CTOR_COUNT.load(Ordering::SeqCst);
    assert_eq!(
        count, 0,
        "session delete must not construct any provider, got {count}"
    );
}

// ---------------------------------------------------------------------------
// P0-5: footprint-only + others untouched
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_5_others_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "keep-id", &meta("keep-id", "Keep", 200, 1)).await;
    write_dir_session(&sessions_dir, "drop-id", &meta("drop-id", "Drop", 300, 1)).await;

    let (paths_before, hashes_before) = snapshot_dir(&sessions_dir);

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("drop-id".to_string()),
        false,
        false,
        None,
        true,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let (paths_after, hashes_after) = snapshot_dir(&sessions_dir);

    let target_paths: std::collections::HashSet<_> = paths_before
        .iter()
        .filter(|p| p.starts_with("drop-id"))
        .cloned()
        .collect();
    let keep_paths: std::collections::HashSet<_> = paths_before
        .iter()
        .filter(|p| p.starts_with("keep-id"))
        .cloned()
        .collect();

    assert!(
        target_paths.iter().all(|p| !paths_after.contains(p)),
        "target session files were not removed"
    );
    assert!(
        keep_paths.iter().all(|p| paths_after.contains(p)),
        "untouched session files disappeared"
    );
    for p in keep_paths {
        let before = hashes_before.iter().find(|(path, _)| path == &p).unwrap();
        let after = hashes_after.iter().find(|(path, _)| path == &p).unwrap();
        assert_eq!(before.1, after.1, "untouched file {p:?} changed");
    }
}

// ---------------------------------------------------------------------------
// P0-6: count-drift guard
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_6_count_drift_guard_rejects_wrong_count() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    for i in 0..4 {
        write_dir_session(
            &sessions_dir,
            &format!("sess-{}", i),
            &meta(
                &format!("sess-{}", i),
                &format!("S{}", i),
                100 + i as i64,
                1,
            ),
        )
        .await;
    }

    // The captured set has 4 sessions; typing 3 must cancel (count-drift guard).
    let mut input = Cursor::new("3\n");
    let mut output = Vec::new();
    let result = run_session_delete(
        None,
        true,
        false,
        None,
        false,
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        true,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok(), "wrong count should cancel, not error");

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert_eq!(summaries.len(), 4, "nothing should have been deleted");
}

// ---------------------------------------------------------------------------
// P0-7: JSON shape
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_7_json_shape_and_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "json-id", &meta("json-id", "JSON", 300, 1)).await;

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("json-id".to_string()),
        false,
        false,
        None,
        true,
        false,
        true,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let stdout = String::from_utf8_lossy(&output);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.0");
    assert_eq!(parsed["dry_run"].as_bool(), Some(false));
    let deleted = parsed["deleted"].as_array().unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["id"].as_str().unwrap(), "json-id");
    assert!(
        deleted[0]["workspace"]
            .as_str()
            .unwrap()
            .contains(dir.path().to_str().unwrap())
    );
    assert_eq!(parsed["refused"].as_array().unwrap().len(), 0);

    let forbidden = ["socket", "nonce", "secret", "token", "url"];
    let lowered = stdout.to_lowercase();
    for word in forbidden {
        assert!(!lowered.contains(word), "JSON must not contain '{word}'");
    }
}

// ---------------------------------------------------------------------------
// P0-9: dry-run
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_9_dry_run_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "dry-id", &meta("dry-id", "Dry", 300, 1)).await;

    let (paths_before, _hashes_before) = snapshot_dir(&sessions_dir);

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("dry-id".to_string()),
        false,
        false,
        None,
        true,
        true,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok());

    let (paths_after, _hashes_after) = snapshot_dir(&sessions_dir);
    assert_eq!(paths_before, paths_after);

    let stdout = String::from_utf8_lossy(&output);
    assert!(stdout.contains("Would delete"));
}

// ---------------------------------------------------------------------------
// End-to-end CLI
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn e2e_delete_single_with_force() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "cli-id", &meta("cli-id", "CLI", 300, 1)).await;

    let mut cmd = rustain_cmd();
    cmd.current_dir(dir.path())
        .arg("session")
        .arg("delete")
        .arg("cli-id")
        .arg("--force");
    cmd.assert().success();

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert!(summaries.is_empty());
}

#[tokio::test]
#[serial(session_cli)]
async fn e2e_delete_all_with_force() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(&sessions_dir, "a", &meta("a", "A", 100, 1)).await;
    write_dir_session(&sessions_dir, "b", &meta("b", "B", 200, 1)).await;

    let mut cmd = rustain_cmd();
    cmd.current_dir(dir.path())
        .arg("session")
        .arg("delete")
        .arg("--all")
        .arg("--force");
    cmd.assert().success();

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert!(summaries.is_empty());
}

#[tokio::test]
#[serial(session_cli)]
async fn e2e_delete_json_output() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "json-cli",
        &meta("json-cli", "JSON CLI", 300, 1),
    )
    .await;

    let output = rustain_cmd()
        .current_dir(dir.path())
        .arg("session")
        .arg("delete")
        .arg("json-cli")
        .arg("--force")
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.0");
    assert_eq!(parsed["deleted"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// P0-5b: symlink escape refused — victim survives (AI-12.3 C6/D1)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
#[serial(session_cli)]
async fn p0_5b_symlink_escape_refused_victim_survives() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    // Create a legitimate session so the workspace is non-empty.
    write_dir_session(
        &sessions_dir,
        "legit-id",
        &meta("legit-id", "Legit", 300, 1),
    )
    .await;

    // Create a victim directory outside sessions_dir.
    let victim_dir = dir.path().join("victim");
    tokio::fs::create_dir_all(&victim_dir).await.unwrap();
    let victim_file = victim_dir.join("precious.txt");
    tokio::fs::write(&victim_file, b"do not delete me")
        .await
        .unwrap();

    // Plant a symlink inside sessions_dir pointing to the victim.
    let symlink_path = sessions_dir.join("evil-link");
    tokio::fs::symlink(&victim_dir, &symlink_path)
        .await
        .unwrap();

    // Also create the meta so the storage adapter can discover this "session".
    let evil_meta_dir = victim_dir.clone();
    tokio::fs::write(
        evil_meta_dir.join("meta.json"),
        serde_json::to_string(&meta("evil-link", "Evil", 400, 1)).unwrap(),
    )
    .await
    .unwrap();
    let conv = serde_json::json!({
        "id": "evil-link",
        "title": "Evil",
        "messages": [],
        "createdAt": 390,
        "updatedAt": 400,
    });
    tokio::fs::write(
        evil_meta_dir.join("conversation.json"),
        serde_json::to_string(&conv).unwrap(),
    )
    .await
    .unwrap();

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some("evil-link".to_string()),
        false,
        false,
        None,
        true, // force — bypass confirmation
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;

    assert!(result.is_err(), "symlink escape must be refused");

    // Victim file survives.
    let content = tokio::fs::read_to_string(&victim_file).await.unwrap();
    assert_eq!(content, "do not delete me", "victim must survive");
}

// ---------------------------------------------------------------------------
// P0-6b: cross-workspace same-id isolation (AI-12.3 D-2)
// ---------------------------------------------------------------------------

struct FakeMultiWorkspaceReader {
    entries: Vec<WorkspaceEntry>,
}

#[async_trait]
impl WorkspaceRegistryReaderPort for FakeMultiWorkspaceReader {
    async fn live_workspaces(&self) -> Result<Vec<WorkspaceEntry>, WorkspaceRegistryError> {
        Ok(self.entries.clone())
    }
}

#[tokio::test]
#[serial(session_cli)]
async fn p0_6b_cross_workspace_same_id_isolation() {
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();

    let sessions_a = ws_a.path().join(".claude").join("sessions");
    let sessions_b = ws_b.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_a).await.unwrap();
    tokio::fs::create_dir_all(&sessions_b).await.unwrap();

    // Same id in both workspaces.
    let shared_id = "same-nanoid";
    write_dir_session(&sessions_a, shared_id, &meta(shared_id, "Alpha", 300, 2)).await;
    write_dir_session(&sessions_b, shared_id, &meta(shared_id, "Beta", 400, 3)).await;

    // Snapshot workspace B before the delete.
    let (paths_before, hashes_before) = snapshot_dir(&sessions_b);
    assert!(!paths_before.is_empty(), "workspace B must have files");

    let reader = FakeMultiWorkspaceReader {
        entries: vec![
            WorkspaceEntry {
                path: ws_a.path().to_path_buf(),
                last_seen: 100,
            },
            WorkspaceEntry {
                path: ws_b.path().to_path_buf(),
                last_seen: 200,
            },
        ],
    };

    // Delete the session in workspace A only (single-target, no --all).
    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let result = run_session_delete(
        Some(shared_id.to_string()),
        false,
        false,
        None,
        true, // force
        false,
        false,
        ws_a.path(),
        storage_for_path,
        &no_daemon(),
        &reader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok(), "delete in workspace A must succeed");

    // Workspace A: session gone.
    let storage_a = storage_for_path(ws_a.path());
    let summaries_a = storage_a.list_conversations_read_only().await.unwrap();
    assert!(
        summaries_a.is_empty(),
        "workspace A session must be deleted"
    );

    // Workspace B: byte-identical.
    let (paths_after, hashes_after) = snapshot_dir(&sessions_b);
    assert_eq!(
        paths_before, paths_after,
        "workspace B paths must be unchanged"
    );
    assert_eq!(
        hashes_before, hashes_after,
        "workspace B contents must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// P0-7b: JSON refused/in-use path — no secrets (AI-12.3 C-1)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_7b_json_refused_in_use_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    write_dir_session(
        &sessions_dir,
        "held-json",
        &meta("held-json", "Held JSON", 300, 1),
    )
    .await;

    let mut input = Cursor::new("");
    let mut output = Vec::new();
    let holder = held("held-json", 54321);
    // --json requires --force or --dry-run; use --dry-run to get the JSON refused path.
    let result = run_session_delete(
        Some("held-json".to_string()),
        false,
        false,
        None,
        false,
        true, // dry_run — allows --json without --force
        true, // json
        dir.path(),
        storage_for_path,
        &holder,
        &EmptyWorkspaceReader,
        false,
        &mut input,
        &mut output,
    )
    .await;
    // in-use is an error even under dry-run
    assert!(result.is_err());

    let stdout = String::from_utf8_lossy(&output);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("output must be valid JSON");
    let refused = parsed["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["reason"].as_str().unwrap(), "in_use");
    assert!(
        refused[0]["holder"].is_object(),
        "holder must be present for in_use"
    );

    let forbidden = ["socket", "nonce", "secret", "boot_id"];
    let lowered = stdout.to_lowercase();
    for word in forbidden {
        assert!(
            !lowered.contains(word),
            "refused JSON must not contain '{word}'"
        );
    }
}

// ---------------------------------------------------------------------------
// P0-6c: count-drift positive case — correct count deletes all (AI-12.3 C5)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial(session_cli)]
async fn p0_6c_count_drift_correct_count_deletes_all() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join(".claude").join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
    for i in 0..3 {
        write_dir_session(
            &sessions_dir,
            &format!("cnt-{}", i),
            &meta(&format!("cnt-{}", i), &format!("C{}", i), 100 + i as i64, 1),
        )
        .await;
    }

    // Type the correct count "3".
    let mut input = Cursor::new("3\n");
    let mut output = Vec::new();
    let result = run_session_delete(
        None,
        true, // --all
        false,
        None,
        false, // no force — interactive prompt
        false,
        false,
        dir.path(),
        storage_for_path,
        &no_daemon(),
        &EmptyWorkspaceReader,
        true, // is_tty
        &mut input,
        &mut output,
    )
    .await;
    assert!(result.is_ok(), "correct count must succeed");

    let storage = storage_for_path(dir.path());
    let summaries = storage.list_conversations_read_only().await.unwrap();
    assert!(summaries.is_empty(), "all sessions must be deleted");
}
