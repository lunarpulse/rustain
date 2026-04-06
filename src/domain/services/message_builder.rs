use crate::domain::models::{
    Conversation, Message, MessageRole, ToolResultMessage, ToolUseMessage,
};

/// Resolved file content to be attached to a message.
/// Created by the adapter layer (file I/O happens there), consumed by the domain.
#[derive(Debug, Clone)]
pub struct ResolvedFileContext {
    pub path: String,
    pub content: String,
}

/// Resolved slash command content to be prepended as a system instruction.
/// Created by the adapter layer (file I/O happens there), consumed by the domain.
#[derive(Debug, Clone)]
pub struct ResolvedCommandContext {
    pub name: String,
    pub content: String,
}

/// Escape XML special characters in attribute values.
fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build a context prefix string from resolved file mentions.
/// Returns XML-formatted file content blocks to be prepended to the user message.
pub fn build_file_context_prefix(files: &[ResolvedFileContext]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut prefix = String::new();
    for file in files {
        prefix.push_str(&format!(
            "<file path=\"{}\">\n<![CDATA[{}]]>\n</file>\n\n",
            escape_xml_attr(&file.path),
            file.content.replace("]]>", "]]]]><![CDATA[>")
        ));
    }
    prefix
}

/// Build a context prefix string from a resolved slash command.
/// Returns XML-formatted command content to be prepended to the user message.
pub fn build_command_context_prefix(command: &ResolvedCommandContext) -> String {
    format!(
        "<command name=\"{}\">\n<![CDATA[{}]]>\n</command>\n\n",
        escape_xml_attr(&command.name),
        command.content.replace("]]>", "]]]]><![CDATA[>")
    )
}

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
