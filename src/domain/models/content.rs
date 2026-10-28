#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Discriminant for content block types in the rendering pipeline.
/// MVP variants are active; later variants are defined with version comments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentBlockType {
    // MVP (Sprint 2)
    Text,
    ToolCall,
    ToolResult,
    PermissionPrompt,
    Error,
    Feedback,
    /// Raw peer-authored transcript content. Persisted so render-time
    /// sanitization and host framing remain active after reload.
    PeerText,

    // v0.5
    Thinking(String),
    /// Operator-private provider replay state (Story 20.1d1b). Persisted for
    /// replay only; every renderer and outward projection skips it.
    ProviderReplay(super::stream::ProviderReplayItem),
    /// One provider response of an assistant message (Story 20.1d4a T2).
    /// Persisted for the provider history only; never rendered and never
    /// projected to observers.
    ResponseSpan(ResponseSpan),
    PlanCard,
    PlanSummary,
    PlanDeviation,
    SubagentStatusBlock,
    AskUserQuestion,
    CompactBoundary,
    DiffBlock,
    TodoList,

    // v1.0
    CodeBlock,
    Table,
    Blockquote,

    // v1.5+
    AgentInspector,
    FleetStatus,

    // v2.0
    NotificationBanner,
    AutoSentMarker,
    TransparencyEntry,
}

/// One provider response inside an assistant `ChatMessage`, in response order
/// (Story 20.1d4a T2). `build_api_messages` rebuilds one assistant `Message`
/// per span, as the turn loop sent them in-turn. A span owns the next
/// `tool_calls` entries of the message's `tool_calls` and the next `replay`
/// of its `ProviderReplay` blocks, counted from the previous span's end: the
/// relative order of both survives every rebuild, absolute block positions
/// do not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseSpan {
    /// The response's visible text, without reasoning.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tool_calls: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replay: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Accumulates one provider response from the turn loop's chunks; `finish`
/// closes it as a [`ResponseSpan`] and starts the next.
#[derive(Debug, Default)]
pub struct ResponseSpanBuilder {
    span: ResponseSpan,
    reasoning: String,
}

impl ResponseSpanBuilder {
    pub fn text(&mut self, text: &str) {
        self.span.text.push_str(text);
    }
    pub fn reasoning(&mut self, text: &str) {
        self.reasoning.push_str(text);
    }
    pub fn call(&mut self) {
        self.span.tool_calls += 1;
    }
    pub fn replay(&mut self) {
        self.span.replay += 1;
    }
    pub fn reasoning_text(&self) -> &str {
        &self.reasoning
    }
    pub fn is_empty(&self) -> bool {
        self.span == ResponseSpan::default() && self.reasoning.is_empty()
    }
    pub fn finish(&mut self) -> ResponseSpan {
        let mut span = std::mem::take(&mut self.span);
        let reasoning = std::mem::take(&mut self.reasoning);
        span.reasoning = (!reasoning.is_empty()).then_some(reasoning);
        span
    }
}
