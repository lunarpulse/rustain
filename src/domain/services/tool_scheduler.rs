//! ToolScheduler — phased pipeline (Validate → Schedule → Await Approval → Execute → Emit)
//! with a serialisable 7-state `ToolCall` discriminated-union FSM.
//!
//! # Phase legality
//!
//! Legal transition sequences:
//! - Happy path (auto-approved): `Validating → Scheduled → Executing → Success`
//! - Happy path (with approval): `Validating → Scheduled → AwaitingApproval → Executing → Success`
//! - Schema invalid: `Validating → Error` (no Scheduled emitted)
//! - Policy denial (no prompt): `Validating → Scheduled → Error`
//! - User rejection: `Validating → Scheduled → AwaitingApproval → Error`
//! - Cancellation: from any non-terminal phase → `Cancelled`
//!
//! Any other sequence is illegal and will fail the conformance test suite.

use std::sync::Arc;

use futures::stream::{FuturesOrdered, StreamExt as _};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::domain::models::ActiveSkill;
use crate::domain::models::tool_call::{
    ApprovalSource, ToolCall, ToolCallRequest, ToolCallResult, ToolCallTransition,
};
use crate::domain::events::ToolProgressEvent;
use crate::domain::ports::{SecurityPort, ToolSetPort};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::permission_chain;
use std::path::PathBuf;
use tokio::sync::{mpsc, RwLock};

/// Schedules and executes tool calls with lifecycle event broadcast.
pub struct ToolScheduler {
    security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    events: broadcast::Sender<ToolCallTransition>,
    approval_runtime: Arc<ApprovalRuntime>,
    plan_file: RwLock<Option<PathBuf>>,
    progress_tx: RwLock<Option<mpsc::UnboundedSender<ToolProgressEvent>>>,
}

impl ToolScheduler {
    /// Construct a new scheduler.
    ///
    /// `event_capacity` defaults to **1024** when the caller passes `0`.
    pub fn new(
        security: Arc<dyn SecurityPort>,
        tools: Arc<dyn ToolSetPort>,
        approval_runtime: Arc<ApprovalRuntime>,
        event_capacity: usize,
    ) -> Arc<Self> {
        let cap = if event_capacity == 0 {
            tracing::warn!("ToolScheduler event_capacity was 0, falling back to 1024");
            1024
        } else {
            event_capacity
        };
        let (events, _) = broadcast::channel(cap);
        Arc::new(Self {
            security,
            tools,
            events,
            approval_runtime,
            plan_file: RwLock::new(None),
            progress_tx: RwLock::new(None),
        })
    }

    /// Subscribe to transition events.  Receivers see events sent *after* the
    /// call to `subscribe` (standard `tokio::sync::broadcast` tail semantics).
    pub fn subscribe(&self) -> broadcast::Receiver<ToolCallTransition> {
        self.events.subscribe()
    }

    /// Set the plan file path for the current conversation.
    /// Used by the turn orchestrator to thread the plan-file exception through the chain.
    pub async fn set_plan_file(&self, path: Option<PathBuf>) {
        *self.plan_file.write().await = path;
    }

    /// Set the progress channel sender for tool progress events.
    /// Story 16.9 — called at startup when `tool_progress.live_tail` is enabled.
    pub async fn set_progress_tx(&self, tx: Option<mpsc::UnboundedSender<ToolProgressEvent>>) {
        *self.progress_tx.write().await = tx;
    }

    /// Run a batch of tool calls through the scheduler.
    ///
    /// If every tool in the batch is `parallel_safe`, the calls execute
    /// concurrently via `FuturesOrdered` while preserving input order in the
    /// returned `Vec<ToolCall>`.  Otherwise the batch falls back to sequential
    /// execution.
    pub async fn schedule(
        self: Arc<Self>,
        source: ApprovalSource,
        batch: Vec<ToolCallRequest>,
        cancel: CancellationToken,
        active_skills: Option<&[ActiveSkill]>,
    ) -> Vec<ToolCall> {
        let all_parallel = batch
            .iter()
            .all(|req| self.tools.is_parallel_safe(&req.tool_name));
        let active_owned: Option<Vec<ActiveSkill>> = active_skills.map(|s| s.to_vec());

        if all_parallel {
            let mut futures = FuturesOrdered::new();
            for req in batch {
                let s = self.clone();
                let src = source.clone();
                let c = cancel.child_token();
                let active = active_owned.clone();
                futures.push_back(async move { s.run_one(src, req, c, active).await });
            }
            let mut out = Vec::with_capacity(futures.len());
            while let Some(call) = futures.next().await {
                out.push(call);
            }
            out
        } else {
            let mut out = Vec::with_capacity(batch.len());
            for req in batch {
                let c = cancel.child_token();
                out.push(
                    self.clone()
                        .run_one(source.clone(), req, c, active_owned.clone())
                        .await,
                );
            }
            out
        }
    }

    /// Execute a single tool call through the full phase pipeline.
    async fn run_one(
        self: Arc<Self>,
        source: ApprovalSource,
        req: ToolCallRequest,
        cancel: CancellationToken,
        active_skills: Option<Vec<ActiveSkill>>,
    ) -> ToolCall {
        let id = req.id.clone();
        let conversation_id = source.conversation_id().to_string();
        let now_secs = chrono::Utc::now().timestamp();

        // Phase 1: Validating
        let call = ToolCall::Validating {
            id: id.clone(),
            request: req.clone(),
            started_at: now_secs,
        };
        self.emit(&conversation_id, &call);
        if let Err(e) = self.tools.validate_input(&req.tool_name, &req.input) {
            return self.terminal(
                &conversation_id,
                ToolCall::Error {
                    id,
                    request: req,
                    error: e.to_string(),
                },
            );
        }
        if cancel.is_cancelled() {
            return self.terminal(
                &conversation_id,
                ToolCall::Cancelled {
                    id,
                    request: req,
                    reason: "pre-schedule".into(),
                },
            );
        }

        // Phase 2: Scheduled
        let call = ToolCall::Scheduled {
            id: id.clone(),
            request: req.clone(),
        };
        self.emit(&conversation_id, &call);

        // Phase 3: Permission chain check
        let plan_file = self.plan_file.read().await.clone();
        let decision_fut = permission_chain::check(
            self.security.as_ref(),
            &req.tool_name,
            &req.input,
            active_skills.as_deref(),
            plan_file.as_deref(),
        );
        let decision = tokio::select! {
            d = decision_fut => d,
            _ = cancel.cancelled() => {
                self.approval_runtime.cancel_by_source(&source, crate::domain::services::approval_runtime::CancelReason::SourceAborted).await;
                return self.terminal(&conversation_id, ToolCall::Cancelled {
                    id, request: req,
                    reason: "cancelled-during-policy".into(),
                });
            }
        };
        use permission_chain::PermissionDecision::*;
        match decision {
            Deny(reason) => {
                return self.terminal(
                    &conversation_id,
                    ToolCall::Error {
                        id,
                        request: req,
                        error: reason,
                    },
                );
            }
            Allow => { /* proceed */ }
            Prompt {
                server_id,
                path_hint,
            } => {
                let risk = crate::domain::models::risk_for_builtin(&req.tool_name);

                let (approval_id, rx) = self
                    .approval_runtime
                    .request(
                        source.clone(),
                        req.tool_name.clone(),
                        req.input.clone(),
                        risk,
                        server_id.as_deref(),
                        path_hint.as_deref(),
                    )
                    .await;

                if let Some(ref real_id) = approval_id {
                    let call = ToolCall::AwaitingApproval {
                        id: id.clone(),
                        request: req.clone(),
                        approval_id: real_id.clone(),
                    };
                    self.emit(&conversation_id, &call);
                }

                let outcome = tokio::select! {
                    o = rx => match o {
                        Ok(o) => o,
                        Err(_) => {
                            return self.terminal(&conversation_id, ToolCall::Cancelled {
                                id, request: req, reason: "approval-channel-closed".into()
                            });
                        }
                    },
                    _ = cancel.cancelled() => {
                        self.approval_runtime.cancel_by_source(&source, crate::domain::services::approval_runtime::CancelReason::SourceAborted).await;
                        return self.terminal(&conversation_id, ToolCall::Cancelled {
                            id, request: req, reason: "cancelled-during-approval".into()
                        });
                    }
                };

                match outcome {
                    crate::domain::models::ApprovalOutcome::Once
                    | crate::domain::models::ApprovalOutcome::AlwaysTool { .. }
                    | crate::domain::models::ApprovalOutcome::AlwaysServer { .. }
                    | crate::domain::models::ApprovalOutcome::AlwaysAndSave { .. } => { /* proceed */
                    }
                    crate::domain::models::ApprovalOutcome::Reject { feedback } => {
                        let error = match feedback {
                            Some(text) => permission_chain::format_feedback_message(&text),
                            None => "Permission denied by user".to_string(),
                        };
                        return self.terminal(
                            &conversation_id,
                            ToolCall::Error {
                                id,
                                request: req,
                                error,
                            },
                        );
                    }
                    crate::domain::models::ApprovalOutcome::Cancel => {
                        return self.terminal(
                            &conversation_id,
                            ToolCall::Cancelled {
                                id,
                                request: req,
                                reason: "user-cancel".into(),
                            },
                        );
                    }
                }
            }
        }

        // Phase 4: Executing
        let started_ms = chrono::Utc::now().timestamp_millis();
        let call = ToolCall::Executing {
            id: id.clone(),
            request: req.clone(),
            started_at: now_secs,
        };
        self.emit(&conversation_id, &call);

        let progress_tx = self.progress_tx.read().await.clone();
        let exec = tokio::select! {
            r = self.tools.execute_with_id(&req.tool_name, &req.id, req.input.clone(), cancel.clone(), progress_tx) => r,
            _ = cancel.cancelled() => {
                return self.terminal(&conversation_id, ToolCall::Cancelled {
                    id, request: req, reason: "cancelled-during-execute".into()
                });
            }
        };
        let duration_ms = (chrono::Utc::now().timestamp_millis() - started_ms).max(0) as u64;

        // Phase 5: Terminal
        match exec {
            Ok(out) => self.terminal(
                &conversation_id,
                ToolCall::Success {
                    id,
                    request: req,
                    result: ToolCallResult {
                        output: out.content,
                        is_error: out.is_error,
                        duration_ms,
                    },
                },
            ),
            Err(e) => self.terminal(
                &conversation_id,
                ToolCall::Error {
                    id,
                    request: req,
                    error: e.to_string(),
                },
            ),
        }
    }

    fn emit(&self, conversation_id: &str, call: &ToolCall) {
        let _ = self.events.send(ToolCallTransition {
            conversation_id: conversation_id.to_string(),
            call: call.clone(),
        });
    }

    fn terminal(&self, conversation_id: &str, call: ToolCall) -> ToolCall {
        self.emit(conversation_id, &call);
        call
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::ToolError;
    use crate::domain::models::{ToolDefinition, ToolResult};
    use async_trait::async_trait;
    use std::time::Duration;

    struct MockSecurity {
        mode: crate::domain::models::PermissionMode,
    }

    #[async_trait]
    impl SecurityPort for MockSecurity {
        fn current_mode(&self) -> crate::domain::models::PermissionMode {
            self.mode
        }

        fn check_blocklist(
            &self,
            _command: &str,
        ) -> Result<(), crate::domain::errors::PermissionError> {
            Ok(())
        }

        fn check_workspace_access(
            &self,
            _path: &std::path::Path,
            _op: crate::domain::models::FileOperation,
        ) -> Result<crate::domain::models::PathAccessType, crate::domain::errors::PermissionError>
        {
            Ok(crate::domain::models::PathAccessType::Workspace)
        }

        fn set_mode(&self, _mode: crate::domain::models::PermissionMode) {}
    }

    struct MockToolSet {
        parallel_safe: bool,
        delay_ms: u64,
    }

    #[async_trait]
    impl ToolSetPort for MockToolSet {
        fn available_tools(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "Mock".to_string(),
                description: "mock".to_string(),
                input_schema: serde_json::json!({}),
                parallel_safe: self.parallel_safe,
            }]
        }

        async fn execute(
            &self,
            _tool_name: &str,
            _input: serde_json::Value,
            _cancel: CancellationToken,
        ) -> Result<ToolResult, ToolError> {
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            Ok(ToolResult {
                tool_use_id: String::new(),
                content: "ok".to_string(),
                is_error: false,
            })
        }
    }

    fn make_scheduler(
        mode: crate::domain::models::PermissionMode,
        parallel_safe: bool,
        delay_ms: u64,
    ) -> Arc<ToolScheduler> {
        let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity { mode });
        let tools: Arc<dyn ToolSetPort> = Arc::new(MockToolSet {
            parallel_safe,
            delay_ms,
        });
        let approval_runtime =
            ApprovalRuntime::new(16, Arc::new(crate::adapters::noop::NoOpApprovalPersistence));
        ToolScheduler::new(security, tools, approval_runtime, 16)
    }

    #[tokio::test]
    async fn run_one_happy_path_yolo() {
        let sched = make_scheduler(crate::domain::models::PermissionMode::Yolo, false, 0);
        let req = ToolCallRequest {
            id: "t1".into(),
            tool_name: "Mock".into(),
            input: serde_json::json!({}),
        };
        let mut rx = sched.subscribe();
        let result = sched
            .clone()
            .run_one(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                req,
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(result.is_terminal());
        assert!(matches!(result, ToolCall::Success { .. }));

        // Collect broadcast transitions
        let mut transitions = vec![];
        while let Ok(t) = rx.try_recv() {
            transitions.push(t.call);
        }
        assert!(
            matches!(
                &transitions[..],
                [
                    ToolCall::Validating { .. },
                    ToolCall::Scheduled { .. },
                    ToolCall::Executing { .. },
                    ToolCall::Success { .. }
                ]
            ),
            "unexpected transitions: {:?}",
            transitions
        );
    }

    #[tokio::test]
    async fn run_one_cancel_pre_schedule() {
        let sched = make_scheduler(crate::domain::models::PermissionMode::Yolo, false, 0);
        let req = ToolCallRequest {
            id: "t1".into(),
            tool_name: "Mock".into(),
            input: serde_json::json!({}),
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = sched
            .clone()
            .run_one(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                req,
                cancel,
                None,
            )
            .await;
        assert!(matches!(result, ToolCall::Cancelled { reason, .. } if reason == "pre-schedule"));
    }

    #[tokio::test]
    async fn run_one_cancel_during_execute() {
        let sched = make_scheduler(crate::domain::models::PermissionMode::Yolo, false, 500);
        let req = ToolCallRequest {
            id: "t1".into(),
            tool_name: "Mock".into(),
            input: serde_json::json!({}),
        };
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let sched2 = sched.clone();
        let handle = tokio::spawn(async move {
            sched2
                .run_one(
                    ApprovalSource::ForegroundTurn {
                        conversation_id: "c1".into(),
                    },
                    req,
                    cancel2,
                    None,
                )
                .await
        });
        // Give it time to reach Executing
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let result = handle.await.unwrap();
        assert!(
            matches!(result, ToolCall::Cancelled { reason, .. } if reason == "cancelled-during-execute")
        );
    }

    #[tokio::test]
    async fn scheduler_parallel_read_timing() {
        let sched = make_scheduler(crate::domain::models::PermissionMode::Yolo, true, 50);
        let batch: Vec<ToolCallRequest> = (0..3)
            .map(|i| ToolCallRequest {
                id: format!("t{}", i),
                tool_name: "Mock".into(),
                input: serde_json::json!({}),
            })
            .collect();
        let start = std::time::Instant::now();
        let results = sched
            .schedule(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                batch,
                CancellationToken::new(),
                None,
            )
            .await;
        let elapsed = start.elapsed();
        assert_eq!(results.len(), 3);
        assert!(
            elapsed < Duration::from_millis(100),
            "parallel batch took {:?}, expected < 100 ms",
            elapsed
        );
    }

    #[tokio::test]
    async fn scheduler_mixed_batch_sequential() {
        // One tool parallel_safe=false forces sequential fallback
        let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
            mode: crate::domain::models::PermissionMode::Yolo,
        });
        let tools: Arc<dyn ToolSetPort> = Arc::new(MockToolSet {
            parallel_safe: false,
            delay_ms: 50,
        });
        let approval_runtime =
            ApprovalRuntime::new(16, Arc::new(crate::adapters::noop::NoOpApprovalPersistence));
        let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
        let batch: Vec<ToolCallRequest> = (0..3)
            .map(|i| ToolCallRequest {
                id: format!("t{}", i),
                tool_name: "Mock".into(),
                input: serde_json::json!({}),
            })
            .collect();
        let start = std::time::Instant::now();
        let results = sched
            .schedule(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                batch,
                CancellationToken::new(),
                None,
            )
            .await;
        let elapsed = start.elapsed();
        assert_eq!(results.len(), 3);
        assert!(
            elapsed >= Duration::from_millis(130),
            "sequential batch took {:?}, expected >= 130 ms",
            elapsed
        );
    }
}
