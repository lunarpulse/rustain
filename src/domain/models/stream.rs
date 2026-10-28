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
        /// Result images for the operator's own history (Story 20.1d4a T1);
        /// every observer projection empties them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<super::message::ImageAttachment>,
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
    /// Provider-owned replay state from a completed response (Story 20.1d1b
    /// E1). Operator-private: kept in the operator's own history only.
    ProviderReplay {
        item: ProviderReplayItem,
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
    /// not the tier-agnostic observer stream. Provider replay state is
    /// account-bound model state that never leaves the operator's history.
    pub fn is_operator_private(&self) -> bool {
        match self {
            Self::ToolUse { name, .. } => name == super::team_proposal::TEAM_PROPOSAL_TOOL_NAME,
            Self::ToolResult {
                operator_private, ..
            } => *operator_private,
            Self::ProviderReplay { .. } => true,
            _ => false,
        }
    }
}

/// One provider-native response item kept for replay to the same provider kind
/// and model (Story 20.1d1b E1). `payload` is the provider's reduced item, e.g.
/// Codex `{type, summary, content, encrypted_content}`. It is shared, not
/// copied, as the item moves through the turn loop, reducer mirror and keepers.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderReplayItem {
    pub provider: String,
    pub model: String,
    #[serde(with = "shared_value")]
    pub payload: std::sync::Arc<serde_json::Value>,
}

mod shared_value {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::Arc;

    pub fn serialize<S: Serializer>(
        value: &Arc<serde_json::Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_ref().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<serde_json::Value>, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Arc::new)
    }
}

impl ProviderReplayItem {
    /// Encoded payload size in bytes, measured without materialising the JSON.
    pub fn encoded_len(&self) -> usize {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut count = Count(0);
        let _ = serde_json::to_writer(&mut count, self.payload.as_ref());
        count.0
    }
}

/// Never prints `summary`, `content` or `encrypted_content`, so `tracing` and
/// panic messages cannot leak the replayed reasoning.
impl std::fmt::Debug for ProviderReplayItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderReplayItem")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("type", &self.payload["type"].as_str().unwrap_or(""))
            .field("bytes", &self.encoded_len())
            .finish()
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
