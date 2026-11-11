//! E2E tests for Story 4-3a.1 AC7: SessionMeta fork_source mirror (DF-095).
//!
//! - AC7.a: SessionMeta fork_source round-trip in both flat and directory layouts
//! - AC7.b: DF-088 regression — unknown SessionMeta fields preserved on re-save
//! - AC7.c: SessionSummary.has_fork_source flag populated from SessionMeta
//! - AC7.d: History window renders 🔀 marker for forked conversations
//! - AC7.e: Legacy backfill — sessions with fork_source in main JSON but not
//!   in sidecar get their sidecars repaired on list_conversations()

use tempfile::TempDir;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::history_window;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout::compute_layout;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::occlusion::OpaqueRects;
use rustain::adapters::tui::presentation::{Shell, compose};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::widgets::history_inspector::row_text;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::conversation::{
    ChatMessage, Conversation, ForkSource, generate_conversation_id,
};
use rustain::domain::models::session_meta::SessionMeta;
use rustain::domain::models::{ConversationSummary, DensityMode, MessageRole};
use rustain::domain::ports::StoragePort;
use rustain::domain::services::session_index::SessionIndex;

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
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: rustain::domain::models::Authorship::HumanWritten,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
    }
}

fn make_forked_conversation(id: &str, parent_id: &str) -> Conversation {
    Conversation {
        id: id.to_string(),
        title: "Forked Session".to_string(),
        messages: vec![make_message(MessageRole::User, "hi")],
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_100,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: Some(ForkSource {
            conversation_id: parent_id.to_string(),
            message_index: 2,
            checkpoint_id: CheckpointId(2),
        }),
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    }
}

fn make_plain_conversation(id: &str) -> Conversation {
    Conversation {
        id: id.to_string(),
        title: "Plain Session".to_string(),
        messages: vec![make_message(MessageRole::User, "plain")],
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_050,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
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

// ── AC7.d: the History window renders the 🔀 marker ──────────────────────────

const NOW: i64 = 1_700_000_200;

fn summary(
    id: &str,
    title: &str,
    updated_at: i64,
    count: usize,
    fork: bool,
) -> ConversationSummary {
    ConversationSummary {
        id: id.to_string(),
        title: title.to_string(),
        created_at: 1_699_999_000,
        updated_at,
        message_count: count,
        has_fork_source: fork,
    }
}

/// Open the History window on `index`, paint it through the production compositor and return
/// the screen rows.
fn painted_history(index: &SessionIndex) -> Vec<String> {
    let (width, height) = (60, 16);
    let mut state = TuiState::new(width, height);
    history_window::open(&mut state, index, NOW);
    let layout = compute_layout(
        Rect::new(0, 0, width, height),
        &state.theme,
        &state.input_buffer,
        1,
        false,
        DensityMode::Focus,
    )
    .expect("a real chat viewport");
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            compose(
                frame,
                &state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

#[test]
fn test_e2e_history_window_renders_fork_marker() {
    // Two in-memory entries: one forked, one not.
    let index = SessionIndex::build(vec![
        summary("forked-a", "Forked Chat", 1_700_000_100, 3, true),
        summary("plain-b", "Plain Chat", 1_700_000_000, 5, false),
    ]);
    let content = painted_history(&index).join("\n");

    assert!(
        content.contains('🔀'),
        "the History window must render the 🔀 marker for a forked session. Rendered:\n{content}"
    );
    assert!(content.contains("Forked Chat"));
    assert!(content.contains("Plain Chat"));

    // The marker belongs to the forked row only, and the row is the shared row builder's text.
    let forked = index.get("forked-a").expect("forked entry");
    let plain = index.get("plain-b").expect("plain entry");
    let forked_row = row_text(forked, NOW, 40);
    assert!(forked_row.contains("🔀 Forked Chat"), "{forked_row}");
    assert!(!row_text(plain, NOW, 40).contains('🔀'));
    let rows = painted_history(&index);
    let painted_forked = rows
        .iter()
        .find(|row| row.contains("Forked Chat"))
        .expect("the forked row is painted");
    let painted_plain = rows
        .iter()
        .find(|row| row.contains("Plain Chat"))
        .expect("the plain row is painted");
    assert!(painted_forked.contains('🔀'), "{painted_forked}");
    assert!(!painted_plain.contains('🔀'), "{painted_plain}");
}

#[test]
fn test_e2e_history_window_no_fork_marker_for_plain_session() {
    // Sanity check: rendering only plain sessions must not emit 🔀 (guards
    // against an accidental unconditional marker).
    let index = SessionIndex::build(vec![summary(
        "plain-only",
        "Only Plain",
        1_700_000_000,
        2,
        false,
    )]);
    let content = painted_history(&index).join("\n");

    assert!(
        content.contains("Only Plain"),
        "the plain row is painted. Rendered:\n{content}"
    );
    assert!(
        !content.contains('🔀'),
        "the History window must NOT render 🔀 when no session is forked. Rendered:\n{content}"
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
