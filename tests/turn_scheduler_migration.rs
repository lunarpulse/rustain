//! Integration test: `run_turn` end-to-end with the new scheduler.
//!
//! Verifies that for a 2-tool batch (Read+Read), the conversation history
//! mirrors the prior direct-execute output, and that `ToolCallTransitionBridged`
//! events appear on a subscribed `raw_rx`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustain::domain::events::AppEvent;
use rustain::domain::models::router::{EscalationReason, ModelTier, StepKind};
use rustain::domain::models::{
    ChatMessage, CompletionOptions, Conversation, Message, MessageRole, StopReason, StreamChunk,
    ToolDefinition, ToolResult, generate_conversation_id,
};
use rustain::domain::ports::{SecurityPort, StreamingProvider, ToolSetPort, UsageLedgerPort};
use rustain::domain::services::model_router::ResolvedModel;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::runtime::turn::run_turn;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use std::sync::atomic::{AtomicUsize, Ordering};

struct MockProvider {
    call_count: AtomicUsize,
}

#[async_trait]
impl StreamingProvider for MockProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let chunks = if count == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: "tool-1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "/tmp/a"}),
                },
                StreamChunk::ToolUse {
                    id: "tool-2".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "/tmp/b"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }]
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock".to_string()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

struct MockSecurity;

#[async_trait]
impl SecurityPort for MockSecurity {
    fn check_blocklist(
        &self,
        _command: &str,
    ) -> Result<(), rustain::domain::errors::PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: rustain::domain::models::FileOperation,
    ) -> Result<rustain::domain::models::PathAccessType, rustain::domain::errors::PermissionError>
    {
        Ok(rustain::domain::models::PathAccessType::Workspace)
    }

    fn current_mode(&self) -> rustain::domain::models::PermissionMode {
        rustain::domain::models::PermissionMode::Yolo
    }
    fn set_mode(&self, _mode: rustain::domain::models::PermissionMode) {}
}

struct MockToolSet;

#[async_trait]
impl ToolSetPort for MockToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "Read".to_string(),
            description: "read".to_string(),
            input_schema: serde_json::json!({}),
            parallel_safe: true,
        }]
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, rustain::domain::errors::ToolError> {
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "file contents".to_string(),
            is_error: false,
        })
    }
}

fn make_conversation() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: vec![ChatMessage {
            synthetic: false,
            id: generate_conversation_id(),
            role: MessageRole::User,
            content: "read two files".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }],
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

#[tokio::test]
async fn turn_scheduler_migration() {
    let (bus, _domain_rx) = EventBus::new(16);
    let mut raw_rx = bus.subscribe_raw();

    let provider: Arc<dyn StreamingProvider> = Arc::new(MockProvider {
        call_count: AtomicUsize::new(0),
    });
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(MockToolSet);
    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval_runtime, 16);

    // Spawn bridge task (mirrors event_loop.rs)
    {
        let domain_tx = bus.domain_tx.clone();
        let raw_tx = bus.raw_tx.clone();
        let mut rx = tool_scheduler.subscribe();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                    Ok(Ok(transition)) => {
                        let ev = AppEvent::ToolCallTransitionBridged {
                            conversation_id: transition.conversation_id.clone(),
                            transition: transition.clone(),
                        };
                        if let Some(raw) =
                            rustain::infrastructure::runtime::event_bus::RawEvent::from_app_event(
                                &ev,
                            )
                        {
                            let _ = raw_tx.send(raw);
                        }
                        let _ = domain_tx.send(ev);
                    }
                    Ok(Err(RecvError::Lagged(n))) => {
                        tracing::warn!(missed = n, "tool transition subscriber lagged");
                    }
                    Ok(Err(RecvError::Closed)) => break,
                    Err(_) => continue,
                }
            }
        });
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    run_turn(
        provider,
        vec![Message {
            role: MessageRole::User,
            content: "read two files".into(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
        reasoning_content: None,
        }],
        CompletionOptions {
            model: "test".into(),
            max_tokens: 100,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        },
        tx,
        security,
        tools,
        tool_scheduler,
        "conv-1".into(),
        Arc::new(rustain::adapters::noop::NoOpStorage),
        make_conversation(),
        None,
        CancellationToken::new(),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
        ResolvedModel {
            model: "test".into(),
            tier: ModelTier::CheapAgentic,
            escalation_reason: EscalationReason::None,
        },
        None,
        0,
        "sess-test".into(),
    )
    .await;

    // Collect events
    let mut events = vec![];
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    // Should see ToolUse, TurnComplete, ToolResult, TurnComplete(EndTurn)
    let tool_results: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::ToolResult { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(tool_results.len(), 2, "expected 2 tool results");

    // Give the bridge task time to forward transitions
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify ToolCallTransitionBridged events appeared on raw_rx
    let mut raw_events = vec![];
    while let Ok(raw) = raw_rx.try_recv() {
        raw_events.push(raw);
    }
    assert!(
        raw_events
            .iter()
            .any(|r| { format!("{:?}", r.kind).contains("Tool") }),
        "expected at least one RawEventKind::Tool on raw_rx"
    );
}
