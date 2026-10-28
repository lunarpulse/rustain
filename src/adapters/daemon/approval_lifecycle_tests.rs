// Story 19.20c: included by server.rs's test module; production permission chain.
struct ExactLifecycleProvider {
    inner: ToolThenTextProvider,
    input: serde_json::Value,
    calls: mpsc::UnboundedSender<usize>,
    request_after_shutdown: bool,
    tool: &'static str,
    shutdown_probe_emitted: AtomicBool,
}

#[async_trait::async_trait]
impl StreamingProvider for ExactLifecycleProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        let queued_probe = self.request_after_shutdown
            && serde_json::to_string(&messages)
                .unwrap()
                .contains("queued turn must not open a new approval after shutdown");
        let stream = self.inner.stream_completion(messages, options).await?;
        let _ = self.calls.send(self.inner.calls.load(Ordering::SeqCst));
        let input = self.input.clone();
        if queued_probe && !self.shutdown_probe_emitted.swap(true, Ordering::SeqCst) {
            return Ok(futures::stream::iter(vec![
                StreamChunk::ToolUse {
                    id: "after-shutdown-tool".into(),
                    name: "Write".into(),
                    input,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ])
            .boxed());
        }
        let tool = self.tool;
        Ok(stream
            .map(move |chunk| match chunk {
                StreamChunk::ToolUse { id, .. } => StreamChunk::ToolUse {
                    id,
                    name: tool.into(),
                    input: input.clone(),
                },
                chunk => chunk,
            })
            .boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        self.inner.abort().await
    }
    fn provider_id(&self) -> String {
        "19-20c-exact-lifecycle".into()
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
}

/// Observe the real adapter rather than replacing its execution behavior.
struct CountExactTools {
    inner: Arc<dyn ToolSetPort>,
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolSetPort for CountExactTools {
    fn available_tools(&self) -> Vec<crate::domain::models::ToolDefinition> {
        self.inner.available_tools()
    }
    fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.inner.workspace_root()
    }
    fn validate_input(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<(), crate::domain::errors::ToolError> {
        self.inner.validate_input(tool, input)
    }
    async fn execute(
        &self,
        tool: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<crate::domain::models::ToolResult, crate::domain::errors::ToolError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(tool, input, cancel).await
    }
    async fn execute_invocation(
        &self,
        tool: &str,
        id: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
        progress: Option<mpsc::UnboundedSender<crate::domain::events::ToolProgressEvent>>,
        invocation: &crate::domain::models::ToolInvocation<'_>,
    ) -> Result<crate::domain::models::ToolResult, crate::domain::errors::ToolError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner
            .execute_invocation(tool, id, input, cancel, progress, invocation)
            .await
    }
}

struct ExactLifecycleHost {
    workspace: tempfile::TempDir,
    server: Arc<AttachServer>,
    runtime: Arc<DaemonTurnRuntime>,
    service: tokio::task::JoinHandle<()>,
    shutdown: CancellationToken,
    calls: mpsc::UnboundedReceiver<usize>,
    channel_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    input: serde_json::Value,
    executions: Arc<AtomicUsize>,
    factory_calls: Arc<AtomicUsize>,
    // Overflow isolates the deciding queue from unrelated event fanout. The
    // real turn still emits domain events and uses the real permission chain.
    _domain_rx: Option<mpsc::UnboundedReceiver<AppEvent>>,
}

impl ExactLifecycleHost {
    async fn start(content: String, queue_capacity: Option<usize>) -> Self {
        Self::start_with_options(content, queue_capacity, false, "Write").await
    }

    async fn start_with_options(
        content: String,
        queue_capacity: Option<usize>,
        request_after_shutdown: bool,
        tool: &'static str,
    ) -> Self {
        Self::start_configured(content, queue_capacity, request_after_shutdown, tool, false).await
    }

    async fn start_configured(
        content: String,
        queue_capacity: Option<usize>,
        request_after_shutdown: bool,
        tool: &'static str,
        sender_consent: bool,
    ) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "file_path": workspace.path().join("exact.txt").to_string_lossy(),
            "content": content,
        });
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let provider = Arc::new(ExactLifecycleProvider {
            inner: ToolThenTextProvider {
                calls: AtomicUsize::new(0),
            },
            input: input.clone(),
            calls: calls_tx,
            request_after_shutdown,
            tool,
            shutdown_probe_emitted: AtomicBool::new(false),
        });
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace.path()),
            workspace.path().into(),
        ));
        let mut runtime = mock_runtime(provider, storage.clone(), workspace.path());
        let tools: Arc<dyn ToolSetPort> =
            Arc::new(crate::adapters::toolset_adapter::ToolSetAdapter::new(
                workspace.path().into(),
                Arc::new(FileSystemStorage::with_workspace_root(
                    crate::infrastructure::paths::sessions_dir(workspace.path()),
                    workspace.path().into(),
                )),
                Arc::new(ArcSwap::from_pointee(
                    Arc::new(crate::adapters::sandbox::NoOpSandbox)
                        as Arc<dyn crate::domain::ports::SandboxManager>,
                )),
                Arc::new(tokio::sync::RwLock::new(
                    crate::domain::models::SandboxPolicy::Permissive,
                )),
            ));
        let executions = Arc::new(AtomicUsize::new(0));
        let tools: Arc<dyn ToolSetPort> = Arc::new(CountExactTools {
            inner: tools,
            executions: executions.clone(),
        });
        let rt = Arc::get_mut(&mut runtime).unwrap();
        rt.tools = tools.clone();
        rt.tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
            rt.security.clone(),
            tools,
            rt.approval.clone(),
            64,
        );
        let factory_runtime = runtime.clone();
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let factory_counter = factory_calls.clone();
        let core = Arc::new(DaemonCore::new(
            workspace.path().into(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                factory_counter.fetch_add(1, Ordering::SeqCst);
                Ok(factory_runtime.clone())
            }),
        ));
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        let mut server = AttachServer::new(
            core,
            Arc::new(Mutex::new(Conversation {
                id: "19-20c-lifecycle".into(),
                ..Default::default()
            })),
            domain_tx,
        );
        if sender_consent {
            Arc::get_mut(&mut server).unwrap().pending_consent =
                Some(Arc::new(super::super::consent::PendingConsentManager::new(
                    Arc::new(crate::adapters::policy::JournalConsentProjection::default()),
                    Arc::new(RecordingJournal::default()),
                    Arc::new(SystemClock::default()),
                    None,
                )));
        }
        if let Some(capacity) = queue_capacity {
            Arc::get_mut(&mut server).unwrap().team_send_test_hooks =
                Some(TeamSendTestHooks::new(None, Some(capacity), false));
        }
        let listener = UnixListener::bind(workspace.path().join("exact.sock")).unwrap();
        let shutdown = CancellationToken::new();
        let (forward_rx, retained_rx) = if queue_capacity.is_some() {
            (mpsc::unbounded_channel().1, Some(domain_rx))
        } else {
            (domain_rx, None)
        };
        let (channel_tx, channel_rx) = mpsc::unbounded_channel();
        let service = {
            let server = server.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                server
                    .run(listener, forward_rx, Some(channel_rx), None, shutdown)
                    .await
            })
        };
        Self {
            workspace,
            server,
            runtime,
            service,
            shutdown,
            calls,
            channel_tx,
            input,
            executions,
            factory_calls,
            _domain_rx: retained_rx,
        }
    }

    async fn attach(&self, readonly: bool, tier: ConnectionTier) -> (UnixStream, AttachSnapshot) {
        let mut client = UnixStream::connect(self.workspace.path().join("exact.sock"))
            .await
            .unwrap();
        match attach_with_proof(&mut client, readonly, tier, &test_signer(220)).await {
            Some(DaemonFrame::AttachAck {
                granted_mode,
                snapshot,
            }) => {
                assert_eq!(
                    granted_mode,
                    if readonly || tier == ConnectionTier::Peer {
                        AttachMode::ReadOnly
                    } else {
                        AttachMode::ReadWrite
                    }
                );
                (client, snapshot)
            }
            other => panic!("signed attach failed: {other:?}"),
        }
    }

    async fn request(&mut self, writer: &mut UnixStream) -> crate::domain::models::RequestId {
        write_frame(
            writer,
            &ClientFrame::UserMessage {
                text: "write exact input".into(),
                images: vec![],
            },
        )
        .await
        .unwrap();
        assert_eq!(self.calls.recv().await, Some(1));
        Self::review(writer, &self.input).await
    }

    async fn review(
        writer: &mut UnixStream,
        expected: &serde_json::Value,
    ) -> crate::domain::models::RequestId {
        let (id, bytes) = loop {
            if let DaemonFrame::ApprovalRequest {
                request_id,
                tool,
                input_bytes,
                ..
            } = read_frame::<_, DaemonFrame>(writer)
                .await
                .unwrap()
                .expect("deciding socket remains connected")
            {
                assert_eq!(tool, "Write");
                break (
                    request_id,
                    input_bytes.expect("tools always carry exact input"),
                );
            }
        };
        let mut assembly = super::super::protocol::ExactInputAssembly::new(bytes);
        loop {
            let frame = read_frame::<_, DaemonFrame>(writer).await.unwrap().unwrap();
            assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_FRAME_BYTES as usize);
            match frame {
                DaemonFrame::ApprovalInputChunk {
                    request_id,
                    sequence,
                    data,
                } => {
                    assert_eq!(request_id, id);
                    assembly.push(sequence, &data).unwrap();
                }
                DaemonFrame::ApprovalInputComplete { request_id, chunks } => {
                    assert_eq!(request_id, id);
                    assert_eq!(assembly.finish(chunks).unwrap(), *expected);
                    break;
                }
                _ => {}
            }
        }
        id
    }

    async fn refuse(
        client: &mut UnixStream,
        id: &crate::domain::models::RequestId,
        reason: super::super::protocol::ApprovalRefusal,
    ) {
        write_frame(
            client,
            &ClientFrame::ApprovalResponse {
                request_id: id.clone(),
                outcome: crate::domain::models::ApprovalOutcome::Once,
            },
        )
        .await
        .unwrap();
        write_frame(
            client,
            &ClientFrame::TeamStatus {
                request: "approval-refusal-fence".into(),
            },
        )
        .await
        .unwrap();
        let mut refused = false;
        loop {
            match read_frame::<_, DaemonFrame>(client).await.unwrap().unwrap() {
                DaemonFrame::ApprovalRefused {
                    request_id,
                    reason: actual,
                } => {
                    assert_eq!(&request_id, id);
                    assert_eq!(actual, reason);
                    refused = true;
                }
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::TeamStatus,
                    ..
                }
                | DaemonFrame::TeamStatusResult { .. } => {
                    assert!(
                        refused,
                        "ordered control-lane fence arrived without ApprovalRefused"
                    );
                    break;
                }
                DaemonFrame::ApprovalRequest { .. }
                | DaemonFrame::ApprovalInputChunk { .. }
                | DaemonFrame::ApprovalInputComplete { .. } => {
                    panic!("observer received exact review")
                }
                _ => {}
            }
        }
    }

    async fn decide_once(&self, writer: &mut UnixStream, id: &crate::domain::models::RequestId) {
        write_frame(
            writer,
            &ClientFrame::ApprovalResponse {
                request_id: id.clone(),
                outcome: crate::domain::models::ApprovalOutcome::Once,
            },
        )
        .await
        .unwrap();
        write_frame(
            writer,
            &ClientFrame::TeamStatus {
                request: "approval-decision-fence".into(),
            },
        )
        .await
        .unwrap();
        let mut closed = false;
        loop {
            match read_frame::<_, DaemonFrame>(writer).await.unwrap().unwrap() {
                DaemonFrame::ApprovalClosed { request_id } if request_id == *id => closed = true,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::TeamStatus,
                    ..
                }
                | DaemonFrame::TeamStatusResult { .. } => break,
                _ => {}
            }
        }
        assert!(
            self.runtime
                .approval
                .pending_review(
                    id,
                    crate::domain::services::approval_runtime::ReviewScope::AllTools
                )
                .await
                .is_none(),
            "socket decision must resolve the generic permission lane, not proposal authorization"
        );
        while self
            .server
            .approval_gate
            .issues
            .lock()
            .await
            .issued
            .contains_key(id)
        {
            tokio::task::yield_now().await;
        }
        write_frame(
            writer,
            &ClientFrame::TeamStatus {
                request: "approval-closed-fence".into(),
            },
        )
        .await
        .unwrap();
        loop {
            match read_frame::<_, DaemonFrame>(writer).await.unwrap().unwrap() {
                DaemonFrame::ApprovalClosed { request_id } if request_id == *id => closed = true,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::TeamStatus,
                    ..
                }
                | DaemonFrame::TeamStatusResult { .. } => {
                    assert!(
                        closed,
                        "resolved tool request must send its issuing writer ApprovalClosed"
                    );
                    break;
                }
                DaemonFrame::ApprovalInputChunk { request_id, .. }
                | DaemonFrame::ApprovalInputComplete { request_id, .. }
                    if request_id == *id =>
                {
                    panic!("input transfer followed the resolved request");
                }
                _ => {}
            }
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.service.await.unwrap();
        assert!(
            self.runtime
                .approval
                .pending_tool_requests(
                    crate::domain::services::approval_runtime::ReviewScope::AllTools,
                )
                .await
                .is_empty()
        );
        assert!(self.server.approval_gate.tasks.lock().await.is_empty());
        assert_eq!(
            self.server.approval_gate.active.load(Ordering::SeqCst),
            0,
            "every gate joined"
        );
        assert!(
            self.server
                .approval_gate
                .issues
                .lock()
                .await
                .issued
                .is_empty()
        );
    }
}

#[tokio::test]
async fn story19_20c_socket_exact_retention_deadline_and_decision_admission() {
    use super::super::protocol::ApprovalRefusal;
    let mut host =
        ExactLifecycleHost::start(format!("{}漢🙂\nTAIL", "original ".repeat(24)), None).await;
    let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    let (mut mirror, _) = host.attach(true, ConnectionTier::TrustedLocal).await;
    let (mut peer, _) = host.attach(false, ConnectionTier::Peer).await;
    let id = host.request(&mut writer).await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(121)).await;
    assert!(
        host.runtime
            .approval
            .pending_review(
                &id,
                crate::domain::services::approval_runtime::ReviewScope::AllTools
            )
            .await
            .is_some()
    );
    assert!(!host.workspace.path().join("exact.txt").exists());
    tokio::time::resume();
    ExactLifecycleHost::refuse(&mut mirror, &id, ApprovalRefusal::WriterOnly).await;
    ExactLifecycleHost::refuse(&mut peer, &id, ApprovalRefusal::WriterOnly).await;
    write_frame(&mut writer, &ClientFrame::Detach)
        .await
        .unwrap();
    while read_frame::<_, DaemonFrame>(&mut writer)
        .await
        .unwrap()
        .is_some()
    {}
    assert!(
        host.runtime
            .approval
            .pending_review(
                &id,
                crate::domain::services::approval_runtime::ReviewScope::AllTools
            )
            .await
            .is_some()
    );
    let (mut retained_mirror, snapshot) = host.attach(true, ConnectionTier::TrustedLocal).await;
    assert_eq!(snapshot.retained_approvals, 1);
    ExactLifecycleHost::refuse(&mut retained_mirror, &id, ApprovalRefusal::WriterOnly).await;
    let (mut next_writer, snapshot) = host.attach(false, ConnectionTier::TrustedLocal).await;
    assert_eq!(snapshot.retained_approvals, 1);
    host.server
        .approval_gate
        .replay_completed
        .acquire()
        .await
        .unwrap()
        .forget();
    let writer_id = host
        .server
        .registry
        .lock()
        .await
        .approval_writer()
        .unwrap()
        .id;
    assert_eq!(
        host.server
            .approval_gate
            .issues
            .lock()
            .await
            .issued
            .get(&id),
        Some(&Some(writer_id)),
        "next writer is issued the retained request before reading its card"
    );
    assert_eq!(
        ExactLifecycleHost::review(&mut next_writer, &host.input).await,
        id
    );
    write_frame(
        &mut next_writer,
        &ClientFrame::UserMessage {
            text: "queued turn".into(),
            images: vec![],
        },
    )
    .await
    .unwrap();
    assert!(
        host.calls.try_recv().is_err(),
        "later turn cannot enter provider before decision"
    );
    host.decide_once(&mut next_writer, &id).await;
    assert_eq!(host.calls.recv().await, Some(2), "approved turn completes");
    assert_eq!(
        host.calls.recv().await,
        Some(3),
        "queued operator turn runs after decision"
    );
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("exact.txt")).unwrap(),
        host.input["content"].as_str().unwrap()
    );
    ExactLifecycleHost::refuse(&mut next_writer, &id, ApprovalRefusal::NoLongerPending).await;
    ExactLifecycleHost::refuse(
        &mut next_writer,
        &crate::domain::models::RequestId::new(),
        ApprovalRefusal::NoLongerPending,
    )
    .await;
    assert_eq!(
        host.executions.load(Ordering::SeqCst),
        1,
        "duplicate socket decision executes no second invocation"
    );
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_socket_full_queue_and_full_reissue_remain_retained() {
    use crate::domain::services::approval_runtime::ReviewScope;
    let mut host = ExactLifecycleHost::start("queue canary".into(), Some(1)).await;
    let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    write_frame(
        &mut writer,
        &ClientFrame::UserMessage {
            text: "write".into(),
            images: vec![],
        },
    )
    .await
    .unwrap();
    assert_eq!(host.calls.recv().await, Some(1));
    host.server
        .approval_gate
        .transfer_attempts
        .acquire()
        .await
        .unwrap()
        .forget();
    assert!(
        host.server
            .registry
            .lock()
            .await
            .approval_writer()
            .is_none(),
        "a full queue must terminal-close, never log-and-drop"
    );
    assert!(matches!(
        read_frame::<_, DaemonFrame>(&mut writer).await.unwrap(),
        Some(DaemonFrame::Error(ProtocolError::Internal(_)))
    ));
    assert!(
        read_frame::<_, DaemonFrame>(&mut writer)
            .await
            .unwrap()
            .is_none()
    );
    let pending = host
        .runtime
        .approval
        .pending_tool_requests(ReviewScope::AllTools)
        .await;
    assert_eq!(pending.len(), 1);
    let (mut retry, snapshot) = host.attach(false, ConnectionTier::TrustedLocal).await;
    assert_eq!(snapshot.retained_approvals, 1);
    host.server
        .approval_gate
        .replay_completed
        .acquire()
        .await
        .unwrap()
        .forget();
    assert!(
        host.server
            .registry
            .lock()
            .await
            .approval_writer()
            .is_none()
    );
    assert_eq!(
        host.runtime
            .approval
            .pending_tool_requests(ReviewScope::AllTools)
            .await
            .len(),
        1,
        "full replay retains instead of resolving or rejecting"
    );
    assert!(matches!(
        read_frame::<_, DaemonFrame>(&mut retry).await.unwrap(),
        Some(DaemonFrame::Error(ProtocolError::Internal(_)))
    ));
    assert!(
        read_frame::<_, DaemonFrame>(&mut retry)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        host.runtime
            .approval
            .pending_tool_requests(ReviewScope::AllTools)
            .await
            .len(),
        1
    );
    assert_eq!(host.server.blocked_waiting.load(Ordering::SeqCst), 0);
    host.server
        .team_send_test_hooks
        .as_ref()
        .unwrap()
        .restore_default_queue_capacity();
    let (mut replacement, snapshot) = host.attach(false, ConnectionTier::TrustedLocal).await;
    assert_eq!(snapshot.retained_approvals, 1);
    let id = ExactLifecycleHost::review(&mut replacement, &host.input).await;
    assert_eq!(id, pending[0].id);
    write_frame(
        &mut replacement,
        &ClientFrame::ApprovalResponse {
            request_id: id,
            outcome: crate::domain::models::ApprovalOutcome::Once,
        },
    )
    .await
    .unwrap();
    assert_eq!(host.calls.recv().await, Some(2));
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("exact.txt")).unwrap(),
        "queue canary"
    );
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_socket_shutdown_cancels_exact_pending_and_joins_gate() {
    let mut host =
        ExactLifecycleHost::start_with_options("must not execute".into(), None, true, "Write")
            .await;
    let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    let id = host.request(&mut writer).await;
    let mut transitions = host.runtime.tool_scheduler.subscribe();
    write_frame(
        &mut writer,
        &ClientFrame::UserMessage {
            text: "queued turn must not open a new approval after shutdown".into(),
            images: vec![],
        },
    )
    .await
    .unwrap();
    write_frame(
        &mut writer,
        &ClientFrame::TeamStatus {
            request: "queued-before-shutdown".into(),
        },
    )
    .await
    .unwrap();
    loop {
        match read_frame::<_, DaemonFrame>(&mut writer)
            .await
            .unwrap()
            .unwrap()
        {
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::TeamStatus,
                ..
            }
            | DaemonFrame::TeamStatusResult { .. } => break,
            _ => {}
        }
    }
    assert!(
        host.calls.try_recv().is_err(),
        "queued turn still waits on turn_serial"
    );
    let (release_exit, held_exit) = tokio::sync::oneshot::channel();
    *host.server.approval_gate.exit_hold.lock().await = Some(held_exit);
    host.shutdown.cancel();
    host.server
        .approval_gate
        .exit_entered
        .acquire()
        .await
        .unwrap()
        .forget();
    assert!(
        !host.service.is_finished(),
        "shutdown cannot return before its held gate task exits"
    );
    release_exit.send(()).unwrap();
    host.service.await.unwrap();
    assert!(host.server.approval_gate.tasks.lock().await.is_empty());
    assert_eq!(
        host.server.approval_gate.active.load(Ordering::SeqCst),
        0,
        "shutdown joins gate before returning"
    );
    assert!(
        host.runtime
            .approval
            .pending_review(
                &id,
                crate::domain::services::approval_runtime::ReviewScope::AllTools
            )
            .await
            .is_none()
    );
    ExactLifecycleHost::refuse(
        &mut writer,
        &id,
        super::super::protocol::ApprovalRefusal::NoLongerPending,
    )
    .await;
    let mut cancelled = 0;
    while cancelled < 2 {
        match transitions.recv().await.unwrap().call {
            crate::domain::models::ToolCall::Cancelled { .. }
            | crate::domain::models::ToolCall::Error { .. } => cancelled += 1,
            crate::domain::models::ToolCall::AwaitingApproval { id, .. }
                if id == "after-shutdown-tool" =>
            {
                panic!("queued turn admitted a new pending responder after shutdown");
            }
            crate::domain::models::ToolCall::Executing { .. }
            | crate::domain::models::ToolCall::Success { .. } => {
                panic!("shutdown request executed")
            }
            _ => {}
        }
    }
    assert!(!host.workspace.path().join("exact.txt").exists());
    assert!(host.runtime.approval.pending_tool_requests(
        crate::domain::services::approval_runtime::ReviewScope::AllTools,
    ).await.is_empty());
}

#[tokio::test]
async fn story19_20c_socket_large_exact_chunks_execute_original_value() {
    let mut host =
        ExactLifecycleHost::start(format!("{}漢🙂TAIL", "x".repeat(10 * 1024 * 1024)), None).await;
    let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    let id = host.request(&mut writer).await;
    write_frame(
        &mut writer,
        &ClientFrame::ApprovalResponse {
            request_id: id,
            outcome: crate::domain::models::ApprovalOutcome::Once,
        },
    )
    .await
    .unwrap();
    assert_eq!(host.calls.recv().await, Some(2));
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("exact.txt")).unwrap(),
        host.input["content"].as_str().unwrap()
    );
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_channel_never_delivered_approval_records_unattended_denial() {
    let mut host = ExactLifecycleHost::start("unattended must not execute".into(), None).await;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    host.channel_tx
        .send(ChannelTurnRequest {
            text: "write without an attached writer".into(),
            origin: ChannelKind::Telegram,
            response_tx,
        })
        .unwrap();
    assert_eq!(host.calls.recv().await, Some(1));
    assert_eq!(
        host.calls.recv().await,
        Some(2),
        "denial returns to the actual provider"
    );
    response_rx.await.unwrap();
    // Gate tasks share issuance state: another task can retire the request
    // before the denying task finishes its asynchronous storage write.
    // Join every gate before asserting the record is durable on disk.
    host.server
        .approval_gate
        .shutdown(&host.server.registry)
        .await;
    assert!(!host.workspace.path().join("exact.txt").exists());
    assert_eq!(host.server.blocked_waiting.load(Ordering::SeqCst), 1);
    let conversation = host
        .runtime
        .storage
        .load_conversation("19-20c-lifecycle")
        .await
        .unwrap()
        .unwrap();
    assert!(
        conversation.messages.iter().any(|message| message.synthetic
            && message.content.contains("Skipped: Write needed approval"))
    );
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_socket_replaced_writer_and_unissued_successor_are_refused() {
    use super::super::protocol::ApprovalRefusal;
    let mut host = ExactLifecycleHost::start("replacement canary".into(), None).await;
    let (mut old_writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    let id = host.request(&mut old_writer).await;
    // Seed only transport registration: the old socket still carries its
    // originally granted ReadWrite mode, while the live writer slot changes.
    // Pending input and authority issuance both came from the real turn.
    host.server
        .registry
        .lock()
        .await
        .conns
        .iter_mut()
        .find(|conn| conn.mode == AttachMode::ReadWrite)
        .unwrap()
        .mode = AttachMode::ReadOnly;
    let (release, held) = tokio::sync::oneshot::channel();
    *host.server.approval_gate.replay_hold.lock().await = Some(held);
    let (mut successor, snapshot) = host.attach(false, ConnectionTier::TrustedLocal).await;
    assert_eq!(snapshot.retained_approvals, 1);
    host.server
        .approval_gate
        .replay_entered
        .acquire()
        .await
        .unwrap()
        .forget();
    ExactLifecycleHost::refuse(&mut old_writer, &id, ApprovalRefusal::WriterOnly).await;
    ExactLifecycleHost::refuse(&mut successor, &id, ApprovalRefusal::IssuedElsewhere).await;
    assert!(!host.workspace.path().join("exact.txt").exists());
    assert!(
        host.runtime
            .approval
            .pending_review(
                &id,
                crate::domain::services::approval_runtime::ReviewScope::AllTools
            )
            .await
            .is_some()
    );
    release.send(()).unwrap();
    assert_eq!(
        ExactLifecycleHost::review(&mut successor, &host.input).await,
        id
    );
    write_frame(
        &mut successor,
        &ClientFrame::ApprovalResponse {
            request_id: id.clone(),
            outcome: crate::domain::models::ApprovalOutcome::Once,
        },
    )
    .await
    .unwrap();
    assert_eq!(host.calls.recv().await, Some(2));
    ExactLifecycleHost::refuse(&mut successor, &id, ApprovalRefusal::NoLongerPending).await;
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("exact.txt")).unwrap(),
        "replacement canary"
    );
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_channel_unattended_safe_read_executes_once_without_prompt() {
    let mut host = ExactLifecycleHost::start_with_options(String::new(), None, false, "Read").await;
    std::fs::write(host.workspace.path().join("exact.txt"), "safe read canary").unwrap();
    let mut approvals = host.runtime.approval.subscribe();
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    host.channel_tx
        .send(ChannelTurnRequest {
            text: "read the canary with no writer".into(),
            origin: ChannelKind::Telegram,
            response_tx,
        })
        .unwrap();
    assert_eq!(host.calls.recv().await, Some(1));
    assert_eq!(host.calls.recv().await, Some(2));
    response_rx.await.unwrap();
    assert_eq!(host.executions.load(Ordering::SeqCst), 1);
    assert_eq!(host.server.blocked_waiting.load(Ordering::SeqCst), 0);
    assert!(matches!(
        approvals.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_socket_shutdown_interrupts_both_inbound_admission_rails() {
    use crate::domain::ports::{InboundApprovalDecision, InboundPeerRuntime};
    for sender_consent in [false, true] {
        let mut host = ExactLifecycleHost::start_configured(
            "not executed".into(),
            None,
            false,
            "Write",
            sender_consent,
        )
        .await;
        let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
        let principal = PolicyPrincipal::rap(test_signer(239).identity().peer_id.clone());
        let ticket = host
            .server
            .request_admission_approval(&principal, "interrupted task")
            .await
            .unwrap();
        assert!(ticket.pending);
        let grouped = if sender_consent {
            Some(
                host.server
                    .request_admission_approval(&principal, "grouped interrupted task")
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let id = loop {
            if let DaemonFrame::ApprovalRequest {
                request_id, tool, ..
            } = read_frame::<_, DaemonFrame>(&mut writer)
                .await
                .unwrap()
                .unwrap()
            {
                assert_eq!(
                    tool,
                    if sender_consent {
                        "a2a/sender-consent"
                    } else {
                        "a2a/message.send"
                    }
                );
                break request_id;
            }
        };
        host.shutdown.cancel();
        host.service.await.unwrap();
        assert_eq!(
            ticket.decision.await.unwrap(),
            InboundApprovalDecision::RuntimeShutdown
        );
        if let Some(grouped) = grouped {
            assert_eq!(
                grouped.decision.await.unwrap(),
                InboundApprovalDecision::RuntimeShutdown
            );
        }
        assert!(host.runtime.approval.pending_source(&id).await.is_none());
        assert!(host.server.approval_gate.tasks.lock().await.is_empty());
        let late = host
            .server
            .request_admission_approval(&principal, "after shutdown")
            .await
            .unwrap();
        assert!(!late.pending);
        assert_eq!(
            late.decision.await.unwrap(),
            InboundApprovalDecision::RuntimeShutdown
        );
        assert_eq!(host.executions.load(Ordering::SeqCst), 0);
        assert!(host.calls.try_recv().is_err());
    }
}

#[tokio::test]
async fn story19_20c_socket_shutdown_never_promotes_a_persisted_consent_grant() {
    use crate::domain::ports::{InboundApprovalDecision, InboundPeerRuntime};
    let mut host =
        ExactLifecycleHost::start_configured("not executed".into(), None, false, "Write", true)
            .await;
    let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    let principal = PolicyPrincipal::rap(test_signer(238).identity().peer_id.clone());
    let ticket = host
        .server
        .request_admission_approval(&principal, "interrupted granted task")
        .await
        .unwrap();
    let id = loop {
        if let DaemonFrame::ApprovalRequest { request_id, .. } =
            read_frame::<_, DaemonFrame>(&mut writer)
                .await
                .unwrap()
                .unwrap()
        {
            break request_id;
        }
    };
    // Pause at the actual durable-grant/approval-resolution boundary. The
    // recorded trust survives, but a still-pending task must not be released
    // when RuntimeShutdown wins before its runtime responder resolves.
    assert!(
        host.server
            .pending_consent
            .as_ref()
            .unwrap()
            .grant_always(id, &principal)
            .await
    );
    host.shutdown.cancel();
    host.service.await.unwrap();
    assert_eq!(
        ticket.decision.await.unwrap(),
        InboundApprovalDecision::RuntimeShutdown
    );
    let late = host
        .server
        .request_admission_approval(&principal, "trusted but stopped")
        .await
        .unwrap();
    assert!(!late.pending);
    assert_eq!(
        late.decision.await.unwrap(),
        InboundApprovalDecision::RuntimeShutdown
    );
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
    assert!(host.calls.try_recv().is_err());
}

#[tokio::test]
async fn story19_20c_socket_idle_forged_decisions_do_not_build_runtime() {
    use super::super::protocol::ApprovalRefusal;
    let host = ExactLifecycleHost::start("idle".into(), None).await;
    let (mut writer, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
    let (mut reader, _) = host.attach(true, ConnectionTier::TrustedLocal).await;
    let (mut peer, _) = host.attach(false, ConnectionTier::Peer).await;
    let id = crate::domain::models::RequestId::new();
    for client in [&mut reader, &mut peer] {
        ExactLifecycleHost::refuse(client, &id, ApprovalRefusal::WriterOnly).await;
    }
    ExactLifecycleHost::refuse(&mut writer, &id, ApprovalRefusal::NoLongerPending).await;
    // A stale transport identity must not touch the lazy factory either.
    host.server
        .resolve_tool_approval(
            id,
            crate::domain::models::ApprovalOutcome::Once,
            AttachMode::ReadWrite,
            ConnectionTier::TrustedLocal,
            u64::MAX,
        )
        .await;
    assert_eq!(host.factory_calls.load(Ordering::SeqCst), 0);
    assert!(host.server.core.built_runtime().is_none());
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_shutdown_first_gate_registration_closes_supplied_runtime() {
    use crate::domain::models::{ApprovalOutcome, ApprovalSource, ToolRisk};
    use crate::domain::services::approval_runtime::ReviewScope;
    let host = ExactLifecycleHost::start("shutdown-first".into(), None).await;
    host.server
        .approval_gate
        .shutdown(&host.server.registry)
        .await;
    let approval = host.runtime.approval.clone();
    let mut events = approval.subscribe();
    let source = ApprovalSource::ForegroundTurn { conversation_id: "late-runtime".into() };
    let (id, responder) = approval
        .request(source.clone(), "Bash".into(), serde_json::json!({"command": "must not execute"}), ToolRisk::Elevated, None, None)
        .await;
    let id = id.expect("runtime starts open before its late registration");
    assert!(matches!(events.recv().await.unwrap(),
        crate::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { id: requested, .. } if requested == id));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        host.server.ensure_approval_gate(approval.clone()),
    )
    .await
    .expect("late registration must not hold the task registry while cancelling");
    assert_eq!(responder.await.unwrap().outcome, ApprovalOutcome::Cancel);
    assert!(matches!(events.recv().await.unwrap(),
        crate::domain::services::approval_runtime::ApprovalRuntimeEvent::Cancelled {
            id: cancelled,
            reason: crate::domain::services::approval_runtime::CancelReason::RuntimeShutdown,
        } if cancelled == id));
    assert!(approval.pending_tool_requests(ReviewScope::AllTools).await.is_empty());
    let (late_id, late) = approval
        .request(source, "Bash".into(), serde_json::json!({"command": "also must not execute"}), ToolRisk::Elevated, None, None)
        .await;
    assert!(late_id.is_none(), "RuntimeShutdown must close future admission");
    assert_eq!(late.await.unwrap().outcome, ApprovalOutcome::Cancel);
    assert!(host.server.approval_gate.tasks.lock().await.is_empty());
    host.stop().await;
}

#[tokio::test]
async fn story19_20c_socket_oversized_prose_and_result_attach_stays_usable() {
    use crate::domain::models::{ChatMessage, MessageRole, ToolCallInfo, ToolResultInfo, WriteDiffState};
    for result in [false, true] {
        let host = ExactLifecycleHost::start("attach".into(), None).await;
        let (mut existing, _) = host.attach(false, ConnectionTier::TrustedLocal).await;
        let large = "reply漢🙂".repeat(1_000_000);
        let mut oversized = ChatMessage {
            id: "oversized-row".into(),
            role: MessageRole::Assistant,
            ..Default::default()
        };
        if result {
            oversized.tool_calls.push(ToolCallInfo { id: "result-call".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "printf result"}),
            result: Some(ToolResultInfo { content: large,
            is_error: false,
            diff: WriteDiffState::NotAWrite, images: Vec::new() }),
            started_at_ms: None,
            completed_at_ms: None,
            status: None, provider_id: None });
        } else {
            oversized.content = large;
        }
        {
            let mut conversation = host.server.conversation.lock().await;
            conversation.messages.push(oversized);
            conversation.messages.push(ChatMessage {
                id: "recent-row".into(),
                role: MessageRole::Assistant,
                content: "recent reply remains exact".into(),
                ..Default::default()
            });
        }
        for tier in [ConnectionTier::TrustedLocal, ConnectionTier::Peer] {
            let (mut attached, snapshot) = host.attach(true, tier).await;
            assert_eq!(snapshot.conversation_id, "19-20c-lifecycle");
            assert_eq!(snapshot.transcript.len(), 1);
            assert_eq!(snapshot.transcript[0].id, "recent-row");
            assert_eq!(snapshot.transcript[0].content, "recent reply remains exact");
            assert!(matches!(
                read_frame::<_, DaemonFrame>(&mut attached).await.unwrap(),
                Some(DaemonFrame::FrameTooLarge { frame, bytes })
                    if frame == "AttachAck" && bytes > u64::from(MAX_FRAME_BYTES)
            ));
            ExactLifecycleHost::refuse(
                &mut attached,
                &crate::domain::models::RequestId::new(),
                super::super::protocol::ApprovalRefusal::WriterOnly,
            ).await;
        }
        ExactLifecycleHost::refuse(
            &mut existing,
            &crate::domain::models::RequestId::new(),
            super::super::protocol::ApprovalRefusal::NoLongerPending,
        ).await;
        assert_eq!(host.server.conversation.lock().await.messages.len(), 2);
        host.stop().await;
    }
}

#[tokio::test]
async fn story19_20c_socket_oversized_attach_metadata_refuses_without_fake_ack() {
    let host = ExactLifecycleHost::start("metadata".into(), None).await;
    host.server.conversation.lock().await.id = "m".repeat(MAX_FRAME_BYTES as usize + 1);
    let mut client = UnixStream::connect(host.workspace.path().join("exact.sock")).await.unwrap();
    assert!(matches!(
        attach_with_proof(&mut client, false, ConnectionTier::TrustedLocal, &test_signer(220)).await,
        Some(DaemonFrame::Error(ProtocolError::Internal(_)))
    ));
    assert!(read_frame::<_, DaemonFrame>(&mut client).await.unwrap().is_none());
    assert!(!host.server.registry.lock().await.has_writer());
    host.server.conversation.lock().await.id = "19-20c-lifecycle".into();
    let (_client, snapshot) = host.attach(false, ConnectionTier::TrustedLocal).await;
    assert_eq!(snapshot.conversation_id, "19-20c-lifecycle");
    host.stop().await;
}
