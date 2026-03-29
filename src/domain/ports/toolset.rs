#![allow(dead_code)]
use async_trait::async_trait;

use crate::domain::errors::ToolError;
use crate::domain::models::{ToolDefinition, ToolResult};

/// Tool discovery and execution.
///
/// Claudian equivalent: `src/core/tools/toolManager.ts`
#[async_trait]
pub trait ToolSetPort: Send + Sync {
    fn available_tools(&self) -> Vec<ToolDefinition>;
    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Result<ToolResult, ToolError>;

    // v0.5+: fn register(&self, tool: ToolDefinition);
    // v0.5+: fn unregister(&self, tool_name: &str);
    // v0.5+: fn has_tool(&self, tool_name: &str) -> bool;
}
