#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::diff::WriteDiffState;
use super::usage::UsageInfo;

/// Reason the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        /// Story 19.20c `A6`: may be absent on a withheld attach projection.
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
        /// Runtime-only visibility, bound by the scheduler's named request.
        /// Internal consumers retain the exact result; public projections omit it.
        #[serde(skip)]
        operator_private: bool,
        /// UI-only display-diff state for completed Write calls (Story 19.1
        /// A3 — populated at the one infrastructure site, turn.rs, by reading
        /// the pre-write snapshot back through `StoragePort::read_snapshot`).
        /// Journaled with the raw event → `#[serde(default)]` for replays,
        /// resolving to `NotCaptured { HistoricalOrReattached }`.
        #[serde(default)]
        diff: WriteDiffState,
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

impl StreamChunk {
    /// Proposal arguments and results belong to the operator review surface,
    /// not the tier-agnostic observer stream.
    pub fn is_operator_private(&self) -> bool {
        match self {
            Self::ToolUse { name, .. } => name == super::team_proposal::TEAM_PROPOSAL_TOOL_NAME,
            Self::ToolResult {
                operator_private, ..
            } => *operator_private,
            _ => false,
        }
    }
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
// TODO(S16.4): retire StreamingState alongside chat_pane render flip
#[derive(Debug, Clone)]
pub struct StreamingState {
    pub phase: StreamingPhase,
    pub current_text_buffer: String,
    pub thinking_buffer: String,
    pub current_blocks: Vec<super::content::ContentBlockType>,
    pub active_tool_calls: std::collections::HashMap<String, super::tools::ToolCallInfo>,
    pub is_streaming: bool,
}

impl Default for StreamingState {
    fn default() -> Self {
        Self {
            phase: StreamingPhase::Idle,
            current_text_buffer: String::new(),
            thinking_buffer: String::new(),
            current_blocks: Vec::new(),
            active_tool_calls: std::collections::HashMap::new(),
            is_streaming: false,
        }
    }
}

impl StreamingState {
    /// Clear the thinking buffer (called on tab switch to avoid stale state).
    pub fn reset_thinking_buffer(&mut self) {
        self.thinking_buffer.clear();
    }
}
