#![allow(dead_code)]
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::errors::ToolError;
use crate::domain::events::ToolProgressEvent;
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

    /// Execute a tool with per-call identity and optional progress channel.
    ///
    /// Default impl delegates to `execute()` so existing implementors
    /// (MockToolSet, NoOp) compile unchanged. Story 16.9.
    async fn execute_with_id(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
        progress_tx: Option<mpsc::UnboundedSender<ToolProgressEvent>>,
    ) -> Result<ToolResult, ToolError> {
        let _ = (tool_use_id, progress_tx);
        self.execute(tool_name, input, cancel).await
    }

    /// Validate tool input against its declared schema.
    /// Default implementation accepts everything; strict validation is deferred
    /// to MCP integration (Story 9-2) where schemas matter.
    fn validate_input(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Query whether a tool is safe to execute in parallel with others.
    /// Unknown tools default to `false` (sequential fallback).
    fn is_parallel_safe(&self, tool_name: &str) -> bool {
        self.available_tools()
            .iter()
            .find(|t| t.name == tool_name)
            .map(|t| t.parallel_safe)
            .unwrap_or(false)
    }

    async fn set_execution_context(
        &self,
        _conversation_id: String,
        _checkpoint: CheckpointId,
        _activation_depth: u8,
    ) {
    }
}
