//! E2E tests for Story 7.4: Context Window Tracking & Compaction.
//!
//! Covers rendering-level verification of compaction-related UI state
//! using the TestHarness and domain-model round-trips.

use rustain::adapters::tui::state::ContextWarnLevel;
use rustain::domain::models::conversation::{CompactionState, PersistedConversation};
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::{
    Conversation, FeedbackAction, FeedbackBlock, FeedbackLevel, generate_conversation_id,
};

mod e2e_harness;

mod common;

// ── Feedback block rendering via TestHarness ─────────────────────

// Covers: AC3 — context-warning block renders in chat pane
#[test]
fn test_context_warning_feedback_block_renders() {
    let mut h = e2e_harness::TestHarness::new();
    let fb = FeedbackBlock {
        id: "ctxwarn-1".to_string(),
        level: FeedbackLevel::Warning,
        message: "Running low on context (85%).".to_string(),
        actions: vec![FeedbackAction::Compact, FeedbackAction::StartFresh],
    };
    h.feedback_blocks.insert(fb.id.clone(), fb);
    h.state.active_feedback_id = Some("ctxwarn-1".to_string());
    h.render();

    let text = h.screen_text();
    assert!(
        text.contains("Running low on context (85%)."),
        "Warning message should render in chat pane, got lines:\n{}",
        h.screen_text_lines().join("\n")
    );
}

// Covers: AC8 — compaction success block renders
#[test]
fn test_compaction_success_feedback_block_renders() {
    let mut h = e2e_harness::TestHarness::new();
    let fb = FeedbackBlock {
        id: "cdone-abc".to_string(),
        level: FeedbackLevel::Info,
        message: "Context compacted: 45k → 8k tokens. Conversation history preserved in session."
            .to_string(),
        actions: vec![],
    };
    h.feedback_blocks.insert(fb.id.clone(), fb);
    h.render();

    let text = h.screen_text();
    assert!(
        text.contains("Context compacted:"),
        "Success message should render in chat pane, got lines:\n{}",
        h.screen_text_lines().join("\n")
    );
}

// Covers: AC9 — compaction failure block renders with retry action
#[test]
fn test_compaction_failure_feedback_block_renders() {
    let mut h = e2e_harness::TestHarness::new();
    let fb = FeedbackBlock {
        id: "cfail-abc".to_string(),
        level: FeedbackLevel::Error,
        message: "Compaction failed: LLM timeout. Context unchanged.".to_string(),
        actions: vec![FeedbackAction::Retry, FeedbackAction::StartFresh],
    };
    h.feedback_blocks.insert(fb.id.clone(), fb);
    h.state.active_feedback_id = Some("cfail-abc".to_string());
    h.render();

    let text = h.screen_text();
    assert!(
        text.contains("Compaction failed:"),
        "Failure message should render in chat pane, got lines:\n{}",
        h.screen_text_lines().join("\n")
    );
}

// ── TabState persistence ─────────────────────────────────────────

// Covers: Patch-5 — context_warn_level survives tab switch via TabManager
#[test]
fn test_tab_state_context_warn_level_survives_switch() {
    let mut tm = TabManager::default();

    // Set a warning level on the first tab
    tm.active_tab_mut().context_warn_level = ContextWarnLevel::Crit;
    assert_eq!(tm.active_tab().context_warn_level, ContextWarnLevel::Crit);

    // Create a second tab (switches to it automatically)
    tm.create_tab();
    tm.active_tab_mut().context_warn_level = ContextWarnLevel::None;
    assert_eq!(tm.active_tab().context_warn_level, ContextWarnLevel::None);

    // Switch back to first tab (1-based index)
    tm.switch_to_index(1);
    assert_eq!(tm.active_tab().context_warn_level, ContextWarnLevel::Crit);

    // Switch to second tab (1-based index)
    tm.switch_to_index(2);
    assert_eq!(tm.active_tab().context_warn_level, ContextWarnLevel::None);
}

// ── PersistedConversation round-trip ─────────────────────────────

// Covers: Patch-9/10 — compaction field survives serialize→deserialize
#[test]
fn test_persisted_conversation_compaction_round_trip() {
    let conv = Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages: Vec::new(),
        turns: Vec::new(),
        created_at: 1000,
        updated_at: 1000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: Some(CompactionState {
            summary: "Compacted summary".to_string(),
            first_kept_message_id: "msg-42".to_string(),
            compacted_at: 2000,
            pre_compaction_tokens: 5000,
        }),
    };

    let persisted = PersistedConversation::from_conversation(&conv);
    assert!(persisted.compaction.is_some());
    let cs = persisted.compaction.as_ref().unwrap();
    assert_eq!(cs.summary, "Compacted summary");
    assert_eq!(cs.first_kept_message_id, "msg-42");
    assert_eq!(cs.pre_compaction_tokens, 5000);

    let round_trip = persisted.to_conversation();
    assert!(round_trip.compaction.is_some());
    let cs2 = round_trip.compaction.as_ref().unwrap();
    assert_eq!(cs2.summary, "Compacted summary");
    assert_eq!(cs2.first_kept_message_id, "msg-42");
}

// Covers: backward compat — legacy JSON without compaction deserializes to None
#[test]
fn test_persisted_conversation_backward_compat_without_compaction() {
    let json = r#"{
        "id": "abc",
        "title": "Legacy",
        "messages": [],
        "turns": [],
        "createdAt": 1000,
        "sessionId": null,
        "forkSource": null,
        "updatedAt": null,
        "lastResponseAt": null,
        "usage": null,
        "plans": {},
        "cleanExit": false
    }"#;

    let persisted: PersistedConversation = serde_json::from_str(json).unwrap();
    assert!(persisted.compaction.is_none());
}
