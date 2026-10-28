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
    /// Opaque replay scope (`model_catalog_cache::replay_scope` over `provider`)
    /// of the credential that produced the item. Replay sends an item only to
    /// a request of the same scope, so it never follows a conversation to
    /// another account; an unscoped item (written before scoping) is never
    /// replayed. Additive: readers without the field ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
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
            .field("scoped", &self.scope.is_some())
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

#[cfg(test)]
mod tests {
    use super::ProviderReplayItem;
    use serde_json::json;
    use std::sync::Arc;

    fn item(scope: Option<&str>) -> ProviderReplayItem {
        ProviderReplayItem {
            provider: "openai-codex".into(),
            model: "m".into(),
            payload: Arc::new(json!({"type":"reasoning","encrypted_content":"ENC-private"})),
            scope: scope.map(str::to_owned),
        }
    }

    #[test]
    fn scoped_item_serializes_its_scope_and_round_trips() {
        let scoped = item(Some("0123456789abcdef"));
        let encoded = serde_json::to_value(&scoped).unwrap();
        assert_eq!(encoded["scope"], "0123456789abcdef");
        let decoded: ProviderReplayItem = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, scoped);
        // The scope is part of the carrier's identity.
        assert_ne!(scoped, item(Some("fedcba9876543210")));
        assert_ne!(scoped, item(None));
    }

    #[test]
    fn unscoped_item_omits_the_key_and_a_pre_scope_file_reads_as_unscoped() {
        let encoded = serde_json::to_value(item(None)).unwrap();
        assert!(encoded.get("scope").is_none(), "{encoded}");
        let legacy = json!({"provider":"openai-codex","model":"m",
            "payload":{"type":"reasoning","encrypted_content":"ENC-private"}});
        let decoded: ProviderReplayItem = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.scope, None);
        assert_eq!(decoded, item(None));
    }

    /// The reader shape of builds without the field still loads a scoped item.
    #[test]
    fn pre_scope_reader_shape_loads_a_scoped_item() {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PreScope {
            provider: String,
            model: String,
            payload: serde_json::Value,
        }
        let encoded = serde_json::to_string(&item(Some("0123456789abcdef"))).unwrap();
        assert!(encoded.contains("\"scope\""), "{encoded}");
        let old: PreScope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(old.provider, "openai-codex");
        assert_eq!(old.model, "m");
        assert_eq!(old.payload["encrypted_content"], "ENC-private");
    }

    #[test]
    fn debug_shows_only_scope_presence() {
        let rendered = format!("{:?}", item(Some("0123456789abcdef")));
        assert!(rendered.contains("scoped: true"), "{rendered}");
        assert!(!rendered.contains("0123456789abcdef"), "{rendered}");
        assert!(!rendered.contains("ENC-private"), "{rendered}");
    }
}
