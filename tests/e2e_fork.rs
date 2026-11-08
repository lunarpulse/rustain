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

use crate::conformance_21_6_decision_queue::support;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::decision_queue::{self, DecisionKind, DecisionSubject, ForkPayload};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::fork_confirm;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::conversation::{
    ChatMessage, Conversation, ForkSource, generate_conversation_id,
};
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::{FocusState, MessageRole};
use rustain::domain::ports::StoragePort;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_message(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: generate_conversation_id(),
        role,
        content: content.to_string(),
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
    }
}

fn make_state_in_chat_focus(width: u16, height: u16) -> TuiState {
    let mut state = TuiState::new(width, height);
    state.focus = FocusState::Chat;
    state
}

fn fork_ready(index: usize) -> TuiState {
    let (mut state, clock) = support::clocked(80, 24);
    decision_queue::raise_fork(&mut state, "c1", index, "message", 0);
    support::paint(&mut state, &fork_facts(), 80, 24);
    support::ready(&mut state, &clock);
    state
}

fn fork_facts() -> rustain::adapters::tui::notification_log::LogFacts {
    rustain::adapters::tui::notification_log::LogFacts {
        active_messages: 5,
        ..support::facts("c1")
    }
}

fn fork_body(preview: &str, index: usize) -> ForkPayload {
    ForkPayload {
        conversation_id: "c1".into(),
        message_index: index,
        target_hash: 0,
        preview: preview.into(),
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
        turns: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: Some("sess-test".to_string()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    }
}

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
    let mut state = fork_ready(2);
    assert_eq!(
        support::route(&mut state, &support::key('n'), &fork_facts())
            .unwrap()
            .into_action(),
        InputAction::ForkCancel
    );
    assert!(
        matches!(state.decisions.take_resolved(), Some(DecisionSubject::Fork(p)) if p.message_index == 2 && p.conversation_id == "c1")
    );
    assert!(!decision_queue::fork_pending(&state, "c1"));
}

#[test]
fn test_e2e_fork_esc_does_not_cancel_the_confirmation() {
    let mut state = fork_ready(2);
    support::route(
        &mut state,
        &support::special(rustain::domain::events::DomainKey::Esc),
        &fork_facts(),
    );
    assert!(decision_queue::fork_pending(&state, "c1"));
    assert!(state.decisions.take_resolved().is_none());
}

#[test]
fn test_e2e_fork_confirm_via_y_key() {
    let mut state = fork_ready(2);
    assert_eq!(
        support::route(&mut state, &support::key('y'), &fork_facts())
            .unwrap()
            .into_action(),
        InputAction::ForkConfirm
    );
    assert!(
        matches!(state.decisions.take_resolved(), Some(DecisionSubject::Fork(p)) if p.message_index == 2 && p.conversation_id == "c1")
    );
    assert!(!decision_queue::fork_pending(&state, "c1"));
}

#[test]
fn test_e2e_fork_overlay_blocks_chat_chars() {
    let mut state = fork_ready(2);
    support::route(&mut state, &support::key('k'), &fork_facts());
    assert!(decision_queue::fork_pending(&state, "c1"));
    assert!(state.decisions.take_resolved().is_none());
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

    let (mut state, clock) = support::clocked(80, 24);
    decision_queue::raise_fork(
        &mut state,
        &source_id,
        2,
        &conv.messages[2].content,
        decision_queue::message_target_fingerprint(&conv.messages[2]),
    );
    let facts = rustain::adapters::tui::notification_log::LogFacts {
        active_messages: conv.messages.len(),
        ..support::facts(&source_id)
    };
    support::paint(&mut state, &facts, 80, 24);
    support::ready(&mut state, &clock);
    assert_eq!(
        support::route(&mut state, &support::key('y'), &facts)
            .unwrap()
            .into_action(),
        InputAction::ForkConfirm
    );
    let Some(DecisionSubject::Fork(payload)) = state.decisions.take_resolved() else {
        panic!("confirmed fork payload");
    };
    assert_eq!(payload.conversation_id, source_id);
    let new_id = rustain::adapters::tui::handlers::transcript_target::handle_fork_confirm(
        &storage,
        &conv,
        payload.message_index,
    )
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
    let mut tabs = TabManager::default();
    let before = tabs.tab_count();
    let tab = tabs.create_tab_with_conversation(forked);
    assert_eq!(tabs.tab_count(), before + 1);
    assert_eq!(tabs.active_tab_id(), tab);
    assert_eq!(tabs.active_tab().conversation.id, new_id);
}

/// 6D-L1-01: a reused index with different content cannot create storage or a tab.
#[tokio::test]
async fn fork_refuses_changed_target_before_storage() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    storage.save_conversation(&conv).await.unwrap();
    let mut tabs = TabManager::default();
    tabs.create_tab_with_conversation(conv.clone());
    let before_tabs = tabs.tab_count();
    let before_conversations = storage.list_conversations().await.unwrap().len();
    let (mut state, clock) = support::clocked(80, 24);
    decision_queue::raise_fork(
        &mut state,
        &conv.id,
        3,
        &conv.messages[3].content,
        decision_queue::message_target_fingerprint(&conv.messages[3]),
    );
    let facts = rustain::adapters::tui::notification_log::LogFacts {
        active_messages: conv.messages.len(),
        ..support::facts(&conv.id)
    };
    support::paint(&mut state, &facts, 80, 24);
    support::ready(&mut state, &clock);
    conv.messages[3].content = "different message".into();
    assert_eq!(
        support::route(&mut state, &support::key('y'), &facts)
            .unwrap()
            .into_action(),
        InputAction::ForkConfirm
    );
    let Some(DecisionSubject::Fork(payload)) = state.decisions.take_resolved() else {
        panic!("fork payload")
    };
    let verified = rustain::adapters::tui::handlers::transcript_target::verify_target(
        &conv,
        payload.message_index,
        payload.target_hash,
    );
    assert_eq!(verified, Err("This decision is no longer pending."));
    if verified.is_ok() {
        let id = rustain::adapters::tui::handlers::transcript_target::handle_fork_confirm(
            &storage,
            &conv,
            payload.message_index,
        )
        .await
        .unwrap();
        tabs.create_tab_with_conversation(storage.load_conversation(&id).await.unwrap().unwrap());
    }
    assert_eq!(tabs.tab_count(), before_tabs);
    assert_eq!(
        storage.list_conversations().await.unwrap().len(),
        before_conversations
    );
}

#[tokio::test]
async fn test_e2e_fork_cancel_preserves_conversation_and_tabs() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    storage.save_conversation(&conv).await.unwrap();
    let mut tabs = TabManager::default();
    tabs.create_tab_with_conversation(conv.clone());
    let before = tabs.tab_count();
    let (mut state, clock) = support::clocked(80, 24);
    decision_queue::raise_fork(
        &mut state,
        &conv.id,
        2,
        &conv.messages[2].content,
        decision_queue::message_target_fingerprint(&conv.messages[2]),
    );
    let facts = rustain::adapters::tui::notification_log::LogFacts {
        active_messages: conv.messages.len(),
        ..support::facts(&conv.id)
    };
    support::paint(&mut state, &facts, 80, 24);
    support::ready(&mut state, &clock);
    assert_eq!(
        support::route(&mut state, &support::key('n'), &facts)
            .unwrap()
            .into_action(),
        InputAction::ForkCancel
    );
    assert!(matches!(
        state.decisions.take_resolved(),
        Some(DecisionSubject::Fork(_))
    ));
    assert_eq!(tabs.tab_count(), before);
    let saved = storage.load_conversation(&conv.id).await.unwrap().unwrap();
    assert_eq!(saved.messages.len(), conv.messages.len());
    for (actual, original) in saved.messages.iter().zip(&conv.messages) {
        assert_eq!(actual.id, original.id);
        assert_eq!(actual.content, original.content);
    }
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
    let mut tm = TabManager::default();
    let initial_count = tm.tab_count();

    let forked_conv = Conversation {
        id: generate_conversation_id(),
        title: "Fork of Test".to_string(),
        messages: vec![make_message(MessageRole::User, "Hello")],
        turns: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000000,
        last_response_at: None,
        session_id: Some(generate_conversation_id()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: Some(ForkSource {
            conversation_id: "original-id".to_string(),
            message_index: 0,
            checkpoint_id: CheckpointId(0),
        }),
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
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
    use rustain::adapters::tui::state::TabRenderState;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::StreamingState;
    use std::collections::BTreeMap;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut tab_render_state = TabRenderState::default();

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
                &mut tab_render_state,
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
    use rustain::adapters::tui::state::TabRenderState;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::StreamingState;
    use std::collections::BTreeMap;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut tab_render_state = TabRenderState::default();

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
                &mut tab_render_state,
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
        turns: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
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
fn test_e2e_fork_widget_renders_frame() {
    let (mut state, _) = support::clocked(80, 24);
    decision_queue::raise_fork(&mut state, "c1", 0, "Hello world", 0);
    let text = support::screen(&support::paint(&mut state, &fork_facts(), 80, 24));
    assert!(text.contains("Hello world"));
    assert!(text.contains("Fork"));
}

#[test]
fn test_e2e_fork_widget_shows_correct_message_number() {
    let theme = Theme::dark();
    let lines = fork_confirm::lines(&fork_body("Hello", 4), &theme);
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
    let (mut state, _) = support::clocked(80, 24);
    decision_queue::raise_fork(&mut state, "c1", 0, "msg", 0);
    let text = support::screen(&support::paint(&mut state, &fork_facts(), 80, 24));
    assert!(text.contains("Fork"));
    assert!(text.contains("Cancel"));
}

#[test]
fn test_e2e_fork_widget_truncates_long_utf8_safely() {
    // Multi-byte UTF-8 stress test (AC3: char-boundary-safe truncation)
    let utf8_msg = "日本語のテスト".repeat(20); // 140 multi-byte chars
    let theme = Theme::dark();
    // Must not panic
    let lines = fork_confirm::lines(&fork_body(&utf8_msg, 0), &theme);
    assert!(!lines.is_empty());
}

// ── DF-018: Fork overlay vs permission overlay ────────────────────────────────

#[test]
fn test_e2e_fork_blocked_during_permission_overlay() {
    use rustain::adapters::tui::decision_queue::{
        self, DecisionSubject, PermissionInput, PermissionPayload, Responder,
    };
    use rustain::adapters::tui::keymap::Host;
    use rustain::adapters::tui::notification_log::LogFacts;
    use rustain::adapters::tui::notifications::NotificationStore;
    use rustain::adapters::tui::presentation::{self, Shell};
    use rustain::domain::clock::MockClock;
    use rustain::domain::events::DomainInputEvent;
    use rustain::domain::models::tool_call::RequestId;
    use rustain::domain::models::{ApprovalSource, NotificationsConfig, ToolRisk};
    use std::sync::Arc;
    use std::time::Instant;

    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = TuiState::new(80, 24);
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    decision_queue::raise_permission(
        &mut state,
        PermissionPayload::new(
            RequestId::new(),
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            ToolRisk::Elevated,
            PermissionInput::Ready(Arc::new(serde_json::json!({"command": "true"}))),
        ),
        Responder::Local,
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    let layout = rustain::adapters::tui::layout::compute_layout(
        ratatui::layout::Rect::new(0, 0, 80, 24),
        &state.theme,
        "",
        1,
        false,
        rustain::domain::models::visual::DensityMode::Focus,
    )
    .unwrap();
    let mut painted = false;
    terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            painted = presentation::compose(
                frame,
                &state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    assert!(painted);
    decision_queue::note_painted(&mut state);
    clock.advance(decision_queue::INERT_AFTER_PAINT);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('f'));
    assert_ne!(action, InputAction::ForkAtMessage);
    assert_eq!(action, InputAction::Consumed);
    assert!(matches!(
        &decision_queue::current(&state, &LogFacts::default()).unwrap().subject,
        DecisionSubject::Permission(payload) if payload.feedback.is_some()
    ));
}

#[test]
fn test_e2e_fork_blocked_during_question_overlay() {
    // DF-018: 'f' key is consumed by the Question frame (it types into its answer field)
    use rustain::adapters::tui::decision_queue::{self, DecisionSubject};
    use rustain::domain::events::DomainInputEvent;
    let clock = std::sync::Arc::new(rustain::domain::clock::MockClock::new(
        std::time::Instant::now(),
    ));
    let mut state = TuiState::new(80, 24);
    state.notifications = rustain::adapters::tui::notifications::NotificationStore::from_config(
        &rustain::domain::models::NotificationsConfig::default(),
        clock.clone(),
    );
    state.focus = FocusState::Chat;
    let (tx, _rx) = tokio::sync::oneshot::channel();
    decision_queue::raise_question(
        &mut state,
        "c1",
        "tu-1".to_string(),
        "What?".to_string(),
        tx,
    );
    // First paint starts the inert clock; the window passes before the user types.
    decision_queue::note_painted(&mut state);
    clock.advance(decision_queue::INERT_AFTER_PAINT);

    let event = DomainInputEvent::KeyPress('f');
    let action = handle_input(&mut state, &event);

    assert_ne!(
        action,
        InputAction::ForkAtMessage,
        "'f' in Question overlay must NOT produce ForkAtMessage (DF-018)"
    );
    assert_eq!(action, InputAction::Consumed);
    let typed = state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            DecisionSubject::Question(payload) => Some(payload.field.as_str().to_owned()),
            _ => None,
        });
    assert_eq!(
        typed.as_deref(),
        Some("f"),
        "the key went to the frame field"
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

/// A resolved decision can only hand its payload to the effect arm once.
#[test]
fn test_e2e_fork_decision_payload_taken_once() {
    let mut state = fork_ready(3);
    support::route(&mut state, &support::key('y'), &fork_facts());
    assert!(
        matches!(state.decisions.take_resolved(), Some(DecisionSubject::Fork(p)) if p.message_index == 3)
    );
    assert!(state.decisions.take_resolved().is_none());
    assert!(!state.decisions.holds(DecisionKind::Fork));
}

#[test]
fn test_e2e_fork_overlay_blocks_enter_submit() {
    let mut state = fork_ready(1);
    let result = support::route(
        &mut state,
        &support::special(rustain::domain::events::DomainKey::Enter),
        &fork_facts(),
    );
    assert!(result.is_some());
    assert!(decision_queue::fork_pending(&state, "c1"));
    assert!(state.decisions.take_resolved().is_none());
}
