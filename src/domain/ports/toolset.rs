#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
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
// 2026-05-19 — Story 8.5 added health_snapshot() with default HealthSummary::unknown() impl
// following additive-with-defaults discipline. No existing adapters needed changes.
// Real metrics ship with real adapters in Epic 12.
//
// 2026-05-20 — Story 9.1 added swap_tier() with default SwapTier::Hot +
// prepare_detach/receive_state/post_transition_verify defaults returning Ok(empty),
// matching the warm-tier protocol from Story 8.4. Composite override returns Warm.
#[async_trait]
pub trait ToolSetPort: Send + Sync {
    fn available_tools(&self) -> Vec<ToolDefinition>;

    fn health_snapshot(&self) -> crate::domain::models::HealthSummary {
        crate::domain::models::HealthSummary::unknown()
    }

    /// Returns the swap tier for this tools adapter.
    /// `ToolSetAdapter` (builtin) inherits the default `Hot`.
    /// `CompositeToolsetAdapter` overrides to `Warm` (Story 9.1).
    fn swap_tier(&self) -> crate::domain::services::swap_tier::SwapTier {
        crate::domain::services::swap_tier::SwapTier::Hot
    }

    /// Warm-tier protocol: capture state before profile switch.
    /// Default returns empty — stateless adapters need no preparation.
    async fn prepare_detach(
        &self,
    ) -> Result<crate::domain::models::TransitionState, crate::domain::errors::TransitionError>
    {
        Ok(crate::domain::models::TransitionState::empty("tools"))
    }

    /// Warm-tier protocol: restore state after profile switch.
    /// Default is a no-op.
    async fn receive_state(
        &self,
        _state: crate::domain::models::TransitionState,
    ) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }

    /// Warm-tier protocol: verify integrity after transition.
    /// Default returns Ok.
    async fn post_transition_verify(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }

    /// Support downcasting from `dyn ToolSetPort` to concrete types.
    /// Used by infrastructure code that needs adapter-specific methods
    /// (e.g., status panel MCP sub-rows, startup MCP connect).
    fn as_any(&self) -> &dyn std::any::Any {
        // Default returns a type-erased reference; concrete adapters override.
        // Since we can't provide a useful default, callers should handle None.
        // Each implementor that needs downcast support must override this.
        unreachable!("as_any must be overridden by concrete adapters that support downcasting")
    }

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
