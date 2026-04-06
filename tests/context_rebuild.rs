//! Integration tests for history context rebuild (Story 2.2b).

use rustain::domain::models::{ChatMessage, MessageRole};
use rustain::domain::services::history_rebuild::build_history_context;

fn make_message(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1700000000,
        token_count: Some(10),
        stop_reason: None,
    }
}

/// 7.12: Integration test -- context rebuild with simulated session expiry.
/// Verifies that build_history_context produces correct context_prefix.
// Covers: FR15 (history rebuild)
#[test]
fn test_context_rebuild_produces_prefix() {
    let messages = vec![
        make_message(MessageRole::User, "Hello, can you help me?"),
        make_message(
            MessageRole::Assistant,
            "Of course! What do you need help with?",
        ),
        make_message(MessageRole::User, "I need to fix a bug in my code"),
        make_message(
            MessageRole::Assistant,
            "Sure, can you share the relevant code?",
        ),
    ];

    let context = build_history_context(&messages);

    assert!(context.starts_with("Previous conversation context (4 messages):"));
    assert!(context.contains("[User]: Hello, can you help me?"));
    assert!(context.contains("[Assistant]: Of course! What do you need help with?"));
    assert!(context.contains("[User]: I need to fix a bug in my code"));
    assert!(context.contains("[Assistant]: Sure, can you share the relevant code?"));
}

/// Context rebuild strips XML tags from messages.
// Covers: FR15 (history rebuild)
#[test]
fn test_context_rebuild_strips_xml() {
    let messages = vec![
        make_message(
            MessageRole::User,
            "Look at this <file_context>long xml content here</file_context> please",
        ),
        make_message(
            MessageRole::Assistant,
            "I see the issue in <file_content>fn main() { panic!() }</file_content> your code",
        ),
    ];

    let context = build_history_context(&messages);

    assert!(!context.contains("file_context"));
    assert!(!context.contains("file_content"));
    assert!(context.contains("[User]: Look at this  please"));
    assert!(context.contains("[Assistant]: I see the issue in  your code"));
}

/// Context rebuild truncates long messages to 200 chars.
// Covers: FR15 (history rebuild)
#[test]
fn test_context_rebuild_truncates_long_messages() {
    let long_content = "A".repeat(500);
    let messages = vec![make_message(MessageRole::User, &long_content)];

    let context = build_history_context(&messages);

    // The user line should contain at most 200 chars of content + "..."
    let expected_truncated = format!("{}...", "A".repeat(200));
    assert!(context.contains(&format!("[User]: {}", expected_truncated)));
}

/// Context rebuild handles empty message list.
// Covers: FR15 (history rebuild)
#[test]
fn test_context_rebuild_empty_messages() {
    let context = build_history_context(&[]);
    assert!(context.is_empty());
}
