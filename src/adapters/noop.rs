//! NoOp adapter stubs for all ports.
//! Used during startup before real adapters are wired, and in tests.

#![allow(dead_code)]

use std::path::Path;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::domain::errors::{PermissionError, ProviderError, StorageError, ToolError};
use crate::domain::models::{
    ApprovalDecision, CompletionOptions, Conversation, ConversationSummary, FileOperation, Message,
    PathAccessType, PermissionMode, StreamChunk, ToolDefinition, ToolResult,
};
use crate::domain::ports::{
    ChannelPort, ContextPort, MemoryPort, PersonaPort, ProviderPort, SchedulerPort, SecurityPort,
    SessionPort, StoragePort, ToolSetPort,
};

// ── ProviderPort ────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NoOpProvider;

#[async_trait]
impl ProviderPort for NoOpProvider {
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

    fn provider_id(&self) -> &str {
        "noop"
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

    async fn request_permission(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
    ) -> Result<ApprovalDecision, PermissionError> {
        Ok(ApprovalDecision::Allow)
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
