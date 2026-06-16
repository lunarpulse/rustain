//! NoOp adapter stubs for all ports.
//! Used during startup before real adapters are wired, and in tests.

#![allow(dead_code)]

use std::path::Path;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::domain::errors::{AuthError, PermissionError, ProviderError, StorageError, ToolError};
use crate::domain::models::ApprovalScope;
use crate::domain::models::credential::{Credential, ProviderStatus};
use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
use crate::domain::models::usage::UsageLedgerEntry;
use crate::domain::models::{
    CompletionOptions, Conversation, ConversationSummary, FileOperation, Message, PathAccessType,
    PermissionMode, StreamChunk, ToolDefinition, ToolResult,
};
use crate::domain::ports::{
    ApprovalPersistencePort, AuthStorePort, ChannelPort, ContextPort, MemoryPort, PersonaPort,
    RecallProviderPort, SchedulerPort, SecurityPort, SessionPort, StoragePort, StreamingProvider,
    ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::approval_runtime::SessionApprovalSet;
use tokio_util::sync::CancellationToken;

// ── StreamingProvider ────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpProvider;

#[async_trait]
impl StreamingProvider for NoOpProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        Err(ProviderError::Other(
            "NoOp provider: no provider configured".into(),
        ))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "noop".to_string()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
        Ok(crate::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }

    fn provider_descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            provider_id: "noop".to_string(),
            healthy: true,
            model_count: 0,
            display_name: "NoOp".to_string(),
        }
    }
}

// ── StoragePort ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpStorage;

#[async_trait]
impl StoragePort for NoOpStorage {
    async fn save_conversation(&self, _conv: &Conversation) -> Result<(), StorageError> {
        Ok(()) // silently discard
    }

    async fn load_conversation(&self, _id: &str) -> Result<Option<Conversation>, StorageError> {
        Ok(None)
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
        Ok(vec![])
    }
}

// ── SecurityPort ────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpSecurity;

#[async_trait]
impl SecurityPort for NoOpSecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        Ok(()) // allow all
    }

    fn check_workspace_access(
        &self,
        _path: &Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        Ok(PathAccessType::Workspace)
    }

    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Normal
    }

    fn set_mode(&self, _mode: PermissionMode) {
        // no-op
    }
}

// ── ToolSetPort ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpToolSet;

#[async_trait]
impl ToolSetPort for NoOpToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    async fn execute(
        &self,
        tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::NotFound(tool_name.into()))
    }
}

// ── PersonaPort ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpPersona;

impl PersonaPort for NoOpPersona {
    fn system_prompt(&self, _workspace_path: &Path) -> String {
        String::new()
    }
}

// ── MemoryPort ──────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpMemory;

impl MemoryPort for NoOpMemory {}

// ── RecallProviderPort (Story 12.1c AC4) ────────────────────────
//
// The default offline recall provider — an EXPLICIT no-op (not an absence). The
// daemon's `SessionBoundary` seam invokes `on_session_end` unconditionally so the
// call site is always exercised (12.1b "prove the seam is called" lesson); this
// impl makes the no-op explicit rather than faking work. It deliberately does NOT
// short-circuit on an empty transcript — the emptiness is the headless daemon's
// missing message source (Story 12.2), not a reason to skip; baking in
// `if empty { return }` here would later mask a real bug.

#[derive(Debug, Default)]
pub struct NoopRecallProvider;

#[async_trait]
impl RecallProviderPort for NoopRecallProvider {
    async fn on_session_end(
        &self,
        _transcript: &[crate::domain::models::ChatMessage],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Explicit no-op: no external recall backend in the default (offline)
        // composition. Zero side-effects, never panics.
        Ok(())
    }
}

// ── SessionPort ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpSession;

impl SessionPort for NoOpSession {}

// ── ChannelPort ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpChannel;

impl ChannelPort for NoOpChannel {}

// ── SchedulerPort ───────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpScheduler;

impl SchedulerPort for NoOpScheduler {}

// ── ContextPort ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpContext;

impl ContextPort for NoOpContext {}

// ── UsageLedgerPort ─────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpUsageLedger;

#[async_trait]
impl UsageLedgerPort for NoOpUsageLedger {
    async fn append(&self, _entry: UsageLedgerEntry) -> Result<(), StorageError> {
        Ok(())
    }

    async fn read_session(&self, _session_id: &str) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        Ok(Vec::new())
    }

    async fn read_since(&self, _since_unix_ms: i64) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        Ok(Vec::new())
    }
}

// ── ApprovalPersistencePort ─────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpApprovalPersistence;

#[async_trait]
impl ApprovalPersistencePort for NoOpApprovalPersistence {
    async fn load(
        &self,
    ) -> Result<SessionApprovalSet, crate::domain::errors::ApprovalPersistenceError> {
        Ok(SessionApprovalSet::default())
    }

    async fn save(
        &self,
        _scope: ApprovalScope,
    ) -> Result<(), crate::domain::errors::ApprovalPersistenceError> {
        Ok(())
    }
}

// ── AuthStorePort ───────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpAuthStore;

#[async_trait]
impl AuthStorePort for NoOpAuthStore {
    async fn get(&self, _provider: &str) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
    async fn set(&self, _provider: &str, _cred: Credential) -> Result<(), AuthError> {
        Ok(())
    }
    async fn remove(&self, _provider: &str) -> Result<(), AuthError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<ProviderStatus>, AuthError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_channel_start_shutdown_are_noop() {
        let channel = NoOpChannel;
        assert!(channel.start_loop().await.is_ok());
        assert!(channel.shutdown_loop().await.is_ok());
    }
}
