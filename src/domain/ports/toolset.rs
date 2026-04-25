#![allow(dead_code)]
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

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
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError>;

    async fn set_execution_context(
        &self,
        _conversation_id: String,
        _checkpoint: CheckpointId,
        _activation_depth: u8,
    ) {
    }
}
