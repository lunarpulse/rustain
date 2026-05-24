#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Role of a message in the provider API conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageRole {
    User,
    Assistant,
    #[serde(rename = "system")]
    System,
}

impl Default for MessageRole {
    fn default() -> Self {
        MessageRole::Assistant
    }
}

/// Provider-agnostic API message. Each adapter translates to its wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub images: Vec<ImageAttachment>,
    pub tool_results: Vec<ToolResultMessage>,
    /// Tool use blocks from assistant messages (needed for multi-turn tool conversations).
    pub tool_uses: Vec<ToolUseMessage>,
    pub context_prefix: Option<String>,
    /// Reasoning/thinking content (DeepSeek v4 thinking mode). Echoed back verbatim
    /// on assistant messages so the API can validate continuity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// A tool use block from an assistant message, sent back for multi-turn tool conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUseMessage {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// A tool result included in a follow-up message to the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

/// An image attached to a user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    pub media_type: String,
    pub data: String,
}

/// A user message queued for sending (used by TurnQueue).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: String,
    pub images: Vec<ImageAttachment>,
}
