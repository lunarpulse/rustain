use crate::domain::models::{
    Conversation, Message, MessageRole, ToolResultMessage, ToolUseMessage,
};

/// Build provider API messages from a conversation's chat history.
/// Maps ChatMessage -> Message for each conversation message.
///
/// Tool calls in an assistant message produce a separate user-role message
/// containing the tool results (Anthropic API format).
pub fn build_api_messages(conversation: &Conversation) -> Vec<Message> {
    let mut messages = Vec::new();

    for cm in &conversation.messages {
        // For assistant messages with tool calls, include tool_use blocks
        // so the Anthropic API can match tool_results to their originating calls.
        let tool_uses = if cm.role == MessageRole::Assistant {
            cm.tool_calls
                .iter()
                .map(|tc| ToolUseMessage {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.input.clone(),
                })
                .collect()
        } else {
            vec![]
        };

        messages.push(Message {
            role: cm.role,
            content: cm.content.clone(),
            images: vec![],
            tool_results: vec![],
            tool_uses,
            context_prefix: None,
        });

        // If assistant message has tool calls with results, add a user message with tool results
        if cm.role == MessageRole::Assistant && !cm.tool_calls.is_empty() {
            let results: Vec<ToolResultMessage> = cm
                .tool_calls
                .iter()
                .filter_map(|tc| {
                    tc.result.as_ref().map(|r| ToolResultMessage {
                        tool_use_id: tc.id.clone(),
                        content: r.content.clone(),
                        is_error: r.is_error,
                    })
                })
                .collect();

            if !results.is_empty() {
                messages.push(Message {
                    role: MessageRole::User,
                    content: String::new(),
                    images: vec![],
                    tool_results: results,
                    tool_uses: vec![],
                    context_prefix: None,
                });
            }
        }
    }

    messages
}
