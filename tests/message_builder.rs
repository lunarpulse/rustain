use rustain::domain::models::{ChatMessage, Conversation, MessageRole};
use rustain::domain::services::message_builder::build_api_messages;

fn make_conversation(messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: "test".to_string(),
        title: String::new(),
        messages,
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    }
}

#[test]
fn test_build_api_messages_maps_roles() {
    let conv = make_conversation(vec![
        ChatMessage {
            role: MessageRole::User,
            content: "hello".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
        },
        ChatMessage {
            role: MessageRole::Assistant,
            content: "hi there".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
        },
    ]);

    let messages = build_api_messages(&conv);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "hello");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].content, "hi there");
}

#[test]
fn test_build_api_messages_empty_conversation() {
    let conv = make_conversation(vec![]);
    let messages = build_api_messages(&conv);
    assert!(messages.is_empty());
}
