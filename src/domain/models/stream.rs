#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::usage::UsageInfo;

/// Reason the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Cancelled,
}

/// A single chunk in a streaming response from a provider.
/// The adapter converts wire-format events into these domain chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamChunk {
    Text {
        content: String,
        parent_tool_use_id: Option<String>,
    },
    Thinking {
        content: String,
        parent_tool_use_id: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Error {
        content: String,
    },
    Blocked {
        content: String,
    },
    TurnComplete {
        stop_reason: StopReason,
    },
    Usage {
        usage: UsageInfo,
        session_id: Option<String>,
    },
    // v0.5+:
    // CompactBoundary,
    // SdkUserUuid { uuid: String },
    // SdkUserSent { uuid: String },
    // SdkAssistantUuid { uuid: String },
    // ContextWindowUpdate { context_window: u32 },
}

/// State machine tracking where we are in the streaming process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamingPhase {
    Idle,
    AccumulatingText,
    InToolCall { tool_id: String },
    InThinking,
    AwaitingToolExecution,
}

/// Transient streaming state (not persisted).
#[derive(Debug, Clone)]
pub struct StreamingState {
    pub phase: StreamingPhase,
    pub current_text_buffer: String,
    pub current_blocks: Vec<super::content::ContentBlockType>,
    pub active_tool_calls: std::collections::HashMap<String, super::tools::ToolCallInfo>,
    pub is_streaming: bool,
}

impl Default for StreamingState {
    fn default() -> Self {
        Self {
            phase: StreamingPhase::Idle,
            current_text_buffer: String::new(),
            current_blocks: Vec::new(),
            active_tool_calls: std::collections::HashMap::new(),
            is_streaming: false,
        }
    }
}
