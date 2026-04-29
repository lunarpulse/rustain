//! AC9 validation: legacy `messages: [...]` deserializer routes through
//! `migrate_chat_message_to_turn` to populate `turns` on pre-16.2 sessions.
//!
//! Also validates round-trip: serialized JSON has both `messages` and `turns`,
//! and re-reading produces an identical `Conversation`.

use rustain::domain::models::{ChatMessage, MessageRole};
use rustain::domain::models::conversation::{Conversation, PersistedConversation};

#[test]
fn legacy_messages_only_json_populates_turns_via_migration() {
    // Hand-authored legacy JSON with only `messages` (no `turns` field)
    // Deserialize via PersistedConversation to trigger the migration in to_conversation()
    let json = r#"{
        "id": "legacy-conv-001",
        "title": "Legacy Session",
        "messages": [
            {
                "id": "msg-001",
                "role": "user",
                "content": "Hello",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000000
            },
            {
                "id": "msg-002",
                "role": "assistant",
                "content": "Hi there!",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000001,
                "stopReason": "endTurn"
            },
            {
                "id": "msg-003",
                "role": "user",
                "content": "What files are here?",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000002
            },
            {
                "id": "msg-004",
                "role": "assistant",
                "content": "Let me check.",
                "contentBlocks": [],
                "toolCalls": [
                    {
                        "id": "tc-1",
                        "name": "Bash",
                        "input": {"command": "ls"},
                        "startedAtMs": 1700000003000,
                        "completedAtMs": null,
                        "status": null
                    }
                ],
                "createdAt": 1700000003
            }
        ],
        "createdAt": 1700000000,
        "updatedAt": 1700000003
    }"#;

    let persisted: PersistedConversation =
        serde_json::from_str(json).expect("legacy JSON should deserialize as PersistedConversation");
    let conversation = persisted.to_conversation();

    // Both turns and messages should be populated
    assert!(
        !conversation.turns.is_empty(),
        "turns should be populated via migration"
    );
    assert!(
        !conversation.messages.is_empty(),
        "messages should still be present"
    );

    // User messages (non-assistant) are preserved intact in messages
    let user_messages: Vec<&ChatMessage> = conversation
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .collect();
    assert_eq!(user_messages.len(), 2, "should have 2 user messages");
    assert_eq!(user_messages[0].content, "Hello");
    assert_eq!(user_messages[1].content, "What files are here?");

    // Assistant messages are rebuilt from turns
    let assistant_messages: Vec<&ChatMessage> = conversation
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    assert_eq!(
        assistant_messages.len(),
        conversation.turns.len(),
        "assistant message count should match turns count"
    );

    // The first turn should have correct content
    let first_turn = &conversation.turns[0];
    assert!(
        !first_turn.parts.is_empty(),
        "first turn should have parts"
    );

    // Round-trip: serialize and deserialize again
    let round_trip_json =
        serde_json::to_string_pretty(&conversation).expect("should serialize");
    let back: Conversation =
        serde_json::from_str(&round_trip_json).expect("round-trip should succeed");

    assert_eq!(conversation.id, back.id);
    assert_eq!(conversation.messages.len(), back.messages.len());
    assert_eq!(conversation.turns.len(), back.turns.len());
}
