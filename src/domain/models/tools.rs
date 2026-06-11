#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Definition of a tool available to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Whether this tool may execute concurrently with other tools in the same batch.
    /// Defaults to `false` (sequential) for safety.  Future tools (e.g. Glob, Grep,
    /// WebFetch) must opt in explicitly.
    #[serde(default)]
    pub parallel_safe: bool,
}

/// Tracking info for an in-progress or completed tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub result: Option<ToolResultInfo>,
    /// Unix timestamp in milliseconds (architecture spec uses Instant, but Instant is not serializable).
    pub started_at_ms: Option<u64>,
    /// Unix timestamp in milliseconds.
    pub completed_at_ms: Option<u64>,
    /// Optional status chip string (e.g. "● Executing") set by the event loop
    /// when a `ToolCallTransition` is received.  Omitted from serialisation when
    /// unset so old session JSONL loads cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Info about a tool result stored in ToolCallInfo.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultInfo {
    pub content: String,
    pub is_error: bool,
}
