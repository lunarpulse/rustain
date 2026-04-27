#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Discriminant for content block types in the rendering pipeline.
/// MVP variants are active; later variants are defined with version comments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentBlockType {
    // MVP (Sprint 2)
    Text,
    ToolCall,
    ToolResult,
    PermissionPrompt,
    Error,
    Feedback,

    // v0.5
    Thinking(String),
    PlanCard,
    PlanSummary,
    PlanDeviation,
    SubagentStatus,
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
