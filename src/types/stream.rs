/// TUI-specific stream events, mapped from rustycode's provider StreamChunk.
///
/// These extend the domain's basic streaming types with presentation-layer
/// semantics needed for Claudian-like rendering.
#[derive(Debug, Clone)]
pub enum TuiStreamEvent {
    /// Streaming text content (append to current message)
    Text {
        content: String,
        parent_tool_use_id: Option<String>,
    },

    /// Extended thinking content
    Thinking {
        content: String,
        parent_tool_use_id: Option<String>,
    },

    /// Tool use started
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        parent_tool_use_id: Option<String>,
    },

    /// Tool execution result
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },

    /// Partial output from a running tool (e.g., bash stdout streaming).
    /// Terminal-native enhancement — shows live command output during execution.
    ToolPartialOutput {
        tool_id: String,
        line: String,
    },

    /// Error from provider
    Error { content: String },

    /// Tool call blocked by permission system
    Blocked { content: String },

    /// Turn complete
    Done,

    /// Token usage update
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_tokens: u32,
        cache_read_tokens: u32,
        context_window: u32,
    },

    /// Compact boundary separator
    CompactBoundary,

    /// SDK UUIDs for fork/rewind support
    SdkUserUuid { uuid: String },
    SdkAssistantUuid { uuid: String },

    /// Context window size update
    ContextWindowUpdate { context_window: u32 },
}
