#![allow(dead_code)]
use async_trait::async_trait;

use crate::domain::errors::ToolError;
use crate::domain::models::checkpoint::CheckpointId;
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

    /// Set the active checkpoint context for file snapshotting.
    ///
    /// Called by `turn.rs` immediately after `create_checkpoint` succeeds, before
    /// any tool in that turn is dispatched.  Every file-writing tool (`Write`, `Edit`)
    /// must call `storage.snapshot_file()` with this context before mutating the file.
    ///
    /// Default: no-op.  `NoOpToolSet` and test doubles inherit this default.
    async fn set_execution_context(&self, _conversation_id: String, _checkpoint: CheckpointId) {
        // no-op default
    }

    // v0.5+: fn register(&self, tool: ToolDefinition);
    // v0.5+: fn unregister(&self, tool_name: &str);
    // v0.5+: fn has_tool(&self, tool_name: &str) -> bool;
}
