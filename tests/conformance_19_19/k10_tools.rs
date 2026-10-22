use super::*;
use futures::StreamExt;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::sandbox::NoOpSandbox;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::errors::{PermissionError, ProviderError};
use rustain::domain::models::{FileOperation, PathAccessType, PermissionMode, SandboxPolicy};
use rustain::domain::ports::{
    ProbeOutcome, SandboxManager, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::RwLock;

#[derive(Default)]
struct ToolProvider {
    calls: AtomicUsize,
    requests: parking_lot::Mutex<Vec<(Vec<Message>, CompletionOptions)>>,
    local_needle: String,
    managed_path: String,
    local_path: String,
    terminal: ToolTerminal,
}

pub(super) fn evidence_provider(managed_path: String) -> Arc<dyn StreamingProvider> {
    Arc::new(ToolProvider {
        managed_path,
        ..Default::default()
    })
}

#[derive(Clone, Copy, Debug, Default)]
enum ToolTerminal {
    #[default]
    Complete,
    Error,
    Cancelled,
    Eof,
}

#[async_trait::async_trait]
impl StreamingProvider for ToolProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        self.requests.lock().push((messages, options));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let chunks = match call {
            0 => vec![
                StreamChunk::ToolUse {
                    id: "managed-read".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": self.managed_path}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            1 => match self.terminal {
                ToolTerminal::Complete => vec![
                    StreamChunk::Text {
                        content: "draft finished".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
                ToolTerminal::Error => vec![StreamChunk::Error {
                    content: "scripted provider failure".into(),
                }],
                ToolTerminal::Cancelled => vec![StreamChunk::TurnComplete {
                    stop_reason: StopReason::Cancelled,
                }],
                ToolTerminal::Eof => Vec::new(),
            },
            2 => vec![
                StreamChunk::ToolUse {
                    id: "local-write".into(),
                    name: "Write".into(),
                    input: serde_json::json!({"file_path": self.local_path, "content": self.local_needle}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            _ => vec![
                StreamChunk::Text {
                    content: "operator turn finished".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        };
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "k10-managed-tool-trace".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

struct YoloSecurity;

#[async_trait::async_trait]
impl SecurityPort for YoloSecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        Ok(PathAccessType::Workspace)
    }
    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Yolo
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

pub(super) fn actual_tool_runtime(
    workspace: &std::path::Path,
    storage: Arc<dyn StoragePort>,
    provider: Arc<dyn StreamingProvider>,
) -> Arc<DaemonTurnRuntime> {
    let security: Arc<dyn SecurityPort> = Arc::new(YoloSecurity);
    let fs_storage = Arc::new(FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(workspace),
        workspace.to_path_buf(),
    ));
    let tools: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
        workspace.to_path_buf(),
        fs_storage,
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>
        )),
        Arc::new(RwLock::new(SandboxPolicy::Permissive)),
    ));
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
    Arc::new(DaemonTurnRuntime {
        recipient_context: Arc::new(
            rustain::adapters::daemon::recipient_context::RecipientContextState::default(),
        ),
        provider,
        app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        security,
        tools,
        tool_scheduler,
        persona: Arc::new(NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(None)),
        context: Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::noop::NoOpContext)
                as Arc<dyn rustain::domain::ports::ContextPort>,
        )),
        storage,
        fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(
            rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
        ),
        approval,
        workspace: workspace.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    })
}

async fn await_peer_tool_display(writer: &mut tokio::net::UnixStream, needle: &str) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let (_, history) = history_fence(writer).await;
        if history.iter().any(|message| {
            message.tool_calls.iter().any(|tool| {
                tool.result
                    .as_ref()
                    .is_some_and(|result| result.content.contains(needle))
            })
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "actual peer tool evidence never reached durable display: {history:?}"
        );
        tokio::task::yield_now().await;
    }
}

struct PermissionProvider {
    calls: AtomicUsize,
    requests: parking_lot::Mutex<Vec<(Vec<Message>, CompletionOptions)>>,
    tool_call_number: usize,
    path: String,
    content: String,
}

#[async_trait::async_trait]
impl StreamingProvider for PermissionProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        self.requests.lock().push((messages, options));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let chunks = if call == self.tool_call_number {
            vec![
                StreamChunk::ToolUse {
                    id: "controlled-write".into(),
                    name: "Write".into(),
                    input: serde_json::json!({"file_path": self.path, "content": self.content}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![
                StreamChunk::Text {
                    content: "draft completed".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "k10-controlled-write".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

async fn select_context_item(stream: &mut tokio::net::UnixStream, item_id: &str) {
    write_frame(
        stream,
        &ClientFrame::SetRecipientContext {
            item_id: item_id.to_owned(),
            selected: true,
        },
    )
    .await
    .unwrap();
    loop {
        match socket_frame(stream).await {
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::ContextSelect,
                target: RecipientActionTarget::Item { item_id: received },
                outcome:
                    RecipientActionOutcome::ContextSelection {
                        selected: true,
                        changed: true,
                    },
            } if received == item_id => break,
            DaemonFrame::Error(error) => panic!("actual context selection refused: {error:?}"),
            _ => {}
        }
    }
}

type PermissionProviderSlot = Arc<parking_lot::Mutex<Option<Arc<PermissionProvider>>>>;
type PermissionRuntimeSlot = Arc<parking_lot::Mutex<Option<Arc<DaemonTurnRuntime>>>>;

async fn k10_permission_host(
    tool_call_number: usize,
    file_name: &str,
    content: &str,
) -> (Host, PermissionProviderSlot, PermissionRuntimeSlot) {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    let provider_slot = Arc::new(parking_lot::Mutex::new(None::<Arc<PermissionProvider>>));
    let slot = provider_slot.clone();
    let runtime_slot = Arc::new(parking_lot::Mutex::new(None::<Arc<DaemonTurnRuntime>>));
    let runtime_capture = runtime_slot.clone();
    let file_name = file_name.to_owned();
    let content = content.to_owned();
    options.runtime_factory = Some(Arc::new(move |workspace, storage, _| {
        let provider = Arc::new(PermissionProvider {
            calls: AtomicUsize::new(0),
            requests: parking_lot::Mutex::new(Vec::new()),
            tool_call_number,
            path: file_name.clone(),
            content: content.clone(),
        });
        *slot.lock() = Some(provider.clone());
        let runtime =
            actual_tool_runtime(workspace, storage, provider as Arc<dyn StreamingProvider>);
        *runtime_capture.lock() = Some(runtime.clone());
        runtime
    }));
    let host = Host::serve_with(options).await;
    (host, provider_slot, runtime_slot)
}

#[tokio::test]
async fn k10_peer_context_taint_prompts_yolo_write_and_refusal_preserves_canary() {
    let canary = b"canary-before".to_vec();
    let content = "must-not-overwrite-canary";

    // Untainted control: the same real Write executes under the same Yolo
    // runtime without producing an approval request.
    let (mut clear_host, clear_slot, _) = k10_permission_host(0, "yolo-canary.txt", content).await;
    let clear_path = clear_host.journal.path.join("yolo-canary.txt");
    std::fs::write(&clear_path, &canary).unwrap();
    let clear_socket = SocketService::start(&mut clear_host).await;
    let (mut clear_writer, _, _) = clear_socket
        .attach(ConnectionTier::TrustedLocal, false)
        .await;
    attach_user_turn(&mut clear_writer, "execute the ordinary local write").await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    let clear_provider = loop {
        if let Some(provider) = clear_slot.lock().clone() {
            break provider;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "untainted runtime did not initialize"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    while clear_provider.requests.lock().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "untainted Write did not execute and return to provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(std::fs::read(&clear_path).unwrap(), content.as_bytes());

    // Tainted case: real HTTP receipt -> journal-derived context selection ->
    // attached operator turn -> production scheduler/permission chain.
    let (mut tainted_host, tainted_slot, runtime_slot) =
        k10_permission_host(1, "yolo-canary.txt", content).await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    let tainted_path = tainted_host.journal.path.join("yolo-canary.txt");
    std::fs::write(&tainted_path, &canary).unwrap();
    let socket = SocketService::start(&mut tainted_host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let item_body = "peer-origin write directive for taint differential 19-19";
    let mut sender = new_sender(vec![tainted_host.peer("recipient")]).await;
    let mut tui = TuiState::new(130, 30);
    sender
        .type_command(
            &mut tui,
            &format!("/team send --type consultation recipient {item_body}"),
        )
        .await;
    settle_send(&mut sender, &mut tui).await;
    let item_id = received(&room_events(&tainted_host.journal).await)
        .into_iter()
        .find(|(_, _, body)| body == item_body)
        .map(|(id, _, _)| id)
        .expect("actual HTTP arrival supplied journal-derived item id");
    let tainted_provider = loop {
        if let Some(provider) = tainted_slot.lock().clone() {
            break provider;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tainted runtime did not initialize"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // The managed draft request is first and does not request a tool.
    while tainted_provider.requests.lock().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "inbound draft did not reach real provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let runtime = runtime_slot
        .lock()
        .clone()
        .expect("actual inbound built the production runtime");
    let mut transitions = runtime.tool_scheduler.subscribe();
    select_context_item(&mut writer, &item_id).await;
    attach_user_turn(
        &mut writer,
        "perform the same write after explicit peer adoption",
    )
    .await;
    let request_id = tokio::time::timeout(SETTLE, async {
        // Frames are read to completion (never cancelled mid-frame); scheduler
        // observations are drained between frames.
        loop {
            let frame = socket_frame(&mut writer).await;
            while let Ok(transition) = transitions.try_recv() {
                match transition.call {
                    rustain::domain::models::ToolCall::Executing { request, .. }
                    | rustain::domain::models::ToolCall::Success { request, .. }
                        if request.tool_name == "Write" =>
                    {
                        panic!("peer-tainted Write executed without approval");
                    }
                    _ => {}
                }
            }
            match frame {
                DaemonFrame::ApprovalRequest {
                    request_id, tool, ..
                } => {
                    assert_eq!(tool, "Write");
                    break request_id;
                }
                DaemonFrame::Error(error) => panic!("unexpected daemon refusal: {error:?}"),
                _ => {}
            }
        }
    })
    .await
    .expect("peer-tainted Write reaches actual approval or execution");
    write_frame(
        &mut writer,
        &ClientFrame::ApprovalResponse {
            request_id,
            outcome: rustain::domain::models::ApprovalOutcome::Reject { feedback: None },
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(SETTLE, async {
        loop {
            match transitions
                .recv()
                .await
                .expect("real scheduler observation remains open")
                .call
            {
                rustain::domain::models::ToolCall::Error { request, .. }
                | rustain::domain::models::ToolCall::Cancelled { request, .. }
                    if request.tool_name == "Write" =>
                {
                    break;
                }
                rustain::domain::models::ToolCall::Executing { request, .. }
                | rustain::domain::models::ToolCall::Success { request, .. }
                    if request.tool_name == "Write" =>
                {
                    panic!("refused Write reached execution");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("actual refused Write reaches a terminal scheduler state");
    let operator_request = serde_json::to_string(&tainted_provider.requests.lock()[1]).unwrap();
    assert!(
        operator_request.contains(item_body),
        "actual selected journal item is present in the real operator request"
    );
    assert_eq!(
        std::fs::read(&tainted_path).unwrap(),
        canary,
        "refusal leaves the actual filesystem canary byte-identical"
    );
}

#[tokio::test]
async fn k10_managed_draft_tool_result_stays_peer_display_and_local_tool_result_stays_local() {
    let managed_needle = "managed-tool-result-19-19-unique".to_owned();
    let local_needle = "ordinary-local-tool-result-19-19-unique".to_owned();
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    let provider_slot = Arc::new(parking_lot::Mutex::new(None::<Arc<ToolProvider>>));
    let provider_for_factory = provider_slot.clone();
    let managed = managed_needle.clone();
    let local = local_needle.clone();
    options.runtime_factory = Some(Arc::new(move |workspace, storage, _default_provider| {
        let managed_path = workspace.join("managed.txt");
        std::fs::write(&managed_path, &managed).expect("create peer-visible Read fixture");
        let provider = Arc::new(ToolProvider {
            local_needle: local.clone(),
            managed_path: managed_path.to_string_lossy().into_owned(),
            local_path: workspace.join("local.txt").to_string_lossy().into_owned(),
            ..Default::default()
        });
        *provider_for_factory.lock() = Some(provider.clone());
        actual_tool_runtime(workspace, storage, provider as Arc<dyn StreamingProvider>)
    }));
    let mut host = Host::serve_with(options).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let mut sender = new_sender(vec![host.peer("recipient")]).await;
    let mut tui = TuiState::new(130, 30);
    let question = "managed inbound question 19-19 asks for a real tool action";
    sender
        .type_command(
            &mut tui,
            &format!("/team send --type consultation recipient {question}"),
        )
        .await;
    settle_send(&mut sender, &mut tui).await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    let provider = loop {
        if let Some(provider) = provider_slot.lock().clone() {
            break provider;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "managed runtime did not initialize"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let deadline = tokio::time::Instant::now() + SETTLE;
    while provider.requests.lock().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "managed peer draft did not invoke its real tool and continue"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let draft_tool_followup = serde_json::to_string(&provider.requests.lock()[1]).unwrap();
    assert!(
        draft_tool_followup.contains(question),
        "inbound question reaches its authorized draft invocation"
    );
    assert!(
        draft_tool_followup.contains(&managed_needle),
        "actual builtin Read result reaches the managed draft provider request"
    );
    assert_eq!(
        std::fs::read_to_string(host.journal.path.join("managed.txt")).unwrap(),
        managed_needle
    );
    await_peer_tool_display(&mut writer, &managed_needle).await;

    attach_user_turn(
        &mut writer,
        "ordinary operator followup after managed peer tool",
    )
    .await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    while provider.requests.lock().len() < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "operator turn did not reach actual ScriptedProvider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let operator_request = serde_json::to_string(&provider.requests.lock()[2]).unwrap();
    assert!(operator_request.contains("ordinary operator followup after managed peer tool"));
    assert!(
        !operator_request.contains(&managed_needle),
        "managed peer ToolResult must not contaminate operator request: {operator_request}"
    );

    let deadline = tokio::time::Instant::now() + SETTLE;
    while provider.requests.lock().len() < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "ordinary local Write did not return to provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let local_result = provider.requests.lock()[3]
        .0
        .iter()
        .flat_map(|message| &message.tool_results)
        .find(|result| result.tool_use_id == "local-write")
        .expect("actual local Write returns a tool result")
        .content
        .clone();
    assert_eq!(
        std::fs::read_to_string(host.journal.path.join("local.txt")).unwrap(),
        local_needle
    );
    attach_user_turn(
        &mut writer,
        "another ordinary operator turn after local tool completion",
    )
    .await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    while provider.requests.lock().len() < 5 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "completed local tool history did not reach the next operator request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let requests = provider.requests.lock();
    let next_local_messages = &requests[4].0;
    assert!(
        next_local_messages
            .iter()
            .any(|message| message.content.contains(&local_result)
                && message
                    .content
                    .contains(host.journal.path.join("local.txt").to_str().unwrap())),
        "actual completed Write result and destination remain in decoded operator history"
    );
    let next_local_request = serde_json::to_string(&requests[4]).unwrap();
    assert!(
        !next_local_request.contains(&managed_needle),
        "the managed peer display result remains excluded from later local history"
    );
}

#[tokio::test]
async fn k10_failed_cancelled_and_eof_managed_tool_turns_do_not_leak_into_operator_history() {
    for terminal in [
        ToolTerminal::Error,
        ToolTerminal::Cancelled,
        ToolTerminal::Eof,
    ] {
        let needle = match terminal {
            ToolTerminal::Error => "error-managed-tool-input-19-19",
            ToolTerminal::Cancelled => "cancel-managed-tool-input-19-19",
            ToolTerminal::Eof => "eof-managed-tool-input-19-19",
            ToolTerminal::Complete => unreachable!(),
        }
        .to_owned();
        let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
        let provider_slot = Arc::new(parking_lot::Mutex::new(None::<Arc<ToolProvider>>));
        let slot = provider_slot.clone();
        let content = needle.clone();
        options.runtime_factory = Some(Arc::new(move |workspace, storage, _| {
            let managed_path = workspace.join("managed.txt");
            std::fs::write(&managed_path, &content).expect("create peer-visible Read fixture");
            let provider = Arc::new(ToolProvider {
                local_needle: "local-write-after-failed-draft".into(),
                managed_path: managed_path.to_string_lossy().into_owned(),
                local_path: workspace.join("local.txt").to_string_lossy().into_owned(),
                terminal,
                ..Default::default()
            });
            *slot.lock() = Some(provider.clone());
            actual_tool_runtime(workspace, storage, provider as Arc<dyn StreamingProvider>)
        }));
        let mut host = Host::serve_with(options).await;
        let socket = SocketService::start(&mut host).await;
        let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
        let mut sender = new_sender(vec![host.peer("recipient")]).await;
        let mut tui = TuiState::new(130, 30);
        let question = format!("managed inbound failure-case request for {terminal:?}");
        sender
            .type_command(
                &mut tui,
                &format!("/team send --type consultation recipient {question}"),
            )
            .await;
        settle_send(&mut sender, &mut tui).await;
        let deadline = tokio::time::Instant::now() + SETTLE;
        let provider = loop {
            if let Some(provider) = provider_slot.lock().clone() {
                break provider;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "failed-turn runtime did not initialize"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        while provider.requests.lock().len() < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "managed {terminal:?} draft did not complete its real Write call"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let actual_tool_roundtrip = serde_json::to_string(&provider.requests.lock()[1]).unwrap();
        assert!(
            actual_tool_roundtrip.contains(&needle),
            "the real builtin Read result reached the managed provider before {terminal:?}"
        );
        await_peer_tool_display(&mut writer, &needle).await;

        attach_user_turn(&mut writer, "operator follow-up after failed peer draft").await;
        while provider.requests.lock().len() < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "operator turn did not reach provider after managed {terminal:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let operator_request = serde_json::to_string(&provider.requests.lock()[2]).unwrap();
        assert!(operator_request.contains("operator follow-up after failed peer draft"));
        assert!(
            !operator_request.contains(&needle),
            "managed tool evidence leaked after {terminal:?}: {operator_request}"
        );
    }
}
