//! E2E tests for Story 4-3a.1 AC7: SessionMeta fork_source mirror (DF-095).
//!
//! - AC7.a: SessionMeta fork_source round-trip in both flat and directory layouts
//! - AC7.b: DF-088 regression — unknown SessionMeta fields preserved on re-save
//! - AC7.c: SessionSummary.has_fork_source flag populated from SessionMeta
//! - AC7.d: Sidebar widget renders 🔀 marker for forked conversations
//! - AC7.e: Legacy backfill — sessions with fork_source in main JSON but not
//!   in sidecar get their sidecars repaired on list_conversations()

use tempfile::TempDir;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::widgets::sidebar;
use rustain::domain::models::MessageRole;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::conversation::{
    ChatMessage, Conversation, ForkSource, generate_conversation_id,
};
use rustain::domain::models::session_meta::SessionMeta;
use rustain::domain::ports::StoragePort;
use rustain::domain::services::session_index::{SessionIndex, SessionSummary};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_message(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: generate_conversation_id(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        images: vec![],
        }
}

fn make_forked_conversation(id: &str, parent_id: &str) -> Conversation {
    Conversation {
        id: id.to_string(),
        title: "Forked Session".to_string(),
        messages: vec![make_message(MessageRole::User, "hi")],
        created_at: 1_700_000_000,
        updated_at: 1_700_000_100,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: Some(ForkSource {
            conversation_id: parent_id.to_string(),
            message_index: 2,
            checkpoint_id: CheckpointId(2),
        }),
    }
}

fn make_plain_conversation(id: &str) -> Conversation {
    Conversation {
        id: id.to_string(),
        title: "Plain Session".to_string(),
        messages: vec![make_message(MessageRole::User, "plain")],
        created_at: 1_700_000_000,
        updated_at: 1_700_000_050,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    }
}

// ── AC7.a: SessionMeta fork_source round-trip ────────────────────────────────

#[tokio::test]
async fn test_e2e_sessionmeta_fork_source_roundtrip_flat() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    // Given: a forked conversation saved without any images (flat layout)
    let conv = make_forked_conversation("forked-flat", "parent-abc");
    storage.save_conversation(&conv).await.unwrap();

    // When: we reload the SessionMeta sidecar
    let meta = storage
        .load_session_meta("forked-flat")
        .await
        .unwrap()
        .expect("sidecar should exist");

    // Then: fork_source mirrors the main conversation JSON
    let fs = meta.fork_source.expect("fork_source must be mirrored");
    assert_eq!(fs.conversation_id, "parent-abc");
    assert_eq!(fs.message_index, 2);
    assert_eq!(fs.checkpoint_id, CheckpointId(2));
}

#[tokio::test]
async fn test_e2e_sessionmeta_fork_source_roundtrip_directory() {
    // Force directory layout by attaching an image. This exercises the
    // `{id}/meta.json` sidecar write path, which must also mirror fork_source.
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    let raw = b"fake-image-bytes".to_vec();
    let img_ref = rustain::domain::models::conversation::ImageReference {
        file_name: format!("{}.png", rustain::adapters::filesystem::content_hash(&raw)),
        media_type: "image/png".to_string(),
        original_size: raw.len(),
    };
    storage
        .save_image("forked-dir", &img_ref, &raw)
        .await
        .unwrap();

    let mut conv = make_forked_conversation("forked-dir", "parent-xyz");
    conv.messages[0].images = vec![img_ref.clone()];
    storage.save_conversation(&conv).await.unwrap();

    // Directory layout should be in place.
    assert!(sessions.join("forked-dir/meta.json").is_file());

    let meta = storage
        .load_session_meta("forked-dir")
        .await
        .unwrap()
        .expect("directory sidecar should exist");
    let fs = meta
        .fork_source
        .expect("directory meta must mirror fork_source");
    assert_eq!(fs.conversation_id, "parent-xyz");
    assert_eq!(fs.checkpoint_id, CheckpointId(2));
}

// ── AC7.b: DF-088 regression — unknown fields preserved on re-save ──────────

#[test]
fn test_e2e_sessionmeta_unknown_field_preserved() {
    // Given: SessionMeta JSON written by a hypothetical newer rustain with an
    // extra field that today's code does not know about.
    let future_json = r#"{
        "version": 3,
        "title": "Future Session",
        "createdAt": 1700000000,
        "updatedAt": 1700000100,
        "messageCount": 4,
        "bookmarks": [],
        "labels": ["priority", "review"],
        "customReviewer": {
            "name": "bob",
            "approved": true
        }
    }"#;

    // When: parsed by the current SessionMeta type and re-serialized
    let meta: SessionMeta = serde_json::from_str(future_json).unwrap();
    let resaved = serde_json::to_string(&meta).unwrap();
    let reparsed: SessionMeta = serde_json::from_str(&resaved).unwrap();

    // Then: both the scalar and object unknown fields survive the round-trip
    assert!(
        resaved.contains("\"labels\""),
        "unknown scalar array must survive round-trip: {}",
        resaved
    );
    assert!(
        resaved.contains("\"customReviewer\""),
        "unknown nested object must survive round-trip: {}",
        resaved
    );
    // Sanity-check the extra map contents match on both sides.
    assert_eq!(meta.extra, reparsed.extra);
}

// ── AC7.c: SessionSummary.has_fork_source populated from SessionMeta ────────

#[tokio::test]
async fn test_e2e_session_summary_has_fork_flag() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    // Given: one forked and one plain session on disk
    storage
        .save_conversation(&make_forked_conversation("forked-1", "parent-a"))
        .await
        .unwrap();
    storage
        .save_conversation(&make_plain_conversation("plain-1"))
        .await
        .unwrap();

    // When: list_conversations → SessionIndex::build
    let summaries = storage.list_conversations().await.unwrap();
    let index = SessionIndex::build(summaries);

    // Then: the forked entry has has_fork_source == true; the plain entry does not
    let forked = index.get("forked-1").expect("forked session present");
    assert!(
        forked.has_fork_source,
        "forked session must have has_fork_source=true"
    );
    let plain = index.get("plain-1").expect("plain session present");
    assert!(
        !plain.has_fork_source,
        "plain session must have has_fork_source=false"
    );
}

// ── AC7.d: Sidebar widget renders 🔀 marker ──────────────────────────────────

#[test]
fn test_e2e_sidebar_renders_fork_marker() {
    let theme = rustain::adapters::tui::theme::Theme::dark();
    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    // Build two in-memory SessionSummary entries: one forked, one not.
    let mut forked = SessionSummary::new(
        "forked-a".to_string(),
        "Forked Chat".to_string(),
        1_700_000_100,
        1_699_999_000,
        3,
    );
    forked.has_fork_source = true;

    let plain = SessionSummary::new(
        "plain-b".to_string(),
        "Plain Chat".to_string(),
        1_700_000_000,
        1_699_999_000,
        5,
    );

    let entries = vec![forked, plain];

    terminal
        .draw(|frame| {
            let area = frame.area();
            sidebar::render_history_panel(area, frame.buffer_mut(), &entries, 0, None, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let content: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();

    assert!(
        content.contains('🔀'),
        "sidebar must render 🔀 marker for forked session. Rendered:\n{}",
        content
    );
    assert!(content.contains("Forked Chat"));
    assert!(content.contains("Plain Chat"));
}

#[test]
fn test_e2e_sidebar_no_fork_marker_for_plain_session() {
    // Sanity check: rendering only plain sessions must not emit 🔀 (guards
    // against an accidental unconditional marker).
    let theme = rustain::adapters::tui::theme::Theme::dark();
    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    let entries = vec![SessionSummary::new(
        "plain-only".to_string(),
        "Only Plain".to_string(),
        1_700_000_000,
        1_699_999_000,
        2,
    )];

    terminal
        .draw(|frame| {
            let area = frame.area();
            sidebar::render_history_panel(area, frame.buffer_mut(), &entries, 0, None, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let content: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();

    assert!(
        !content.contains('🔀'),
        "sidebar must NOT render 🔀 when no session is forked. Rendered:\n{}",
        content
    );
}

// ── AC7.e: Legacy backfill for forks created before AC7 shipped ─────────────

#[tokio::test]
async fn test_e2e_legacy_forked_session_backfill_flat() {
    // Given: a flat session on disk where the main conversation JSON carries
    // fork_source but the sidecar does not (simulating a session created by
    // Story 4-3a before AC7 mirrored the field).
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    let main_json = r#"{
        "id": "legacy-fork",
        "title": "Legacy Fork",
        "messages": [
            {
                "id": "m0",
                "role": "user",
                "content": "hello",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000000,
                "tokenCount": null
            }
        ],
        "createdAt": 1700000000,
        "updatedAt": 1700000100,
        "forkSource": {
            "conversationId": "old-parent",
            "messageIndex": 2,
            "checkpointId": 2
        },
        "cleanExit": true
    }"#;
    std::fs::write(sessions.join("legacy-fork.meta.json"), main_json).unwrap();

    // Sidecar WITHOUT fork_source (simulates pre-AC7 write).
    let stale_sidecar = r#"{
        "version": 1,
        "title": "Legacy Fork",
        "createdAt": 1700000000,
        "updatedAt": 1700000100,
        "messageCount": 1,
        "bookmarks": []
    }"#;
    std::fs::write(sessions.join("legacy-fork.session.json"), stale_sidecar).unwrap();

    // When: list_conversations runs (triggers backfill)
    let storage = FileSystemStorage::new(sessions.clone());
    let summaries = storage.list_conversations().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert!(
        summaries[0].has_fork_source,
        "list_conversations must report fork_source from main JSON for legacy sessions"
    );

    // Then: the sidecar on disk has been repaired — a subsequent SessionMeta
    // load must now see fork_source populated.
    let meta = storage
        .load_session_meta("legacy-fork")
        .await
        .unwrap()
        .expect("sidecar must still exist");
    let fs = meta
        .fork_source
        .expect("legacy backfill must populate fork_source in sidecar");
    assert_eq!(fs.conversation_id, "old-parent");
    assert_eq!(fs.message_index, 2);
    assert_eq!(fs.checkpoint_id, CheckpointId(2));
}

#[tokio::test]
async fn test_e2e_legacy_forked_session_backfill_directory() {
    // Directory-layout variant: session dir has {id}/conversation.json with
    // fork_source but {id}/meta.json without it.
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let dir = sessions.join("legacy-dir-fork");
    std::fs::create_dir_all(&dir).unwrap();

    let main_json = r#"{
        "id": "legacy-dir-fork",
        "title": "Legacy Dir Fork",
        "messages": [
            {
                "id": "m0",
                "role": "user",
                "content": "hello",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000000,
                "tokenCount": null
            }
        ],
        "createdAt": 1700000000,
        "updatedAt": 1700000200,
        "forkSource": {
            "conversationId": "parent-dir",
            "messageIndex": 1,
            "checkpointId": 1
        },
        "cleanExit": true
    }"#;
    std::fs::write(dir.join("conversation.json"), main_json).unwrap();

    let stale_meta = r#"{
        "version": 1,
        "title": "Legacy Dir Fork",
        "createdAt": 1700000000,
        "updatedAt": 1700000200,
        "messageCount": 1,
        "bookmarks": []
    }"#;
    std::fs::write(dir.join("meta.json"), stale_meta).unwrap();

    let storage = FileSystemStorage::new(sessions.clone());
    let summaries = storage.list_conversations().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].has_fork_source);

    let meta = storage
        .load_session_meta("legacy-dir-fork")
        .await
        .unwrap()
        .expect("directory sidecar must still exist");
    let fs = meta
        .fork_source
        .expect("directory legacy backfill must populate fork_source");
    assert_eq!(fs.conversation_id, "parent-dir");
    assert_eq!(fs.checkpoint_id, CheckpointId(1));
}
