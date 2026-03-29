#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Definition of a tool available to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
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
