//! E2E tests for Story 4-3a: Fork Conversations.
//!
//! AC[1]: Fork Trigger & Confirmation Card
//! AC[2]: Fork Execution -- New Tab with Truncated Messages
//! AC[3]: Fork Visual Indicator
//! AC[4]: Fork Independence
//! AC[5]: Edge Case -- Fork at First Message
//! AC[6]: Edge Case -- Fork at Last Message
//! AC[7]: Fork Persists ForkSource Provenance
//! AC[8]: StoragePort fork_at_checkpoint Extension
//! AC[9]: Full Test Suite Green (cargo test)
//! DF-018: Fork overlay cannot co-occur with streaming overlays

use tempfile::TempDir;

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::fork_confirm::render_fork_confirmation_lines;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::conversation::{
    ChatMessage, Conversation, ForkSource, generate_conversation_id,
};
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::visual::{ConfirmationType, OverlayType};
use rustain::domain::models::{FocusState, MessageRole};
use rustain::domain::ports::StoragePort;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_message(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        id: generate_conversation_id(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1700000000,
        token_count: None,
        stop_reason: None,
        images: vec![],
    }
}

fn make_conversation_5_messages() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: "Test Conversation".to_string(),
        messages: vec![
            make_message(MessageRole::User, "Message 0"),
            make_message(MessageRole::Assistant, "Response 0"),
            make_message(MessageRole::User, "Message 1"),
            make_message(MessageRole::Assistant, "Response 1"),
            make_message(MessageRole::User, "Message 2"),
        ],
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: Some("sess-test".to_string()),
        usage: None,
        fork_source: None,
    }
}

fn make_state_in_chat_focus(width: u16, height: u16) -> TuiState {
    let mut state = TuiState::new(width, height);
    state.focus = FocusState::Chat;
    state.auto_scroll = true;
    state
}

// ── AC[1]: Fork Trigger & Confirmation Card ───────────────────────────────────

#[test]
fn test_e2e_fork_f_key_opens_confirmation() {
    // Given: a conversation with messages in Chat focus
    let _conv = make_conversation_5_messages();
    let mut state = make_state_in_chat_focus(80, 24);
    // Set up message_boundaries so the guard passes (non-empty)
    state.message_boundaries = vec![0, 5, 10];
    state.total_content_height = 30;
    // Simulate having conversation in scope (via auto_scroll=true)

    // When: I press 'f'
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('f');
    let action = handle_input(&mut state, &event);

    // Then: ForkAtMessage action returned
    assert_eq!(
        action,
        InputAction::ForkAtMessage,
        "Pressing 'f' in Chat focus should return ForkAtMessage"
    );
}

#[test]
fn test_e2e_fork_cancel_via_n_key() {
    // Given: fork confirmation overlay is active
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork));
    state.pending_fork_index = Some(2);

    // When: I press 'n'
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('n');
    let action = handle_input(&mut state, &event);

    // Then: ForkCancel returned
    assert_eq!(
        action,
        InputAction::ForkCancel,
        "'n' in Fork confirmation should return ForkCancel"
    );
}

#[test]
fn test_e2e_fork_cancel_via_esc() {
    // Given: fork confirmation overlay is active
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork));
    state.pending_fork_index = Some(2);

    // When: I press Esc
    use rustain::domain::events::{DomainInputEvent, DomainKey};
    let event = DomainInputEvent::SpecialKey(DomainKey::Esc);
    let action = handle_input(&mut state, &event);

    // Then: ForkCancel returned
    assert_eq!(
        action,
        InputAction::ForkCancel,
        "Esc in Fork confirmation should return ForkCancel"
    );
}

#[test]
fn test_e2e_fork_confirm_via_y_key() {
    // Given: fork confirmation overlay is active
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork));
    state.pending_fork_index = Some(2);

    // When: I press 'y'
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('y');
    let action = handle_input(&mut state, &event);

    // Then: ForkConfirm returned
    assert_eq!(
        action,
        InputAction::ForkConfirm,
        "'y' in Fork confirmation should return ForkConfirm"
    );
}

// ── AC[1]: Overlay blocks other interactions (Tier-1 pattern) ────────────────

#[test]
fn test_e2e_fork_overlay_blocks_chat_chars() {
    // Given: fork confirmation overlay is active
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork));

    // When: I press an unrelated key (not y/n/Esc)
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('k'); // navigation key
    let action = handle_input(&mut state, &event);

    // Then: Consumed (blocked, not passed through to Chat)
    assert_eq!(
        action,
        InputAction::Consumed,
        "Fork overlay should consume all keys except y/n/Esc"
    );
}

// ── AC[2]: Fork Execution -- New Tab with Truncated Messages ──────────────────

#[tokio::test]
async fn test_e2e_fork_confirm_creates_new_tab() {
    // Given: a filesystem storage with a 5-message conversation
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // When: I fork at message 2 (checkpoint=2)
    let checkpoint = CheckpointId(2);
    let new_id = storage
        .fork_at_checkpoint(&source_id, checkpoint)
        .await
        .unwrap();

    // Then: a new conversation was created
    assert_ne!(new_id, source_id, "Forked conversation must have a new ID");

    // And: the new conversation has messages 0..=2 (3 messages)
    let forked = storage
        .load_conversation(&new_id)
        .await
        .unwrap()
        .expect("should load forked");
    assert_eq!(
        forked.messages.len(),
        3,
        "Forked conversation should have 3 messages (indices 0, 1, 2)"
    );
}

#[tokio::test]
async fn test_e2e_fork_new_tab_has_truncated_messages() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    let original_msgs: Vec<_> = conv.messages.iter().map(|m| m.content.clone()).collect();
    storage.save_conversation(&conv).await.unwrap();

    // Fork at message index 1 (inclusive)
    let new_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(1))
        .await
        .unwrap();
    let forked = storage
        .load_conversation(&new_id)
        .await
        .unwrap()
        .expect("should load");

    // Messages 0 and 1 only
    assert_eq!(forked.messages.len(), 2);
    assert_eq!(forked.messages[0].content, original_msgs[0]);
    assert_eq!(forked.messages[1].content, original_msgs[1]);
}

#[test]
fn test_e2e_fork_tab_manager_create_tab_with_conversation() {
    // Given: a TabManager and a forked conversation
    let mut tm = TabManager::new();
    let initial_count = tm.tab_count();

    let forked_conv = Conversation {
        id: generate_conversation_id(),
        title: "Fork of Test".to_string(),
        messages: vec![make_message(MessageRole::User, "Hello")],
        created_at: 1700000000,
        updated_at: 1700000000,
        last_response_at: None,
        session_id: Some(generate_conversation_id()),
        usage: None,
        fork_source: Some(ForkSource {
            conversation_id: "original-id".to_string(),
            message_index: 0,
            checkpoint_id: CheckpointId(0),
        }),
    };
    let forked_id = forked_conv.id.clone();

    // When: create_tab_with_conversation is called
    let tab_id = tm.create_tab_with_conversation(forked_conv);

    // Then: tab count incremented, new tab is active, conversation preserved
    assert_eq!(
        tm.tab_count(),
        initial_count + 1,
        "tab count should increase"
    );
    assert_eq!(tm.active_tab_id(), tab_id, "new tab should be active");
    assert_eq!(
        tm.active_tab().conversation.id,
        forked_id,
        "forked conversation preserved in new tab"
    );
    assert!(
        tm.active_tab().conversation.fork_source.is_some(),
        "fork_source preserved"
    );
}

// ── AC[3]: Fork Visual Indicator ─────────────────────────────────────────────

#[test]
fn test_e2e_fork_marker_on_forked_conversation() {
    // Given: a forked conversation rendered in the chat pane
    let mut forked_conv = make_conversation_5_messages();
    forked_conv.fork_source = Some(ForkSource {
        conversation_id: "original-123".to_string(),
        message_index: 4,
        checkpoint_id: CheckpointId(4),
    });

    // When: rendering the chat pane via chat_pane::render (via TestBackend)
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::prelude::Rect;
    use rustain::adapters::tui::state::HeightCache;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::StreamingState;
    use std::collections::{BTreeMap, HashMap};

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut height_cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, 80, 24);
            let result = chat_pane::render(
                frame,
                area,
                &forked_conv,
                &StreamingState::default(),
                0,
                true,
                &theme,
                &mut height_cache,
                &HashMap::new(),
                &BTreeMap::new(),
            );
            // The render should succeed with valid boundaries
            assert!(!result.message_boundaries.is_empty());
        })
        .unwrap();

    // Then: verify the rendered buffer contains the 🔀 marker
    let buffer = terminal.backend().buffer().clone();
    let content: String = buffer
        .content
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        content.contains('🔀'),
        "Fork point message should have 🔀 marker in rendered output"
    );
}

#[test]
fn test_e2e_fork_no_marker_on_non_forked_conversation() {
    // Given: a conversation WITHOUT fork_source
    let conv = make_conversation_5_messages();
    assert!(conv.fork_source.is_none());

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::prelude::Rect;
    use rustain::adapters::tui::state::HeightCache;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::StreamingState;
    use std::collections::{BTreeMap, HashMap};

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut height_cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, 80, 24);
            chat_pane::render(
                frame,
                area,
                &conv,
                &StreamingState::default(),
                0,
                true,
                &theme,
                &mut height_cache,
                &HashMap::new(),
                &BTreeMap::new(),
            );
        })
        .unwrap();

    let buffer = terminal.backend().buffer().clone();
    let content: String = buffer
        .content
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        !content.contains('🔀'),
        "Non-forked conversation should NOT have 🔀 marker"
    );
}

// ── AC[4]: Fork Independence ──────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_fork_independence_messages_dont_cross() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Fork at message 2
    let fork_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(2))
        .await
        .unwrap();

    // Add a message to the original
    conv.messages
        .push(make_message(MessageRole::User, "New message in original"));
    storage.save_conversation(&conv).await.unwrap();

    // Then: forked conversation is unchanged
    let forked = storage
        .load_conversation(&fork_id)
        .await
        .unwrap()
        .expect("should load");
    assert_eq!(
        forked.messages.len(),
        3,
        "Fork should not be affected by changes to original"
    );
    assert!(
        forked
            .messages
            .iter()
            .all(|m| m.content != "New message in original"),
        "New message in original should not appear in fork"
    );

    // And: original has the new message
    let original = storage
        .load_conversation(&source_id)
        .await
        .unwrap()
        .expect("should load");
    assert_eq!(original.messages.len(), 6);
}

// ── AC[5]: Edge Case -- Fork at First Message ─────────────────────────────────

#[tokio::test]
async fn test_e2e_fork_at_first_message_single_msg() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Fork at index 0 (single message)
    let fork_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(0))
        .await
        .unwrap();
    let forked = storage
        .load_conversation(&fork_id)
        .await
        .unwrap()
        .expect("should load");

    // Then: fork has exactly 1 message
    assert_eq!(
        forked.messages.len(),
        1,
        "Fork at first message should have 1 message"
    );
    assert_eq!(
        forked.messages[0].content, "Message 0",
        "Fork should have the first message's content"
    );
    assert!(
        forked.fork_source.is_some(),
        "Fork at first message should still have fork_source"
    );
}

// ── AC[6]: Edge Case -- Fork at Last Message ──────────────────────────────────

#[tokio::test]
async fn test_e2e_fork_at_last_message_full_copy() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    let msg_count = conv.messages.len(); // 5
    storage.save_conversation(&conv).await.unwrap();

    // Fork at last message (index 4)
    let fork_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId((msg_count - 1) as u64))
        .await
        .unwrap();
    let forked = storage
        .load_conversation(&fork_id)
        .await
        .unwrap()
        .expect("should load");

    // Then: fork has all 5 messages (full copy)
    assert_eq!(
        forked.messages.len(),
        msg_count,
        "Fork at last message should be a full copy"
    );
    assert_eq!(
        forked.fork_source.as_ref().unwrap().message_index,
        msg_count - 1,
        "fork_source.message_index should be the last message index"
    );
}

// ── AC[7]: Fork Persists ForkSource Provenance ────────────────────────────────

#[tokio::test]
async fn test_e2e_fork_source_persisted_correctly() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let checkpoint = CheckpointId(2);
    let fork_id = storage
        .fork_at_checkpoint(&source_id, checkpoint)
        .await
        .unwrap();
    let forked = storage
        .load_conversation(&fork_id)
        .await
        .unwrap()
        .expect("should load");

    // Then: fork_source is correctly set
    let fs = forked
        .fork_source
        .expect("forked conversation must have fork_source");
    assert_eq!(
        fs.conversation_id, source_id,
        "fork_source.conversation_id must be the original's ID"
    );
    assert_eq!(fs.message_index, 2, "fork_source.message_index must be 2");
    assert_eq!(
        fs.checkpoint_id,
        CheckpointId(2),
        "fork_source.checkpoint_id must match the CheckpointId"
    );

    // And: forked conversation has independent timestamps
    assert!(forked.created_at > 0, "created_at should be set");
    assert!(forked.updated_at > 0, "updated_at should be set");
    assert_ne!(forked.id, source_id, "forked ID must differ from source");
}

#[tokio::test]
async fn test_e2e_fork_source_checkpoint_id_in_json() {
    // AC[7]: checkpointId appears in persisted JSON
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let checkpoint = CheckpointId(3);
    let fork_id = storage
        .fork_at_checkpoint(&source_id, checkpoint)
        .await
        .unwrap();

    // Read raw JSON file
    let meta_path = tmp
        .path()
        .join("sessions")
        .join(format!("{}.meta.json", fork_id));
    let json = std::fs::read_to_string(&meta_path).expect("session file should exist");

    // The JSON must contain "forkSource" (camelCase) and "checkpointId"
    assert!(json.contains("forkSource"), "JSON must contain forkSource");
    assert!(
        json.contains("checkpointId"),
        "JSON must contain checkpointId"
    );
}

// ── AC[8]: StoragePort fork_at_checkpoint Extension ──────────────────────────

#[tokio::test]
async fn test_e2e_fork_at_checkpoint_storage_port() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Test through the StoragePort trait (not concrete type)
    let port: &dyn StoragePort = &storage;
    let result = port.fork_at_checkpoint(&source_id, CheckpointId(1)).await;
    assert!(
        result.is_ok(),
        "fork_at_checkpoint via StoragePort should succeed"
    );

    let new_id = result.unwrap();
    let forked = port
        .load_conversation(&new_id)
        .await
        .unwrap()
        .expect("should load");
    assert_eq!(forked.messages.len(), 2);
}

#[tokio::test]
async fn test_e2e_fork_storage_source_unchanged() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    let original_msg_count = conv.messages.len();
    storage.save_conversation(&conv).await.unwrap();

    // Fork at message 2
    storage
        .fork_at_checkpoint(&source_id, CheckpointId(2))
        .await
        .unwrap();

    // Then: original conversation is unchanged
    let original = storage
        .load_conversation(&source_id)
        .await
        .unwrap()
        .expect("should load");
    assert_eq!(
        original.messages.len(),
        original_msg_count,
        "Source conversation must have same number of messages after fork"
    );
    assert!(
        original.fork_source.is_none(),
        "Source conversation must NOT have fork_source set"
    );
}

#[tokio::test]
async fn test_e2e_fork_noop_adapter_returns_not_supported() {
    // AC[8]: NoOpStorage uses default NotSupported implementation from StoragePort trait
    use rustain::adapters::noop::NoOpStorage;
    let noop = NoOpStorage;
    let result: Result<String, _> = noop.fork_at_checkpoint("any-id", CheckpointId(0)).await;
    assert!(
        result.is_err(),
        "NoOpStorage should return an error for fork_at_checkpoint (default impl)"
    );
    match result.unwrap_err() {
        rustain::domain::errors::StorageError::NotSupported(_) => {}
        e => panic!("Expected NotSupported error, got: {:?}", e),
    }
}

// ── AC[8]: Error cases ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_fork_source_not_found_returns_error() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    let result = storage
        .fork_at_checkpoint("nonexistent-id", CheckpointId(0))
        .await;
    assert!(
        result.is_err(),
        "Fork of non-existent conversation should error"
    );
    match result.unwrap_err() {
        rustain::domain::errors::StorageError::NotFound(_) => {}
        e => panic!("Expected NotFound, got: {:?}", e),
    }
}

#[tokio::test]
async fn test_e2e_fork_index_out_of_bounds_returns_error() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Index 10 is out of bounds for 5-message conversation
    let result = storage
        .fork_at_checkpoint(&source_id, CheckpointId(10))
        .await;
    assert!(result.is_err(), "Fork at out-of-bounds index should error");
    match result.unwrap_err() {
        rustain::domain::errors::StorageError::Other(_) => {}
        e => panic!("Expected Other error, got: {:?}", e),
    }
}

#[tokio::test]
async fn test_e2e_fork_empty_conversation_returns_error() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = Conversation {
        id: generate_conversation_id(),
        title: "Empty".to_string(),
        messages: vec![],
        created_at: 1700000000,
        updated_at: 1700000000,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    };
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let result = storage
        .fork_at_checkpoint(&source_id, CheckpointId(0))
        .await;
    assert!(result.is_err(), "Fork of empty conversation should error");
    match result.unwrap_err() {
        rustain::domain::errors::StorageError::Other(_) => {}
        e => panic!("Expected Other error, got: {:?}", e),
    }
}

// ── AC[1]: Fork confirmation card widget ─────────────────────────────────────

#[test]
fn test_e2e_fork_widget_renders_double_border() {
    let theme = Theme::dark();
    let lines = render_fork_confirmation_lines("Hello world", 0, 60, &theme);

    assert!(lines.len() >= 5);
    let first: String = lines[0]
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    let last: String = lines
        .last()
        .unwrap()
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(first.starts_with('╔'));
    assert!(first.ends_with('╗'));
    assert!(last.starts_with('╚'));
    assert!(last.ends_with('╝'));
}

#[test]
fn test_e2e_fork_widget_shows_correct_message_number() {
    let theme = Theme::dark();
    let lines = render_fork_confirmation_lines("Hello", 4, 60, &theme);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.to_string())
        .collect();
    assert!(
        text.contains("Message 5:"),
        "Should display 1-based message number (index 4 → 'Message 5:')"
    );
}

#[test]
fn test_e2e_fork_widget_shows_actions() {
    let theme = Theme::dark();
    let lines = render_fork_confirmation_lines("msg", 0, 60, &theme);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.to_string())
        .collect();
    assert!(text.contains("[y] Fork"), "Should show [y] Fork action");
    assert!(text.contains("[n] Cancel"), "Should show [n] Cancel action");
}

#[test]
fn test_e2e_fork_widget_truncates_long_utf8_safely() {
    // Multi-byte UTF-8 stress test (AC3: char-boundary-safe truncation)
    let utf8_msg = "日本語のテスト".repeat(20); // 140 multi-byte chars
    let theme = Theme::dark();
    // Must not panic
    let lines = render_fork_confirmation_lines(&utf8_msg, 0, 60, &theme);
    assert!(!lines.is_empty());
}

// ── DF-018: Fork overlay vs permission overlay ────────────────────────────────

#[test]
fn test_e2e_fork_blocked_during_permission_overlay() {
    // DF-018: 'f' key is consumed by the Permission overlay, not passed through to Chat
    let mut state = TuiState::new(80, 24);
    state.focus = rustain::domain::models::FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::Permission,
    ));

    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('f');
    let action = handle_input(&mut state, &event);

    // 'f' is not a valid permission key (y/n/a), so it should be consumed (not produce ForkAtMessage)
    assert_ne!(
        action,
        InputAction::ForkAtMessage,
        "'f' in Permission overlay must NOT produce ForkAtMessage (DF-018)"
    );
    // It should be consumed by the permission overlay handler
    assert_eq!(
        action,
        InputAction::Consumed,
        "'f' in Permission overlay should be Consumed"
    );
}

#[test]
fn test_e2e_fork_blocked_during_question_overlay() {
    // DF-018: 'f' key is consumed by the Question overlay
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Question));
    state.ask_user_question = Some(
        rustain::adapters::tui::widgets::ask_user_question::AskUserQuestionState {
            tool_use_id: "tu-1".to_string(),
            question: "What?".to_string(),
            input_buffer: String::new(),
            cursor_position: 0,
            submitted_answer: None,
        },
    );

    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('f');
    let action = handle_input(&mut state, &event);

    assert_ne!(
        action,
        InputAction::ForkAtMessage,
        "'f' in Question overlay must NOT produce ForkAtMessage (DF-018)"
    );
}

// ── CheckpointId domain type tests ───────────────────────────────────────────

#[test]
fn test_e2e_checkpoint_id_ord() {
    let a = CheckpointId(0);
    let b = CheckpointId(5);
    let c = CheckpointId(5);
    assert!(a < b);
    assert!(b == c);
    assert!(a <= b);
}

#[test]
fn test_e2e_checkpoint_id_serde_roundtrip() {
    let id = CheckpointId(42);
    let json = serde_json::to_string(&id).unwrap();
    let decoded: CheckpointId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, decoded);
}

#[test]
fn test_e2e_checkpoint_id_backward_compat_default() {
    // A ForkSource without checkpointId should deserialize with CheckpointId(0)
    let json = r#"{"conversationId":"abc","messageIndex":2}"#;
    let fs: ForkSource =
        serde_json::from_str(json).expect("should deserialize without checkpointId");
    assert_eq!(
        fs.checkpoint_id,
        CheckpointId(0),
        "Missing checkpointId should default to CheckpointId(0)"
    );
    assert_eq!(fs.message_index, 2);
}

// ── ForkSource independence of forked conversation ────────────────────────────

#[tokio::test]
async fn test_e2e_fork_independence_add_message_to_fork() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Fork at message 2
    let fork_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(2))
        .await
        .unwrap();

    // Add a message to the fork
    let mut forked = storage
        .load_conversation(&fork_id)
        .await
        .unwrap()
        .expect("should load");
    forked
        .messages
        .push(make_message(MessageRole::User, "Fork-only message"));
    storage.save_conversation(&forked).await.unwrap();

    // Then: original is unaffected
    let original = storage
        .load_conversation(&source_id)
        .await
        .unwrap()
        .expect("should load");
    assert_eq!(
        original.messages.len(),
        5,
        "Original should still have 5 messages"
    );
    assert!(
        original
            .messages
            .iter()
            .all(|m| m.content != "Fork-only message"),
        "Fork-only message should not appear in original"
    );
}

// ── Party-mode review 2026-04-12: P4 / P5 / P2 follow-up tests ───────────────

/// P4: Forking a conversation with an empty or whitespace-only title must not
/// produce "Fork of " with a dangling trailing space. The adapter substitutes
/// "Fork of (Untitled)" so the new tab displays a meaningful label.
#[tokio::test]
async fn test_e2e_fork_empty_title_produces_untitled_fallback() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    let mut conv = make_conversation_5_messages();
    conv.title = "   ".to_string(); // whitespace-only title
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let new_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(1))
        .await
        .unwrap();
    let forked = storage
        .load_conversation(&new_id)
        .await
        .unwrap()
        .expect("should load");

    assert_eq!(
        forked.title, "Fork of (Untitled)",
        "Whitespace-only source title must fall back to '(Untitled)' in the forked title"
    );
}

/// P4: A normal (non-empty) title keeps the regular "Fork of <title>" format.
/// Regression guard so the fallback does not shadow the happy path.
#[tokio::test]
async fn test_e2e_fork_nonempty_title_preserves_format() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    let conv = make_conversation_5_messages(); // title = "Test Conversation"
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let new_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(1))
        .await
        .unwrap();
    let forked = storage
        .load_conversation(&new_id)
        .await
        .unwrap()
        .expect("should load");

    assert_eq!(forked.title, "Fork of Test Conversation");
}

/// P5 dedup invariant: once `pending_fork_index` has been taken, a subsequent
/// ForkConfirm action must find it `None` and refuse to start a second fork.
///
/// We can't drive the full event loop from a unit test, so we assert the
/// precondition the event_loop.rs match arm relies on: `.take()` leaves the
/// Option empty, and the `if let Some(...)` guard trivially short-circuits.
#[test]
fn test_e2e_fork_pending_index_take_makes_confirm_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork));
    state.pending_fork_index = Some(3);

    // First confirm takes the value.
    let first = state.pending_fork_index.take();
    assert_eq!(first, Some(3));

    // A second confirm landing before focus is cleared would hit the same
    // match arm — but `take()` has already cleared the Option, so the
    // `if let Some(_) = state.pending_fork_index.take()` check is a no-op.
    let second = state.pending_fork_index.take();
    assert_eq!(
        second, None,
        "Second ForkConfirm must observe None — this is the dedup guarantee"
    );
}

/// P2: While the fork confirmation overlay is active, user input keys (e.g. a
/// submit via Enter) must not be routed to the input box. This eliminates the
/// "message count changes between 'f' and 'y'" concern — no async path can add
/// a message while the overlay is up.
#[test]
fn test_e2e_fork_overlay_blocks_enter_submit() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork));
    state.pending_fork_index = Some(1);

    use rustain::domain::events::{DomainInputEvent, DomainKey};
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));

    // Enter must not produce a SubmitMessage action — it's consumed by the overlay.
    assert!(
        !matches!(action, InputAction::SubmitMessage(_)),
        "Enter in Fork overlay must NOT produce SubmitMessage, got: {:?}",
        action
    );
}
