use serde::{Deserialize, Serialize};

// ── Conversation ────────────────────────────────────────────────

pub type ConversationId = String;

/// Claudian-compatible conversation model.
/// This is rustain's own type — NOT rustycode's domain::Session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_response_at: Option<i64>,
    pub messages: Vec<ChatMessage>,

    /// SDK session ID for resume (captured from first API response)
    pub session_id: Option<String>,
    /// Previous expired session IDs
    pub previous_session_ids: Vec<String>,

    /// Currently attached file context
    pub current_note: Option<String>,
    /// External directories with read/write access
    pub external_context_paths: Vec<String>,

    /// Token usage tracking
    pub usage: Option<UsageInfo>,
    /// Active MCP servers for this conversation
    pub enabled_mcp_servers: Vec<String>,

    /// For fork: resume SDK session at this assistant UUID
    pub resume_session_at: Option<String>,
    /// Fork metadata (if this conversation was forked)
    pub fork_source: Option<ForkSource>,

    /// Title generation state
    pub title_generation_status: TitleStatus,
}

impl Conversation {
    pub fn new() -> Self {
        let now = chrono::Utc::now().timestamp_millis();
        let id = generate_conversation_id();
        Self {
            id,
            title: "New Chat".to_string(),
            created_at: now,
            updated_at: now,
            last_response_at: None,
            messages: Vec::new(),
            session_id: None,
            previous_session_ids: Vec::new(),
            current_note: None,
            external_context_paths: Vec::new(),
            usage: None,
            enabled_mcp_servers: Vec::new(),
            resume_session_at: None,
            fork_source: None,
            title_generation_status: TitleStatus::Pending,
        }
    }
}

/// Generate Claudian-compatible conversation ID: conv-{timestamp}-{random9}
fn generate_conversation_id() -> String {
    use rand::Rng;
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random: String = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(9)
        .map(char::from)
        .collect::<String>()
        .to_lowercase();
    format!("conv-{}-{}", timestamp, random)
}

// ── Chat Message ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub role: MessageRole,
    /// Full text content (for search, export, etc.)
    pub content: String,
    /// Ordered content blocks for rendering (preserves streaming order)
    pub content_blocks: Vec<ContentBlock>,
    /// Tool call metadata
    pub tool_calls: Vec<ToolCallInfo>,
    /// Image attachments
    pub images: Vec<ImageAttachment>,
    /// SDK user message UUID (for fork/rewind)
    pub sdk_user_uuid: Option<String>,
    /// SDK assistant message UUID (for fork/rewind)
    pub sdk_assistant_uuid: Option<String>,
    pub timestamp: i64,
    /// True if this message was interrupted (Ctrl+C partial response)
    pub is_interrupt: bool,
    /// True if this is a rebuilt history context message
    pub is_rebuilt_context: bool,
}

impl ChatMessage {
    pub fn user(content: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::User,
            content_blocks: vec![ContentBlock::Text {
                content: content.clone(),
            }],
            content,
            tool_calls: Vec::new(),
            images: Vec::new(),
            sdk_user_uuid: None,
            sdk_assistant_uuid: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
            is_interrupt: false,
            is_rebuilt_context: false,
        }
    }

    pub fn assistant() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::Assistant,
            content: String::new(),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            images: Vec::new(),
            sdk_user_uuid: None,
            sdk_assistant_uuid: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
            is_interrupt: false,
            is_rebuilt_context: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

// ── Content Blocks ──────────────────────────────────────────────

/// Content block for preserving streaming render order.
/// Each assistant message is a sequence of these blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text {
        content: String,
    },
    ToolUse {
        tool_id: String,
        name: String,
        input: String,
        result: Option<String>,
        is_error: bool,
        #[serde(skip)]
        collapsed: bool,
    },
    Thinking {
        content: String,
        duration_seconds: Option<f64>,
        #[serde(skip)]
        collapsed: bool,
    },
    Subagent {
        subagent_id: String,
        mode: SubagentMode,
    },
    CompactBoundary,
}

// ── Tool Call Info ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub result: Option<String>,
    pub is_error: bool,
    pub status: ToolCallStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Pending,
    Executing,
    Complete,
    Error,
    Blocked,
}

// ── Subagent ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentMode {
    Sync,
    Async,
}

// ── Fork Source ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSource {
    pub session_id: String,
    pub resume_at: String,
}

// ── Usage Info ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub context_window: u32,
    pub context_tokens: u32,
    pub percentage: f32,
}

// ── Title Status ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TitleStatus {
    Pending,
    Generating,
    Success,
    Failed,
    ManuallySet,
}

// ── Image Attachment ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub id: String,
    pub path: Option<String>,
    pub media_type: String,
    /// Base64 data (cleared after save to reduce memory)
    #[serde(skip)]
    pub data: Option<String>,
    pub sha256: Option<String>,
}
