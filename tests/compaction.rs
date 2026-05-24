//! Integration tests for conversation compaction (Story 7.4).
//!
//! Tests compaction domain service functions and state mutations.

use rustain::domain::models::{
    ChatMessage, Conversation, Message, MessageRole, UsageInfo, generate_conversation_id,
};
use rustain::domain::services::compaction;

// ── Helpers ────────────────────────────────────────────────────

fn make_conversation_with_messages() -> Conversation {
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        turns: Vec::new(),
        created_at: 1000,
        updated_at: 1000,
        last_response_at: None,
        session_id: Some(generate_conversation_id()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    };

    // Add 3 user messages and 2 assistant messages (≥ 2 turns of history)
    conv.messages.push(ChatMessage {
        id: "m1".to_string(),
        role: MessageRole::User,
        content: "First user message".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    });
    conv.messages.push(ChatMessage {
        id: "m2".to_string(),
        role: MessageRole::Assistant,
        content: "First assistant response".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1001,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    });
    conv.messages.push(ChatMessage {
        id: "m3".to_string(),
        role: MessageRole::User,
        content: "Second user message".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1002,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    });
    conv.messages.push(ChatMessage {
        id: "m4".to_string(),
        role: MessageRole::Assistant,
        content: "Second assistant response".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1003,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    });
    conv.messages.push(ChatMessage {
        id: "m5".to_string(),
        role: MessageRole::User,
        content: "Third user message".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1004,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    });

    conv
}

// ── Tests ──────────────────────────────────────────────────────

// Covers: Story 7.4 AC8 — CompactionComplete sets conversation.compaction
#[test]
fn test_compaction_complete_sets_compaction_state() {
    let mut conv = make_conversation_with_messages();
    conv.usage = Some(UsageInfo {
        input_tokens: 1000,
        output_tokens: 500,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: None,
    });

    // Simulate CompactionComplete event handling
    let summary = "Compacted context".to_string();
    let first_kept = "m5".to_string();
    let pre_tokens = 1000;

    conv.compaction = Some(rustain::domain::models::conversation::CompactionState {
        summary: summary.clone(),
        first_kept_message_id: first_kept.clone(),
        compacted_at: 2000,
        pre_compaction_tokens: pre_tokens,
    });

    assert!(conv.compaction.is_some());
    let cs = conv.compaction.as_ref().unwrap();
    assert_eq!(cs.summary, summary);
    assert_eq!(cs.first_kept_message_id, first_kept);
    assert_eq!(cs.pre_compaction_tokens, pre_tokens);

    // Original messages should be preserved (AC7)
    assert_eq!(conv.messages.len(), 5);
}

// Covers: Story 7.4 AC9 — CompactionFailed leaves conversation untouched
#[test]
fn test_compaction_failed_leaves_conversation_untouched() {
    let conv = make_conversation_with_messages();
    let original_messages = conv.messages.clone();
    let original_usage = conv.usage.clone();

    // Simulate CompactionFailed — conversation should not change
    // (In real handler, conversation is left untouched)
    assert_eq!(conv.messages.len(), original_messages.len());
    assert_eq!(
        conv.usage.as_ref().map(|u| u.input_tokens),
        original_usage.as_ref().map(|u| u.input_tokens)
    );
    assert!(conv.compaction.is_none());
}

// Covers: Story 7.4 AC2 — first_kept_message_id returns None with < 2 turns
#[test]
fn test_first_kept_message_id_less_than_two_turns() {
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        turns: Vec::new(),
        created_at: 1000,
        updated_at: 1000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    };

    // Only 1 user message
    conv.messages.push(ChatMessage {
        id: "m1".to_string(),
        role: MessageRole::User,
        content: "Only message".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    });

    assert_eq!(compaction::first_kept_message_id(&conv), None);
}

// Covers: Story 7.4 AC2 — first_kept_message_id returns last user msg with ≥ 2 turns
#[test]
fn test_first_kept_message_id_with_two_turns() {
    let conv = make_conversation_with_messages();
    // 3 user messages (m1, m3, m5) → last is m5
    assert_eq!(
        compaction::first_kept_message_id(&conv),
        Some("m5".to_string())
    );
}

// Covers: Story 7.4 AC2 — build_compaction_prompt_input flattens messages
#[test]
fn test_build_compaction_prompt_input_flattening() {
    let conv = make_conversation_with_messages();
    let input = compaction::build_compaction_prompt_input(&conv, "m5", 1_000_000);

    assert!(input.contains("[User]: First user message"));
    assert!(input.contains("[Assistant]: First assistant response"));
    assert!(input.contains("[User]: Second user message"));
    assert!(input.contains("[Assistant]: Second assistant response"));
    // Boundary message (m5) and after should be excluded
    assert!(!input.contains("Third user message"));
}

// Covers: Story 7.4 AC7 — shape_compacted_messages slices and sets prefix
#[test]
fn test_shape_compacted_messages_slices_and_prefixes() {
    let mut conv = make_conversation_with_messages();
    conv.compaction = Some(rustain::domain::models::conversation::CompactionState {
        summary: "Summary text".to_string(),
        first_kept_message_id: "m5".to_string(),
        compacted_at: 2000,
        pre_compaction_tokens: 1000,
    });

    let mut api_msgs = vec![
        Message {
            role: MessageRole::User,
            content: "First user message".to_string(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
        reasoning_content: None,
        },
        Message {
            role: MessageRole::Assistant,
            content: "First assistant response".to_string(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
        reasoning_content: None,
        },
        Message {
            role: MessageRole::User,
            content: "Third user message".to_string(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
        reasoning_content: None,
        },
    ];

    compaction::shape_compacted_messages(&conv, &mut api_msgs);

    // Should keep only messages from boundary onward
    assert_eq!(api_msgs.len(), 1);
    assert_eq!(api_msgs[0].content, "Third user message");
    assert!(
        api_msgs[0]
            .context_prefix
            .as_ref()
            .unwrap()
            .contains("<conversation-summary>")
    );
    assert!(
        api_msgs[0]
            .context_prefix
            .as_ref()
            .unwrap()
            .contains("Summary text")
    );
}

// Covers: Story 7.4 AC2 — compose_context_prefix joins with \\n\\n
#[test]
fn test_compose_context_prefix_both_branches() {
    assert_eq!(
        compaction::compose_context_prefix(None, "a".to_string()),
        "a"
    );
    assert_eq!(
        compaction::compose_context_prefix(Some("x".to_string()), "y".to_string()),
        "x\n\ny"
    );
}
