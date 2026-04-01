use crate::domain::models::{ChatMessage, Conversation, Message};

/// Build provider API messages from a conversation's chat history.
/// Maps ChatMessage -> Message for each conversation message.
pub fn build_api_messages(conversation: &Conversation) -> Vec<Message> {
    conversation
        .messages
        .iter()
        .map(|cm: &ChatMessage| Message {
            role: cm.role,
            content: cm.content.clone(),
            images: vec![],
            tool_results: vec![], // TODO(1-5): map tool_calls to tool_results
            context_prefix: None,
        })
        .collect()
}
