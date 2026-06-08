//! E2E tests for Story 4.1: Multi-Tab Architecture & Tab Bar.
//! Tests TabManager, tab bar rendering, HeightCache ID-keying, and PersistedConversation.

use rustain::domain::models::StreamingState;
use rustain::domain::models::conversation::{
    Conversation, PersistedConversation, generate_conversation_id,
};
use rustain::domain::models::tab::{TabManager, TabState};

// ── TabManager unit tests ─────────────────────────────────────────────────────

#[test]
fn test_tab_manager_new_has_one_tab() {
    let tm = TabManager::default();
    assert_eq!(tm.tab_count(), 1);
    assert_eq!(tm.active_tab_index(), 0);
}

#[test]
fn test_tab_manager_create_tab_increments_count() {
    let mut tm = TabManager::default();
    let id = tm.create_tab();
    assert_eq!(tm.tab_count(), 2);
    assert_eq!(tm.active_tab_id(), id);
    assert_eq!(tm.active_tab_index(), 1);
}

#[test]
fn test_tab_manager_create_multiple_tabs() {
    let mut tm = TabManager::default();
    let _id1 = tm.create_tab();
    let _id2 = tm.create_tab();
    assert_eq!(tm.tab_count(), 3);
    assert_eq!(tm.active_tab_index(), 2);
}

#[test]
fn test_tab_manager_close_non_last_tab() {
    let mut tm = TabManager::default();
    let id0 = tm.active_tab_id();
    let _id1 = tm.create_tab();
    let _id2 = tm.create_tab();
    // Close the first tab (index 0)
    let conv = tm.close_tab(id0);
    assert!(conv.is_some());
    assert_eq!(tm.tab_count(), 2);
}

#[test]
fn test_tab_manager_close_last_tab_creates_new() {
    let mut tm = TabManager::default();
    let only_id = tm.active_tab_id();
    let conv = tm.close_tab(only_id);
    assert!(conv.is_some());
    assert_eq!(tm.tab_count(), 1);
    // New tab has a different ID
    assert_ne!(tm.active_tab_id(), only_id);
}

#[test]
fn test_tab_manager_close_unknown_id_returns_none() {
    let mut tm = TabManager::default();
    let result = tm.close_tab(9999);
    assert!(result.is_none());
    assert_eq!(tm.tab_count(), 1);
}

#[test]
fn test_tab_manager_switch_to_next_wraps() {
    let mut tm = TabManager::default();
    tm.create_tab();
    tm.create_tab();
    // active = 2 (last)
    assert_eq!(tm.active_tab_index(), 2);
    tm.switch_to_next();
    assert_eq!(tm.active_tab_index(), 0); // wrapped to first
    tm.switch_to_next();
    assert_eq!(tm.active_tab_index(), 1);
}

#[test]
fn test_tab_manager_switch_to_prev_wraps() {
    let mut tm = TabManager::default();
    tm.create_tab();
    // active = 1
    assert_eq!(tm.active_tab_index(), 1);
    tm.switch_to_prev();
    assert_eq!(tm.active_tab_index(), 0);
    tm.switch_to_prev();
    assert_eq!(tm.active_tab_index(), 1); // wrapped
}

#[test]
fn test_tab_manager_switch_single_tab_is_noop() {
    let mut tm = TabManager::default();
    let orig = tm.active_tab_index();
    tm.switch_to_next();
    assert_eq!(tm.active_tab_index(), orig);
    tm.switch_to_prev();
    assert_eq!(tm.active_tab_index(), orig);
}

#[test]
fn test_tab_manager_switch_to_index_1based() {
    let mut tm = TabManager::default();
    tm.create_tab();
    tm.create_tab();
    // tabs: [0, 1, 2], active = 2
    tm.switch_to_index(1);
    assert_eq!(tm.active_tab_index(), 0);
    tm.switch_to_index(2);
    assert_eq!(tm.active_tab_index(), 1);
    tm.switch_to_index(3);
    assert_eq!(tm.active_tab_index(), 2);
}

#[test]
fn test_tab_manager_switch_to_index_zero_is_noop() {
    let mut tm = TabManager::default();
    let orig = tm.active_tab_index();
    tm.switch_to_index(0);
    assert_eq!(tm.active_tab_index(), orig);
}

#[test]
fn test_tab_manager_switch_to_index_out_of_range_is_noop() {
    let mut tm = TabManager::default();
    let orig = tm.active_tab_index();
    tm.switch_to_index(5);
    assert_eq!(tm.active_tab_index(), orig);
}

#[test]
fn test_tab_manager_find_by_conversation_id() {
    let tm = TabManager::default();
    let conv_id = tm.active_tab().conversation.id.clone();
    assert!(tm.find_by_conversation(&conv_id).is_some());
    assert!(tm.find_by_conversation("nonexistent").is_none());
}

#[test]
fn test_tab_manager_find_by_conversation_mut() {
    let mut tm = TabManager::default();
    let conv_id = tm.active_tab().conversation.id.clone();
    {
        let tab = tm.find_by_conversation_mut(&conv_id).unwrap();
        tab.conversation.title = "Updated".to_string();
    }
    assert_eq!(tm.active_tab().conversation.title, "Updated");
}

#[test]
fn test_tab_state_reset_display_state() {
    let mut ts = TabState::new(0);
    ts.view_state.scroll_offset = 42;
    ts.view_state.mode = rustain::domain::models::AnchorMode::Reading;
    ts.block_boundaries = vec![1, 2, 3];
    ts.message_boundaries = vec![0];
    ts.total_content_height = 100;
    ts.pending_anchor = Some(2);

    ts.reset_display_state();

    assert_eq!(ts.view_state.scroll_offset, 0);
    assert!(matches!(
        ts.view_state.mode,
        rustain::domain::models::AnchorMode::Following
    ));
    assert!(ts.block_boundaries.is_empty());
    assert!(ts.message_boundaries.is_empty());
    assert_eq!(ts.total_content_height, 0);
    assert!(ts.pending_anchor.is_none());
}

#[test]
fn test_tab_state_new_has_fresh_conversation() {
    let ts = TabState::new(42);
    assert_eq!(ts.id, 42);
    assert!(ts.conversation.messages.is_empty());
    assert!(ts.conversation.title.is_empty());
    assert!(!ts.conversation.id.is_empty());
    assert!(matches!(
        ts.view_state.mode,
        rustain::domain::models::AnchorMode::Following
    ));
    assert_eq!(ts.view_state.scroll_offset, 0);
}

#[test]
fn test_tab_manager_with_conversation() {
    let conv = make_test_conversation();
    let conv_id = conv.id.clone();
    let tm = TabManager::with_conversation(conv, tokio_util::sync::CancellationToken::new());
    assert_eq!(tm.tab_count(), 1);
    assert_eq!(tm.active_tab().conversation.id, conv_id);
}

#[test]
fn test_tab_manager_returns_conversation_on_close() {
    let mut tm = TabManager::default();
    let id0 = tm.active_tab_id();
    let conv_id = tm.active_tab().conversation.id.clone();
    let _ = tm.create_tab();

    let returned = tm.close_tab(id0).unwrap();
    assert_eq!(returned.id, conv_id);
}

// ── Thinking buffer tests ─────────────────────────────────────────────────────

#[test]
fn test_reset_thinking_buffer_clears() {
    let mut streaming = StreamingState {
        thinking_buffer: "some thoughts".to_string(),
        ..StreamingState::default()
    };
    streaming.reset_thinking_buffer();
    assert!(streaming.thinking_buffer.is_empty());
}

#[test]
fn test_switch_tabs_clears_thinking_buffer() {
    let mut tm = TabManager::default();
    // Fill thinking buffer on first tab
    tm.active_tab_mut().streaming.thinking_buffer = "thinking...".to_string();
    let _ = tm.create_tab();
    // After switch, old tab's buffer should be cleared
    // (switch_to_prev goes back to tab 0)
    tm.switch_to_prev();
    assert_eq!(tm.active_tab_index(), 0);
    // The buffer was cleared when we switched FROM tab 0 TO tab 1 on create_tab,
    // but create_tab doesn't call switch logic. Let's switch explicitly:
    tm.switch_to_next();
    // Now back at tab 1, switch back to 0:
    tm.switch_to_prev();
    assert!(
        tm.active_tab().streaming.thinking_buffer.is_empty(),
        "thinking buffer should be cleared on departure"
    );
}

// ── HeightCache ID-keying tests ────────────────────────────────────────────────

use rustain::adapters::tui::state::{HeightCache, MessageHeightKey};

#[test]
fn test_height_cache_message_set_and_get() {
    let mut cache = HeightCache::default();
    let key1 = MessageHeightKey {
        msg_id: "msg-abc-123".to_string(),
        terminal_width: 80,
        content_hash: 0,
    };
    let key2 = MessageHeightKey {
        msg_id: "msg-xyz-999".to_string(),
        terminal_width: 80,
        content_hash: 0,
    };
    cache.set_message(key1.clone(), 5);
    assert_eq!(cache.get_message(&key1), Some(5));
    assert_eq!(cache.get_message(&key2), None);
}

#[test]
fn test_height_cache_invalidate_all() {
    let mut cache = HeightCache::default();
    let key1 = MessageHeightKey {
        msg_id: "msg-1".to_string(),
        terminal_width: 80,
        content_hash: 0,
    };
    let key2 = MessageHeightKey {
        msg_id: "msg-2".to_string(),
        terminal_width: 80,
        content_hash: 0,
    };
    cache.set_message(key1.clone(), 3);
    cache.set_message(key2.clone(), 7);
    cache.invalidate_all();
    assert_eq!(cache.get_message(&key1), None);
    assert_eq!(cache.get_message(&key2), None);
}

#[test]
fn test_height_cache_invalidate_turn() {
    use rustain::adapters::tui::state::HeightKey;
    use rustain::domain::models::turn::TurnId;
    use rustain::domain::models::view_state::SummaryTier;
    let mut cache = HeightCache::default();
    let turn_a = TurnId("turn-a".to_string());
    let turn_b = TurnId("turn-b".to_string());
    let key_a = HeightKey {
        turn_id: turn_a.clone(),
        expansion: true,
        summary_tier: SummaryTier::Tier1,
        terminal_width: 80,
        tool_block_states_version: 0,
    };
    let key_b = HeightKey {
        turn_id: turn_b.clone(),
        expansion: true,
        summary_tier: SummaryTier::Tier1,
        terminal_width: 80,
        tool_block_states_version: 0,
    };
    cache.set(
        key_a.clone(),
        rustain::adapters::tui::state::CachedTurnLayout {
            height: 3,
            block_offsets: vec![],
        },
    );
    cache.set(
        key_b.clone(),
        rustain::adapters::tui::state::CachedTurnLayout {
            height: 7,
            block_offsets: vec![],
        },
    );
    cache.invalidate_turn(&turn_a);
    assert!(cache.get(&key_a).is_none());
    assert_eq!(cache.get(&key_b).map(|l| l.height), Some(7));
}

// ── ChatMessage.id tests ──────────────────────────────────────────────────────

#[test]
fn test_chat_message_has_unique_ids() {
    use rustain::domain::models::conversation::generate_message_id;
    let id1 = generate_message_id();
    let id2 = generate_message_id();
    assert!(!id1.is_empty());
    assert!(!id2.is_empty());
    assert_ne!(id1, id2);
}

#[test]
fn test_chat_message_serializes_with_id() {
    use rustain::domain::models::ChatMessage;
    use rustain::domain::models::MessageRole;

    let msg = ChatMessage {
        synthetic: false,
        id: "test-msg-id".to_string(),
        role: MessageRole::User,
        content: "Hello".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1000,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"id\":\"test-msg-id\""));
    let deserialized: ChatMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, "test-msg-id");
}

#[test]
fn test_chat_message_deserializes_without_id_uses_default() {
    use rustain::domain::models::ChatMessage;

    // Old format without id field
    let json =
        r#"{"role":"user","content":"Hi","contentBlocks":[],"toolCalls":[],"createdAt":1000}"#;
    let msg: ChatMessage = serde_json::from_str(json).unwrap();
    // Should have generated a default ID
    assert!(!msg.id.is_empty());
}

// ── PersistedConversation domain tests ────────────────────────────────────────

#[test]
fn test_persisted_conversation_roundtrip() {
    let conv = make_test_conversation();
    let persisted = PersistedConversation::from_conversation(&conv);
    let restored = persisted.to_conversation();

    assert_eq!(restored.id, conv.id);
    assert_eq!(restored.title, conv.title);
    assert_eq!(restored.messages.len(), conv.messages.len());
}

#[test]
fn test_persisted_conversation_clean_exit_flag() {
    let conv = make_test_conversation();

    let p_dirty = PersistedConversation::from_conversation_with_exit(&conv, false);
    assert!(!p_dirty.clean_exit);

    let p_clean = PersistedConversation::from_conversation_with_exit(&conv, true);
    assert!(p_clean.clean_exit);
}

#[test]
fn test_persisted_conversation_serializes() {
    let conv = make_test_conversation();
    let persisted = PersistedConversation::from_conversation(&conv);
    let json = serde_json::to_string(&persisted).unwrap();
    assert!(json.contains(&conv.id));
    let deserialized: PersistedConversation = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, conv.id);
}

// ── Tab bar widget tests ──────────────────────────────────────────────────────

#[test]
fn test_tab_bar_renders_without_panic() {
    use ratatui::prelude::*;
    use rustain::adapters::tui::color_detect::ColorCapability;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::tab_bar;

    let tm = TabManager::default();
    let area = Rect::new(0, 0, 80, 1);
    let mut buf = Buffer::empty(area);
    let theme = Theme::for_capability(ColorCapability::TrueColor);
    tab_bar::render_tab_bar(&tm, 0, area, &mut buf, &theme);
    // Should render [Tab 1] for a single empty tab
    let content: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(content.contains('['));
}

#[test]
fn test_tab_bar_renders_multiple_tabs() {
    use ratatui::prelude::*;
    use rustain::adapters::tui::color_detect::ColorCapability;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::tab_bar;

    let mut tm = TabManager::default();
    tm.create_tab();
    tm.create_tab();
    let area = Rect::new(0, 0, 80, 1);
    let mut buf = Buffer::empty(area);
    let theme = Theme::for_capability(ColorCapability::TrueColor);
    tab_bar::render_tab_bar(&tm, tm.active_tab_index(), area, &mut buf, &theme);
}

#[test]
fn test_tab_bar_tiny_area_no_panic() {
    use ratatui::prelude::*;
    use rustain::adapters::tui::color_detect::ColorCapability;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::tab_bar;

    let tm = TabManager::default();
    let area = Rect::new(0, 0, 0, 0); // zero size
    let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1)); // minimal buf
    let theme = Theme::for_capability(ColorCapability::TrueColor);
    tab_bar::render_tab_bar(&tm, 0, area, &mut buf, &theme);
    // Should not panic
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_test_conversation() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages: vec![],
        turns: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: Some("sess-test".to_string()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

// ── Story 6-0a: CancellationToken tree + tab lifecycle ───────────────────────

use tokio_util::sync::CancellationToken;

/// AC1: A tab whose turn was cancelled can still start a new turn after reset.
#[test]
fn test_tab_turn_cancel_reset_allows_new_turn() {
    let session = CancellationToken::new();
    let mut tm = TabManager::new(session.clone());
    let id = tm.active_tab_id();

    // Simulate a cancelled turn
    tm.active_tab_mut().turn_cancel.cancel();
    assert!(tm.active_tab().turn_cancel.is_cancelled());

    // Reset the turn cancel (as event_loop does before each start_turn)
    let new_cancel = tm.reset_and_clone_turn_cancel();
    assert!(!new_cancel.is_cancelled());
    assert!(!tm.active_tab().turn_cancel.is_cancelled());

    // Simulate another cancellation cycle
    new_cancel.cancel();
    assert!(tm.active_tab().turn_cancel.is_cancelled());

    // Reset again — tab should remain viable
    let new_cancel_2 = tm.reset_and_clone_turn_cancel();
    assert!(!new_cancel_2.is_cancelled());
    assert!(!tm.active_tab().turn_cancel.is_cancelled());
}

/// AC1: Cancelling one tab's turn does not affect sibling tabs' reset capability.
#[test]
fn test_sibling_tab_reset_independent_after_cancel() {
    let session = CancellationToken::new();
    let mut tm = TabManager::new(session.clone());
    let _id_a = tm.active_tab_id();
    let id_b = tm.create_tab();

    // Cancel tab B's turn
    tm.tabs()[1].turn_cancel.cancel();
    assert!(tm.tabs()[1].turn_cancel.is_cancelled());
    assert!(!tm.tabs()[0].turn_cancel.is_cancelled());

    // Reset active tab (B)
    tm.switch_to_index(2); // 1-based index for tab B
    let cancel_b = tm.reset_and_clone_turn_cancel();
    assert!(!cancel_b.is_cancelled());

    // Tab A should still be independent
    assert!(!tm.tabs()[0].turn_cancel.is_cancelled());

    let _ = id_b;
}
