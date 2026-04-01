#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::content::ContentBlockType;
use super::stream::StopReason;
use super::tools::ToolCallInfo;
use super::usage::UsageInfo;

/// A single message in a conversation. Persisted to session files.
/// Distinct from `Message` which is the provider-agnostic API request format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: super::message::MessageRole,
    pub content: String,
    pub content_blocks: Vec<ContentBlockType>,
    pub tool_calls: Vec<ToolCallInfo>,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    pub token_count: Option<u32>,
    /// Why this message's generation stopped. `None` for user messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

/// Persistable conversation data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Unix timestamp in seconds.
    pub updated_at: i64,
    /// Unix timestamp in seconds.
    pub last_response_at: Option<i64>,
    pub session_id: Option<String>,
    pub usage: Option<UsageInfo>,
    pub fork_source: Option<ForkSource>,
    // v0.5+: pub active_agent: Option<AgentDefinition>,
    // v1.0+: pub enabled_mcp_servers: Vec<String>,
    // v1.0+: pub external_context_paths: Vec<String>,
}

/// Summary for conversation list display (lighter than full Conversation).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Unix timestamp in seconds.
    pub updated_at: i64,
    pub message_count: usize,
}

/// Tracks the origin of a forked conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSource {
    pub conversation_id: String,
    pub message_index: usize,
}

/// Generate a unique conversation ID using nanoid.
pub fn generate_conversation_id() -> String {
    nanoid::nanoid!()
}
