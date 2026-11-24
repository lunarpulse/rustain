//! E2E tests for filesystem adapter hardening (Phase E of Epic 4-6).
//!
//! E1.2 (DF-096b): `save_directory_layout` rolls back `conversation.json` when
//!   `meta.json` rename fails — leaves flat files as the authoritative copy.
//!
//! E3 (DF-099): `detect_layout` uses a single `read_dir` pass — a session
//!   directory WITHOUT `conversation.json` must NOT be classified as Directory.

use tempfile::TempDir;

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::models::MessageRole;
use rustain::domain::models::conversation::{ChatMessage, Conversation, generate_conversation_id};
use rustain::domain::ports::StoragePort;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_simple_conversation(id: &str) -> Conversation {
    Conversation {
        id: id.to_string(),
        title: "Filesystem E2E Test".to_string(),
        messages: vec![
            ChatMessage {
                synthetic: false,
                id: generate_conversation_id(),
                role: MessageRole::User,
                content: "Hello".to_string(),
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: 1700000000,
                token_count: None,
                stop_reason: None,
                images: vec![],
                origin: rustain::domain::models::ChannelKind::Terminal,
                authorship: rustain::domain::models::Authorship::HumanWritten,
                send_authorization: rustain::domain::models::SendAuthorization::Unknown,
                retracted_at_ms: None,
                history_source: Default::default(),
            },
            ChatMessage {
                synthetic: false,
                id: generate_conversation_id(),
                role: MessageRole::Assistant,
                content: "Hi!".to_string(),
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: 1700000001,
                token_count: None,
                stop_reason: None,
                images: vec![],
                origin: rustain::domain::models::ChannelKind::Terminal,
                authorship: rustain::domain::models::Authorship::HumanWritten,
                send_authorization: rustain::domain::models::SendAuthorization::Unknown,
                retracted_at_ms: None,
                history_source: Default::default(),
            },
        ],
        turns: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
        persist: Default::default(),
    }
}

// ── E1.2: sidecar rename failure after the transcript was published ─────────

/// E1.2 (DF-096b, superseded by D-24 / AC2): when the `meta.json` rename fails after
/// `conversation.json` was renamed into place, the transcript rename is the publication point.
/// The save reports a *partial commit*, the accepted transcript stays (the pre-D-24 rollback
/// removed the only copy of the session), and the pending sidecar effect stays recorded.
///
/// We simulate the failure by placing a DIRECTORY at the `meta.json` path of a directory-layout
/// session: renaming a regular file over a directory fails with EISDIR.
#[tokio::test]
async fn test_save_conversation_rollback_on_meta_rename_failure() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    let conv_id = "rollback-test-conv";
    let session_dir = sessions_dir.join(conv_id);
    let conv_path = session_dir.join("conversation.json");
    let meta_path = session_dir.join("meta.json");

    // An image reference forces the directory layout on the first save.
    let storage = FileSystemStorage::new(sessions_dir.clone());
    let mut conv = make_simple_conversation(conv_id);
    conv.messages[0].images = vec![rustain::domain::models::ImageReference {
        file_name: "0123456789abcdef.png".into(),
        media_type: "image/png".into(),
        original_size: 1,
    }];
    storage.save_conversation(&conv).await.unwrap();
    assert!(conv_path.exists(), "precondition: directory layout");

    // Block the sidecar rename, then save again.
    tokio::fs::remove_file(&meta_path).await.unwrap();
    tokio::fs::create_dir_all(&meta_path).await.unwrap();
    conv.messages
        .push(make_simple_conversation(conv_id).messages.remove(1));
    let result = storage.save_conversation(&conv).await;

    assert!(
        matches!(
            result,
            Err(rustain::domain::errors::StorageError::PartialCommit(_))
        ),
        "the blocked sidecar is a partial commit, not an unchanged failure: {result:?}"
    );
    assert!(
        conv_path.exists(),
        "the published transcript is never removed by a later sidecar failure"
    );
    let loaded = storage.load_conversation(conv_id).await.unwrap().unwrap();
    assert_eq!(
        loaded.messages.len(),
        3,
        "the new transcript is what a reader sees"
    );
}

// ── E3: detect_layout single read_dir pass ───────────────────────────────────

/// E3 (DF-099): A session directory that exists but does NOT contain
/// `conversation.json` must be classified as Flat (or Missing), never Directory.
///
/// This validates the new single `read_dir` pass in `detect_layout` — the old
/// two-call implementation (metadata(dir) → metadata(conv_file)) would detect
/// the directory's existence and then look for conv_file, but the new path
/// enumerates the directory contents and checks for the file there.
///
/// The test explicitly creates a session dir with only ancillary files (simulating
/// an orphaned checkpoints/images dir created without a full migration), then
/// verifies that the flat conversation is still the authoritative copy.
#[tokio::test]
async fn test_detect_layout_single_readdir_pass() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

    let storage = FileSystemStorage::new(sessions_dir.clone());

    // ── Positive case: directory WITH conversation.json → Directory layout ──

    let dir_id = generate_conversation_id();
    let conv_dir = sessions_dir.join(&dir_id);
    tokio::fs::create_dir_all(&conv_dir).await.unwrap();
    // Write a real conversation via the storage port so conversation.json appears.
    let conv_dir_conv = make_simple_conversation(&dir_id);
    // Force directory layout by saving as directory: manually write conv.json + meta.json.
    let persisted_json = serde_json::json!({
        "schemaVersion": 1,
        "id": dir_id,
        "title": "Dir layout conv",
        "messages": [],
        "createdAt": 1700000000,
        "updatedAt": 1700000001,
        "cleanExit": true
    })
    .to_string();
    tokio::fs::write(conv_dir.join("conversation.json"), &persisted_json)
        .await
        .unwrap();
    let meta_json = serde_json::json!({
        "id": dir_id,
        "title": "Dir layout conv",
        "createdAt": 1700000000,
        "updatedAt": 1700000001,
        "open": false
    })
    .to_string();
    tokio::fs::write(conv_dir.join("meta.json"), &meta_json)
        .await
        .unwrap();

    // load_conversation should find it via Directory layout.
    let loaded = storage.load_conversation(&dir_id).await.unwrap();
    assert!(
        loaded.is_some(),
        "directory-layout conversation must be loadable"
    );
    assert_eq!(
        loaded.unwrap().id,
        dir_id,
        "loaded directory-layout conversation must have correct id"
    );

    // ── Negative case: directory WITHOUT conversation.json → falls through ──

    let flat_id = generate_conversation_id();
    let orphan_dir = sessions_dir.join(&flat_id);
    tokio::fs::create_dir_all(&orphan_dir).await.unwrap();
    // Only write a checkpoints file (no conversation.json — orphaned sidecar dir).
    tokio::fs::write(orphan_dir.join("checkpoints.json"), b"{}")
        .await
        .unwrap();

    // Also write a flat conversation file for the same ID.
    let flat_conv = make_simple_conversation(&flat_id);
    // Save via storage — this will use flat layout (no images, detect_layout returns Missing
    // for the directory because conversation.json is absent, then the flat write happens).
    storage.save_conversation(&flat_conv).await.unwrap();

    // load_conversation must find the FLAT conversation, not get confused by the orphan dir.
    let loaded_flat = storage.load_conversation(&flat_id).await.unwrap();
    assert!(
        loaded_flat.is_some(),
        "flat conversation must be loadable even when an orphaned directory exists"
    );
    assert_eq!(
        loaded_flat.unwrap().id,
        flat_id,
        "loaded flat conversation must have correct id"
    );

    // The orphan directory must still exist (we did not delete it).
    assert!(
        orphan_dir.exists(),
        "orphaned session directory must not be removed"
    );

    // Verify that the storage did NOT create a conversation.json inside the orphan dir.
    assert!(
        !orphan_dir.join("conversation.json").exists(),
        "save must not write conversation.json into orphaned directory"
    );

    // Drop unused variable to avoid compiler warning.
    let _ = conv_dir_conv;
}
