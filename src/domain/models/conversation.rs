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
    /// Unique message ID (nanoid). `#[serde(default)]` ensures old sessions without IDs still load.
    #[serde(default = "generate_message_id")]
    pub id: String,
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

/// Generate a unique message ID using nanoid.
pub fn generate_message_id() -> String {
    nanoid::nanoid!()
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

// ── PersistedConversation serde wrapper ────────────────────────────

/// CC-compatible on-disk format with camelCase JSON field naming.
/// All optional fields use `#[serde(default)]` for forward compatibility.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedConversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    pub created_at: i64,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub fork_source: Option<ForkSource>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub last_response_at: Option<i64>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
    /// Crash detection flag: `false` while session is in-flight, `true` after graceful shutdown.
    /// Defaults to `false` for forward compat (old files trigger recovery prompt — safe default).
    #[serde(default)]
    pub clean_exit: bool,
}

impl PersistedConversation {
    pub fn from_conversation(conv: &Conversation) -> Self {
        Self::from_conversation_with_exit(conv, false)
    }

    pub fn from_conversation_with_exit(conv: &Conversation, clean_exit: bool) -> Self {
        Self {
            id: conv.id.clone(),
            title: conv.title.clone(),
            messages: conv.messages.clone(),
            created_at: conv.created_at,
            session_id: conv.session_id.clone(),
            fork_source: conv.fork_source.clone(),
            updated_at: Some(conv.updated_at),
            last_response_at: conv.last_response_at,
            usage: conv.usage.clone(),
            clean_exit,
        }
    }

    pub fn to_conversation(self) -> Conversation {
        Conversation {
            id: self.id,
            title: self.title,
            messages: self.messages,
            created_at: self.created_at,
            updated_at: self.updated_at.unwrap_or(self.created_at),
            last_response_at: self.last_response_at,
            session_id: self.session_id,
            usage: self.usage,
            fork_source: self.fork_source,
        }
    }
}
