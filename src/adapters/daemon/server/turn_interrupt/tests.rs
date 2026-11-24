//! Story 21.2 K8 — attached interrupt, daemon vertical. Real authenticated Unix
//! sockets with a held terminal provider stream / held tool; barriers and
//! structural hooks only, never sleeps (the read deadline is a hang guard).
use super::super::tests::{attach_with_proof, mock_runtime, test_signer};
use super::*;
use crate::adapters::daemon::protocol::{InterruptOutcome, InterruptRefusal};
use crate::adapters::daemon::runtime::DaemonCore;
use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::noop::{NoOpMemory, NoOpPersona, NoOpSecurity};
use crate::domain::errors::{ProviderError, ToolError};
use crate::domain::models::{
    AppConfig, CompletionOptions, Message, ModelDescriptor, ToolDefinition, ToolResult,
};
use crate::domain::ports::{StoragePort, StreamingProvider, ToolSetPort};
use crate::infrastructure::runtime::event_bus::{EventBus, RawEventKind};
use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use std::sync::atomic::AtomicUsize;
use tokio::net::{UnixListener, UnixStream};

const CONVERSATION: &str = "interrupt-conv";
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

// ── The held world ──────────────────────────────────────────────────────────

/// What the provider does on its n-th call.
#[derive(Clone, Copy)]
enum Step {
    /// Answer `text` and end the turn.
    Text(&'static str),
    Billable,
    /// Ask for the held `Glob` tool.
    Tool,
    /// Ask for a `Bash` tool that requires operator approval.
    ApprovalTool,
    /// Stream `partial`, then hang until released.
    HeldStream,
    /// End without a response terminal.
    Disconnect,
    /// Complete without text.
    Empty,
    /// A response containing an operator-private proposal.
    Private,
    /// Fail before a stream is created.
    Fail,
}

/// Barriers shared by the provider, the tool and the test.
#[derive(Clone)]
struct Probes {
    /// One permit each time a held point (tool body or stalled stream) is reached.
    entered: Arc<Semaphore>,
    /// Add a permit to let one held point proceed normally.
    release: Arc<Semaphore>,
    /// Held points whose future/stream has finished or been dropped.
    exited: Arc<AtomicUsize>,
    /// Every `stream_completion` call the provider has received.
    stream_calls: Arc<AtomicUsize>,
    /// The held tool returns an error as soon as its token is cancelled.
    fail_on_cancel: Arc<AtomicBool>,
}

impl Probes {
    fn new() -> Self {
        Self {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
            exited: Arc::new(AtomicUsize::new(0)),
            stream_calls: Arc::new(AtomicUsize::new(0)),
            fail_on_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn held_point_reached(&self) {
        tokio::time::timeout(DEADLINE, self.entered.acquire())
            .await
            .expect("the provider/tool never reached its held point")
            .expect("open")
            .forget();
    }

    fn exited(&self) -> usize {
        self.exited.load(Ordering::SeqCst)
    }
}

struct ExitGuard(Arc<AtomicUsize>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct HeldProvider {
    calls: AtomicUsize,
    script: Vec<Step>,
    probes: Probes,
}

#[async_trait::async_trait]
impl StreamingProvider for HeldProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.probes.stream_calls.fetch_add(1, Ordering::SeqCst);
        let step = self.script[call.min(self.script.len() - 1)];
        if matches!(step, Step::Fail) {
            return Err(ProviderError::ConnectionFailed(
                "c2b fixture failure".into(),
            ));
        }
        Ok(match step {
            Step::Text(text) => futures::stream::iter(vec![
                StreamChunk::Text {
                    content: text.to_owned(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .boxed(),
            Step::Billable => {
                self.probes.entered.add_permits(1);
                self.probes.release.acquire().await.unwrap().forget();
                futures::stream::iter(vec![
                    StreamChunk::Text {
                        content: "C2bBudgetChannel normal reply".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::Usage {
                        usage: crate::domain::models::usage::UsageInfo {
                            input_tokens: 10_000,
                            output_tokens: 0,
                            cache_creation_input_tokens: None,
                            cache_read_input_tokens: None,
                            reasoning_tokens: None,
                            counters_observed: false,
                        },
                        session_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ])
                .boxed()
            }
            Step::Disconnect => futures::stream::iter(vec![StreamChunk::Text {
                content: "partial".into(),
                parent_tool_use_id: None,
            }])
            .boxed(),
            Step::Empty => futures::stream::iter(vec![StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }])
            .boxed(),
            Step::Private => futures::stream::iter(vec![
                StreamChunk::Text {
                    content: "visible".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::ToolUse {
                    id: "private-proposal".into(),
                    name: crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME.into(),
                    input: serde_json::json!({"secret": "private"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .boxed(),
            Step::Fail => unreachable!(),
            Step::Tool | Step::ApprovalTool => futures::stream::iter(vec![
                StreamChunk::ToolUse {
                    id: format!("held-{call}"),
                    name: if matches!(step, Step::Tool) {
                        "Glob"
                    } else {
                        "Bash"
                    }
                    .to_owned(),
                    input: if matches!(step, Step::Tool) {
                        serde_json::json!({"pattern": "*"})
                    } else {
                        serde_json::json!({"command": "echo held"})
                    },
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ])
            .boxed(),
            Step::HeldStream => {
                let probes = self.probes.clone();
                let guard = ExitGuard(probes.exited.clone());
                futures::stream::unfold(
                    (0_u8, guard, probes),
                    |(phase, guard, probes)| async move {
                        match phase {
                            0 => Some((
                                StreamChunk::Text {
                                    content: "partial".to_owned(),
                                    parent_tool_use_id: None,
                                },
                                (1, guard, probes),
                            )),
                            1 => {
                                probes.entered.add_permits(1);
                                probes.release.acquire().await.expect("open").forget();
                                Some((
                                    StreamChunk::TurnComplete {
                                        stop_reason: StopReason::EndTurn,
                                    },
                                    (2, guard, probes),
                                ))
                            }
                            _ => None,
                        }
                    },
                )
                .boxed()
            }
        })
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "held".to_owned()
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

/// A `Glob` tool that reports it is running, then waits for a release permit
/// or for its cancellation token.
struct HeldTool {
    probes: Probes,
}

#[async_trait::async_trait]
impl ToolSetPort for HeldTool {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        ["Glob", "Bash"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.to_owned(),
                description: "held".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                parallel_safe: true,
            })
            .collect()
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let _exit = ExitGuard(self.probes.exited.clone());
        self.probes.entered.add_permits(1);
        if self.probes.fail_on_cancel.load(Ordering::SeqCst) {
            // A well-behaved tool: it reports its cancellation as an ERROR, which
            // the scheduler may surface as `ToolCall::Error` instead of `Cancelled`.
            tokio::select! {
                permit = self.probes.release.acquire() => permit.expect("open").forget(),
                () = cancel.cancelled() => {
                    return Err(ToolError::ExecutionFailed("cancelled by the operator".to_owned()));
                }
            }
        } else {
            // Ignores its own token: only the scheduler's cancellation race (or the
            // driver's abort) can end it, so the outcome is deterministic.
            self.probes.release.acquire().await.expect("open").forget();
        }
        Ok(ToolResult {
            tool_use_id: "held".to_owned(),
            content: "held result".to_owned(),
            is_error: false,
            images: vec![],
        })
    }
}

fn held_core(
    workspace: &std::path::Path,
    provider: Arc<HeldProvider>,
    tool: Arc<HeldTool>,
) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        crate::infrastructure::paths::sessions_dir(workspace),
        workspace.to_path_buf(),
    ));
    let ws = workspace.to_path_buf();
    let factory_storage = storage.clone();
    let core = DaemonCore::new(
        ws.clone(),
        Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        Arc::new(NoOpMemory),
        storage.clone(),
        Arc::new(NoOpSecurity),
        Arc::new(NoOpPersona),
        Arc::new(crate::adapters::rap::PeerTopicStore::new()),
        Box::new(move || {
            let mut runtime =
                Arc::try_unwrap(mock_runtime(provider.clone(), factory_storage.clone(), &ws))
                    .ok()
                    .expect("new unshared runtime");
            let tools: Arc<dyn ToolSetPort> = tool.clone();
            runtime.tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
                runtime.security.clone(),
                tools.clone(),
                runtime.approval.clone(),
                64,
            );
            runtime.tools = tools;
            Ok(Arc::new(runtime))
        }),
    );
    (Arc::new(core), storage)
}

// ── The daemon rig ──────────────────────────────────────────────────────────

struct Rig {
    server: Arc<AttachServer>,
    socket: std::path::PathBuf,
    storage: Arc<dyn StoragePort>,
    probes: Probes,
    channel_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    shutdown: CancellationToken,
    run: tokio::task::JoinHandle<()>,
    _bus: EventBus,
    _workspace: tempfile::TempDir,
}

impl Rig {
    async fn start(script: Vec<Step>) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let probes = Probes::new();
        let provider = Arc::new(HeldProvider {
            calls: AtomicUsize::new(0),
            script,
            probes: probes.clone(),
        });
        let tool = Arc::new(HeldTool {
            probes: probes.clone(),
        });
        let (core, storage) = held_core(workspace.path(), provider, tool);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: CONVERSATION.to_owned(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let (channel_tx, channel_rx) = mpsc::unbounded_channel();
        let socket = workspace.path().join("interrupt.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let run = {
            let server = server.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                server
                    .run(listener, domain_rx, Some(channel_rx), None, shutdown)
                    .await
            })
        };
        Self {
            server,
            socket,
            storage,
            probes,
            channel_tx,
            shutdown,
            run,
            _bus: bus,
            _workspace: workspace,
        }
    }

    async fn attach(
        &self,
        seed: u8,
        read_only_ok: bool,
        tier: ConnectionTier,
    ) -> (UnixStream, AttachMode) {
        let mut stream = UnixStream::connect(&self.socket).await.unwrap();
        match attach_with_proof(&mut stream, read_only_ok, tier, &test_signer(seed)).await {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => (stream, granted_mode),
            other => panic!("expected AttachAck, got {other:?}"),
        }
    }

    async fn writer(&self, seed: u8) -> UnixStream {
        let (stream, mode) = self.attach(seed, false, ConnectionTier::TrustedLocal).await;
        assert_eq!(mode, AttachMode::ReadWrite, "first attach is the writer");
        stream
    }

    async fn persisted(&self) -> Conversation {
        self.storage
            .load_conversation(CONVERSATION)
            .await
            .unwrap()
            .expect("the turn persisted its conversation")
    }

    fn current_writer(&self) -> u64 {
        self.server.last_writer_conn_id.load(Ordering::SeqCst)
    }

    async fn finish(self) {
        self.shutdown.cancel();
        self.run.abort();
    }
}

// ── Frame helpers ───────────────────────────────────────────────────────────

async fn send(stream: &mut UnixStream, frame: ClientFrame) {
    write_frame(stream, &frame).await.unwrap();
}

async fn next(stream: &mut UnixStream) -> DaemonFrame {
    tokio::time::timeout(DEADLINE, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("a frame within the hang-guard deadline")
        .unwrap()
        .expect("connection stays open")
}

/// Everything the test cares about, accumulated in wire order.
#[derive(Default)]
struct Seen {
    results: Vec<(TurnGeneration, InterruptOutcome)>,
    terminals: Vec<StopReason>,
    projections: Vec<Option<TurnGeneration>>,
    text: String,
}

impl Seen {
    fn take(&mut self, frame: DaemonFrame) {
        match frame {
            DaemonFrame::ActiveTurn { generation } => self.projections.push(generation),
            DaemonFrame::InterruptResult {
                generation,
                outcome,
            } => self.results.push((generation, outcome)),
            DaemonFrame::Event(raw) => match raw.kind {
                RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason }) => {
                    // `ToolUse` ends one provider response, not the turn.
                    if stop_reason != StopReason::ToolUse {
                        self.terminals.push(stop_reason);
                    }
                }
                RawEventKind::Provider(StreamChunk::Text { content, .. }) => {
                    self.text.push_str(&content);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

async fn pump(stream: &mut UnixStream, seen: &mut Seen, done: impl Fn(&Seen) -> bool) {
    while !done(seen) {
        let frame = next(stream).await;
        seen.take(frame);
    }
}

async fn collect(stream: &mut UnixStream, done: impl Fn(&Seen) -> bool) -> Seen {
    let mut seen = Seen::default();
    pump(stream, &mut seen, done).await;
    seen
}

/// The turn is over: its terminal event and the cleared projection arrived.
fn turn_over(seen: &Seen) -> bool {
    seen.terminals.len() == 1 && seen.projections.last() == Some(&None)
}

/// Consume frames until the writer is told the live turn's generation.
async fn live_generation(stream: &mut UnixStream) -> TurnGeneration {
    loop {
        if let DaemonFrame::ActiveTurn {
            generation: Some(generation),
        } = next(stream).await
        {
            return generation;
        }
    }
}

async fn interrupt_answer(stream: &mut UnixStream) -> (TurnGeneration, InterruptOutcome) {
    loop {
        if let DaemonFrame::InterruptResult {
            generation,
            outcome,
        } = next(stream).await
        {
            return (generation, outcome);
        }
    }
}

fn refused(reason: InterruptRefusal) -> InterruptOutcome {
    InterruptOutcome::Refused { reason }
}

fn has_assistant(conversation: &Conversation) -> bool {
    conversation
        .messages
        .iter()
        .any(|message| message.role == MessageRole::Assistant)
}

// ── K8: the attached actual interrupt ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_6_interrupt_while_an_approval_waits_leaves_the_approval_resolvable_or_closed_and_the_turn_not_stuck()
 {
    use crate::adapters::daemon::protocol::{ApprovalRefusal, ExactInputAssembly};
    use crate::domain::models::ApprovalOutcome;

    let rig = Rig::start(vec![Step::ApprovalTool, Step::Text("done")]).await;
    let mut writer = rig.writer(1).await;
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "stop while approval waits".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut writer).await;
    let (request_id, bytes) = loop {
        if let DaemonFrame::ApprovalRequest {
            request_id,
            tool,
            input_bytes,
            ..
        } = next(&mut writer).await
        {
            assert_eq!(tool, "Bash");
            break (request_id, input_bytes.expect("tool input length"));
        }
    };
    let mut input = ExactInputAssembly::new(bytes);
    loop {
        match next(&mut writer).await {
            DaemonFrame::ApprovalInputChunk {
                request_id: id,
                sequence,
                data,
            } => {
                assert_eq!(id, request_id);
                input.push(sequence, &data).unwrap();
            }
            DaemonFrame::ApprovalInputComplete {
                request_id: id,
                chunks,
            } => {
                assert_eq!(id, request_id);
                assert_eq!(
                    input.finish(chunks).unwrap(),
                    serde_json::json!({"command": "echo held"})
                );
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        rig.probes.entered.available_permits(),
        0,
        "the tool has not executed"
    );

    send(&mut writer, ClientFrame::InterruptTurn { generation }).await;
    let mut seen = Seen::default();
    let mut closed = false;
    while seen.results.is_empty() || !turn_over(&seen) {
        let frame = next(&mut writer).await;
        if let DaemonFrame::ApprovalClosed { request_id: id } = &frame {
            assert_eq!(id, &request_id);
            closed = true;
        }
        seen.take(frame);
    }
    assert_eq!(seen.results, [(generation, InterruptOutcome::Interrupted)]);
    assert_eq!(seen.terminals, [StopReason::Cancelled]);

    // The scheduler can cancel the request first, or the turn driver's abort
    // can drop the waiting future first. A late response therefore either
    // resolves the still-pending request or is refused as no longer pending.
    send(
        &mut writer,
        ClientFrame::ApprovalResponse {
            request_id: request_id.clone(),
            outcome: ApprovalOutcome::Once,
        },
    )
    .await;
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "again".into(),
            images: vec![],
        },
    )
    .await;
    let mut followup = Seen::default();
    let mut next_generation = None;
    while !closed || !turn_over(&followup) {
        let frame = next(&mut writer).await;
        match &frame {
            DaemonFrame::ApprovalClosed { request_id: id } => {
                assert_eq!(id, &request_id);
                closed = true;
            }
            DaemonFrame::ApprovalRefused {
                request_id: id,
                reason,
            } => {
                assert_eq!(id, &request_id);
                assert_eq!(*reason, ApprovalRefusal::NoLongerPending);
                closed = true;
            }
            DaemonFrame::ActiveTurn {
                generation: Some(next),
            } => next_generation = Some(*next),
            _ => {}
        }
        followup.take(frame);
    }
    assert!(next_generation.expect("follow-up generation").0 > generation.0);
    assert_eq!(followup.terminals, [StopReason::EndTurn]);
    assert_eq!(followup.text, "done");
    assert_eq!(
        rig.probes.entered.available_permits(),
        0,
        "interrupted tool never executed"
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_writer_interrupt_cancels_a_held_tool_and_the_socket_stays_usable() {
    let rig = Rig::start(vec![Step::Tool, Step::Text("done")]).await;
    let mut writer = rig.writer(1).await;
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "stop me".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut writer).await;
    rig.probes.held_point_reached().await;
    assert_eq!(rig.probes.exited(), 0, "the tool is genuinely running");

    send(&mut writer, ClientFrame::InterruptTurn { generation }).await;
    let seen = collect(&mut writer, |seen| {
        !seen.results.is_empty() && turn_over(seen)
    })
    .await;
    assert_eq!(seen.results, [(generation, InterruptOutcome::Interrupted)]);
    assert_eq!(
        seen.terminals,
        [StopReason::Cancelled],
        "normal terminal event"
    );
    assert_eq!(
        rig.probes.exited(),
        1,
        "the held tool stopped: server work ended"
    );

    let persisted = rig.persisted().await;
    assert!(
        persisted
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "stop me"),
        "the submitted message persisted"
    );
    assert!(
        !persisted.messages.iter().any(|m| {
            m.role == MessageRole::Assistant && m.stop_reason == Some(StopReason::EndTurn)
        }),
        "a cancelled turn commits no answer (only the tool-use response, if any)"
    );

    // Positive control: the very same socket runs the next turn to completion.
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "again".into(),
            images: vec![],
        },
    )
    .await;
    let next_generation = live_generation(&mut writer).await;
    assert!(next_generation.0 > generation.0, "a fresh generation");
    let seen = collect(&mut writer, turn_over).await;
    assert_eq!(seen.terminals, [StopReason::EndTurn]);
    assert_eq!(seen.text, "done");
    assert!(
        rig.persisted()
            .await
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content == "done"),
        "the next turn committed its answer"
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_writer_interrupt_stops_a_provider_stream_that_never_yields() {
    let rig = Rig::start(vec![Step::HeldStream, Step::Text("done")]).await;
    let mut writer = rig.writer(1).await;
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "hang".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut writer).await;
    rig.probes.held_point_reached().await;

    send(&mut writer, ClientFrame::InterruptTurn { generation }).await;
    let seen = collect(&mut writer, |seen| {
        !seen.results.is_empty() && turn_over(seen)
    })
    .await;
    assert_eq!(seen.results, [(generation, InterruptOutcome::Interrupted)]);
    assert_eq!(seen.terminals, [StopReason::Cancelled]);
    assert_eq!(seen.text, "partial", "the streamed prefix reached the view");
    assert_eq!(rig.probes.exited(), 1, "the stalled stream was dropped");
    assert!(
        !has_assistant(&rig.persisted().await),
        "the partial answer is never committed"
    );

    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "again".into(),
            images: vec![],
        },
    )
    .await;
    live_generation(&mut writer).await;
    let seen = collect(&mut writer, turn_over).await;
    assert_eq!(
        (seen.terminals, seen.text),
        (vec![StopReason::EndTurn], "done".to_owned()),
        "the next turn does not inherit the cancelled partial"
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_attach_during_an_active_turn_gets_the_live_generation_and_stops_it() {
    let rig = Rig::start(vec![Step::Tool, Step::Text("done")]).await;
    let mut first = rig.writer(1).await;
    send(
        &mut first,
        ClientFrame::UserMessage {
            text: "held while detached".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut first).await;
    rig.probes.held_point_reached().await;

    // Explicit detach: the held work survives.
    send(&mut first, ClientFrame::Detach).await;
    loop {
        // `Detached`, then EOF once the daemon has deregistered this writer.
        let frame = tokio::time::timeout(DEADLINE, read_frame::<_, DaemonFrame>(&mut first))
            .await
            .expect("detach completes")
            .unwrap();
        if frame.is_none() {
            break;
        }
    }
    assert_eq!(
        rig.probes.exited(),
        0,
        "Detach must keep the held turn alive"
    );

    let mut second = rig.writer(2).await;
    let live = live_generation(&mut second).await;
    assert_eq!(live, generation, "attach projects the SAME live generation");
    send(&mut second, ClientFrame::InterruptTurn { generation: live }).await;
    let seen = collect(&mut second, |seen| {
        !seen.results.is_empty() && turn_over(seen)
    })
    .await;
    assert_eq!(seen.results, [(live, InterruptOutcome::Interrupted)]);
    assert_eq!(seen.terminals, [StopReason::Cancelled]);
    assert_eq!(rig.probes.exited(), 1);

    send(
        &mut second,
        ClientFrame::UserMessage {
            text: "next".into(),
            images: vec![],
        },
    )
    .await;
    live_generation(&mut second).await;
    let seen = collect(&mut second, turn_over).await;
    assert_eq!(seen.terminals, [StopReason::EndTurn]);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_detach_keeps_a_held_turn_alive_through_to_its_persisted_answer() {
    let rig = Rig::start(vec![Step::Tool, Step::Text("survived")]).await;
    let mut first = rig.writer(1).await;
    send(
        &mut first,
        ClientFrame::UserMessage {
            text: "keep going".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut first).await;
    rig.probes.held_point_reached().await;
    send(&mut first, ClientFrame::Detach).await;
    while tokio::time::timeout(DEADLINE, read_frame::<_, DaemonFrame>(&mut first))
        .await
        .expect("detach completes")
        .unwrap()
        .is_some()
    {}

    let mut second = rig.writer(2).await;
    assert_eq!(live_generation(&mut second).await, generation);
    rig.probes.release.add_permits(1);
    let seen = collect(&mut second, turn_over).await;
    assert_eq!(seen.terminals, [StopReason::EndTurn]);
    assert!(
        rig.persisted()
            .await
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content == "survived"),
        "the detached client's turn completed and persisted"
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_unauthorized_stale_duplicate_and_foreign_controls_cancel_nothing() {
    let rig = Rig::start(vec![Step::Tool, Step::Tool, Step::Text("done")]).await;
    let mut writer = rig.writer(1).await;
    // The client CLAIMS it may write; the daemon alone decides the grant.
    let (mut reader, reader_mode) = rig.attach(2, false, ConnectionTier::TrustedLocal).await;
    assert_eq!(reader_mode, AttachMode::ReadOnly);
    let (mut peer, peer_mode) = rig.attach(3, false, ConnectionTier::Peer).await;
    assert_eq!(peer_mode, AttachMode::ReadOnly);

    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "turn one".into(),
            images: vec![],
        },
    )
    .await;
    let first = live_generation(&mut writer).await;
    rig.probes.held_point_reached().await;

    // ReadOnly, Peer and a never-minted generation: all refused, nothing stops.
    let mut reader_seen = Seen::default();
    send(
        &mut reader,
        ClientFrame::InterruptTurn { generation: first },
    )
    .await;
    pump(&mut reader, &mut reader_seen, |seen| {
        seen.results.len() == 1
    })
    .await;
    assert_eq!(
        reader_seen.results[0],
        (first, refused(InterruptRefusal::ReadOnly))
    );
    send(&mut peer, ClientFrame::InterruptTurn { generation: first }).await;
    assert_eq!(
        interrupt_answer(&mut peer).await,
        (first, refused(InterruptRefusal::PeerTier))
    );
    let bogus = TurnGeneration(first.0 + 1000);
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: bogus },
    )
    .await;
    assert_eq!(
        interrupt_answer(&mut writer).await,
        (bogus, refused(InterruptRefusal::Stale))
    );
    assert_eq!(rig.probes.exited(), 0, "no refused control cancelled work");

    // The matching writer interrupt works (the duplicate is covered, with a
    // deterministic order, by the queued-duplicate test below).
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: first },
    )
    .await;
    let seen = collect(&mut writer, |seen| {
        seen.results.len() == 1 && turn_over(seen)
    })
    .await;
    assert_eq!(seen.results, [(first, InterruptOutcome::Interrupted)]);
    assert_eq!(rig.probes.exited(), 1);

    // After the turn ended, its generation names nothing.
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: first },
    )
    .await;
    assert_eq!(
        interrupt_answer(&mut writer).await,
        (first, refused(InterruptRefusal::NoActiveTurn))
    );

    // Turn two: the OLD generation must never cancel the NEWER turn.
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "turn two".into(),
            images: vec![],
        },
    )
    .await;
    let second = live_generation(&mut writer).await;
    assert_ne!(second, first);
    rig.probes.held_point_reached().await;
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: first },
    )
    .await;
    assert_eq!(
        interrupt_answer(&mut writer).await,
        (first, refused(InterruptRefusal::Stale))
    );
    send(
        &mut reader,
        ClientFrame::InterruptTurn { generation: second },
    )
    .await;
    pump(&mut reader, &mut reader_seen, |seen| {
        seen.results.len() == 2
    })
    .await;
    assert_eq!(
        reader_seen.results[1],
        (second, refused(InterruptRefusal::ReadOnly))
    );
    assert_eq!(rig.probes.exited(), 1, "turn two is still held");

    // Positive control: turn two runs to its normal end, uncancelled.
    rig.probes.release.add_permits(1);
    let seen = collect(&mut writer, turn_over).await;
    assert_eq!(seen.terminals, [StopReason::EndTurn]);
    assert_eq!(seen.text, "done");

    // Only the writer was ever told a generation.
    pump(&mut reader, &mut reader_seen, |seen| {
        seen.terminals.len() == 2
    })
    .await;
    assert!(
        reader_seen.projections.is_empty(),
        "read-only: no projection"
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_duplicate_interrupt_is_refused_and_fires_the_token_only_once() {
    let rig = Rig::start(vec![Step::Tool, Step::Text("done")]).await;
    let mut writer = rig.writer(1).await;
    let writer_id = rig.current_writer();
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "held".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut writer).await;
    rig.probes.held_point_reached().await;

    // Both interrupts queue at the fence before the turn can end: the driver's
    // own registry access queues behind them, so the order is fixed.
    let (reply_tx, mut reply_rx) = mpsc::channel(4);
    let admission = rig.server.registry.lock().await;
    let mut first = Box::pin(
        rig.server
            .answer_interrupt(writer_id, &reply_tx, generation),
    );
    assert!(futures::poll!(first.as_mut()).is_pending());
    let mut second = Box::pin(
        rig.server
            .answer_interrupt(writer_id, &reply_tx, generation),
    );
    assert!(futures::poll!(second.as_mut()).is_pending());
    drop(admission);
    first.await;
    second.await;
    assert_eq!(answer_of(&mut reply_rx), InterruptOutcome::Interrupted);
    assert_eq!(
        answer_of(&mut reply_rx),
        refused(InterruptRefusal::AlreadyInterrupted)
    );

    let seen = collect(&mut writer, turn_over).await;
    assert_eq!(seen.terminals, [StopReason::Cancelled]);
    assert_eq!(rig.probes.exited(), 1, "one cancellation, one tool stopped");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_a_channel_origin_turn_is_wrong_origin_and_keeps_running() {
    let rig = Rig::start(vec![Step::HeldStream]).await;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let channel_turn = {
        let server = rig.server.clone();
        tokio::spawn(async move {
            server
                .drive_channel_turn(ChannelTurnRequest {
                    text: "from telegram".into(),
                    origin: ChannelKind::Telegram,
                    response_tx,
                })
                .await;
        })
    };
    rig.probes.held_point_reached().await;

    let mut writer = rig.writer(1).await;
    // The channel turn holds the first minted generation; the writer was never
    // told it, but a guessed match must still not cancel a foreign turn.
    let guess = TurnGeneration(1);
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: guess },
    )
    .await;
    assert_eq!(
        interrupt_answer(&mut writer).await,
        (guess, refused(InterruptRefusal::WrongOrigin))
    );
    let other = TurnGeneration(2);
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: other },
    )
    .await;
    assert_eq!(
        interrupt_answer(&mut writer).await,
        (other, refused(InterruptRefusal::Stale))
    );
    assert_eq!(rig.probes.exited(), 0, "the channel turn was not touched");

    rig.probes.release.add_permits(1);
    let answer = tokio::time::timeout(DEADLINE, response_rx)
        .await
        .expect("channel turn finishes")
        .expect("channel response delivered");
    assert_eq!(answer, "partial");
    tokio::time::timeout(DEADLINE, channel_turn)
        .await
        .expect("the channel turn task never finished")
        .unwrap();
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_interrupt_bypasses_an_operator_fifo_blocked_behind_the_active_turn() {
    let rig = Rig::start(vec![Step::Tool, Step::Text("queued answer")]).await;
    let mut writer = rig.writer(1).await;
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "first".into(),
            images: vec![],
        },
    )
    .await;
    let first = live_generation(&mut writer).await;
    rig.probes.held_point_reached().await;
    // Queued behind the held turn in the operator FIFO: the worker is busy.
    send(
        &mut writer,
        ClientFrame::UserMessage {
            text: "second".into(),
            images: vec![],
        },
    )
    .await;
    // The held tool only ends on cancellation, so this interrupt reaching the
    // daemon (outside the blocked FIFO) is the only way anything proceeds.
    send(
        &mut writer,
        ClientFrame::InterruptTurn { generation: first },
    )
    .await;
    let mut seen = Seen::default();
    while !(seen.results.len() == 1 && seen.terminals.len() == 2) {
        seen.take(next(&mut writer).await);
    }
    assert_eq!(seen.results, [(first, InterruptOutcome::Interrupted)]);
    assert_eq!(
        seen.terminals,
        [StopReason::Cancelled, StopReason::EndTurn],
        "the cancelled turn, then the queued message as its own turn"
    );
    assert_eq!(seen.text, "queued answer");
    let second = seen
        .projections
        .iter()
        .flatten()
        .copied()
        .find(|generation| *generation != first)
        .expect("the queued turn got its own generation");
    assert!(second.0 > first.0);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_paused_old_writer_interrupt_cannot_fire_after_the_handoff() {
    let rig = Rig::start(vec![Step::Tool, Step::Text("done")]).await;
    let mut old_writer = rig.writer(1).await;
    let old_id = rig.current_writer();
    send(
        &mut old_writer,
        ClientFrame::UserMessage {
            text: "held".into(),
            images: vec![],
        },
    )
    .await;
    let generation = live_generation(&mut old_writer).await;
    rig.probes.held_point_reached().await;

    // Park the old writer's interrupt before the linearized fence.
    let gate = rig.server.interrupt_probe.arm(old_id).await;
    send(&mut old_writer, ClientFrame::InterruptTurn { generation }).await;
    tokio::time::timeout(DEADLINE, gate.reached.acquire())
        .await
        .expect("the paused interrupt never reached the fence hook")
        .expect("open")
        .forget();

    // Replacement admitted: the old writer is dropped as a slow consumer is.
    rig.server.registry.lock().await.remove(old_id);
    let mut new_writer = rig.writer(2).await;
    assert_ne!(rig.current_writer(), old_id);
    assert_eq!(
        live_generation(&mut new_writer).await,
        generation,
        "the replacement learns the live turn"
    );

    // Release the stale interrupt: it must not fire the token.
    gate.release.add_permits(1);
    assert_eq!(
        interrupt_answer(&mut old_writer).await,
        (generation, refused(InterruptRefusal::NotWriter)),
        "the old writer still hears an honest refusal"
    );
    assert_eq!(rig.probes.exited(), 0, "old writer cancelled nothing");

    // Positive control: the replacement can stop the very same turn.
    send(&mut new_writer, ClientFrame::InterruptTurn { generation }).await;
    let seen = collect(&mut new_writer, |seen| {
        !seen.results.is_empty() && turn_over(seen)
    })
    .await;
    assert_eq!(seen.results, [(generation, InterruptOutcome::Interrupted)]);
    assert_eq!(seen.terminals, [StopReason::Cancelled]);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_old_protocol_version_is_rejected_by_negotiation() {
    let rig = Rig::start(vec![Step::Text("done")]).await;
    let mut stream = UnixStream::connect(&rig.socket).await.unwrap();
    let signer = test_signer(9);
    let nonce = match next(&mut stream).await {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("expected AttachChallenge, got {other:?}"),
    };
    let old = PROTOCOL_VERSION - 1;
    let proof = signer.attach_proof(&nonce, old, ConnectionTier::TrustedLocal.proof_tag(), false);
    send(
        &mut stream,
        ClientFrame::Attach {
            protocol_version: old,
            read_only_ok: false,
            tier: ConnectionTier::TrustedLocal,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await;
    assert!(matches!(
        next(&mut stream).await,
        DaemonFrame::Error(ProtocolError::VersionMismatch { daemon, client })
            if daemon == PROTOCOL_VERSION && client == old
    ));
    rig.finish().await;
}

// ── The fence itself: deterministic interleavings, no sockets ───────────────

struct Fence {
    server: Arc<AttachServer>,
    cancel: CancellationToken,
    generation: TurnGeneration,
    writer_rx: mpsc::Receiver<DaemonFrame>,
    _workspace: tempfile::TempDir,
}

async fn fence_with_active_turn() -> Fence {
    let workspace = tempfile::tempdir().unwrap();
    let (core, _) = super::super::tests::mock_core(workspace.path(), vec![]);
    let conversation = Arc::new(Mutex::new(Conversation::default()));
    let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
    let server = AttachServer::new(core, conversation, domain_tx);
    let (tx, writer_rx) = mpsc::channel(8);
    server.registry.lock().await.conns.push(Conn {
        id: 1,
        tx,
        terminal_tx: watch::channel(None).0,
        mode: AttachMode::ReadWrite,
        tier: ConnectionTier::TrustedLocal,
        approval_ready: true,
    });
    server.last_writer_conn_id.store(1, Ordering::SeqCst);
    let cancel = CancellationToken::new();
    let generation = server.registry.lock().await.begin_turn(Some(&cancel));
    Fence {
        server,
        cancel,
        generation,
        writer_rx,
        _workspace: workspace,
    }
}

fn answer_of(reply: &mut mpsc::Receiver<DaemonFrame>) -> InterruptOutcome {
    match reply.try_recv() {
        Ok(DaemonFrame::InterruptResult { outcome, .. }) => outcome,
        other => panic!("expected an InterruptResult, got {other:?}"),
    }
}

#[tokio::test]
async fn story21_2_handoff_queued_before_the_interrupt_wins_and_the_old_writer_cannot_fire() {
    let fence = fence_with_active_turn().await;
    let (reply_tx, mut reply_rx) = mpsc::channel(4);
    let admission = fence.server.registry.lock().await; // a handoff holds the fence
    let mut handoff = Box::pin({
        let server = fence.server.clone();
        async move { server.registry.lock().await.remove(1) }
    });
    assert!(
        futures::poll!(handoff.as_mut()).is_pending(),
        "handoff queues first"
    );
    let mut interrupt = Box::pin(
        fence
            .server
            .answer_interrupt(1, &reply_tx, fence.generation),
    );
    assert!(
        futures::poll!(interrupt.as_mut()).is_pending(),
        "interrupt queues second"
    );
    drop(admission);
    handoff.await;
    interrupt.await;
    assert_eq!(
        answer_of(&mut reply_rx),
        refused(InterruptRefusal::NotWriter)
    );
    assert!(!fence.cancel.is_cancelled(), "the old writer fired nothing");
}

#[tokio::test]
async fn story21_2_interrupt_queued_before_the_handoff_linearizes_first_and_fires_once() {
    let fence = fence_with_active_turn().await;
    let (reply_tx, mut reply_rx) = mpsc::channel(4);
    let admission = fence.server.registry.lock().await;
    let mut interrupt = Box::pin(
        fence
            .server
            .answer_interrupt(1, &reply_tx, fence.generation),
    );
    assert!(
        futures::poll!(interrupt.as_mut()).is_pending(),
        "interrupt queues first"
    );
    let mut handoff = Box::pin({
        let server = fence.server.clone();
        async move { server.registry.lock().await.remove(1) }
    });
    assert!(
        futures::poll!(handoff.as_mut()).is_pending(),
        "handoff queues second"
    );
    assert!(
        !fence.cancel.is_cancelled(),
        "nothing fires while the fence is held by someone else"
    );
    drop(admission);
    interrupt.await;
    assert!(
        fence.cancel.is_cancelled(),
        "authorization, match and firing completed before the handoff ran"
    );
    handoff.await;
    assert_eq!(answer_of(&mut reply_rx), InterruptOutcome::Interrupted);
    assert!(fence.server.registry.lock().await.conns.is_empty());
}

#[tokio::test]
async fn story21_2_fence_refuses_every_unauthorized_or_stale_control_without_firing() {
    let mut fence = fence_with_active_turn().await;
    let _ = fence.writer_rx.try_recv(); // the begin projection
    let (ro_tx, _ro_rx) = mpsc::channel(8);
    let (peer_tx, _peer_rx) = mpsc::channel(8);
    let (closing_tx, _closing_rx) = mpsc::channel(8);
    {
        let mut registry = fence.server.registry.lock().await;
        for (id, tx, mode, tier) in [
            (2, ro_tx, AttachMode::ReadOnly, ConnectionTier::TrustedLocal),
            (3, peer_tx, AttachMode::ReadOnly, ConnectionTier::Peer),
            (
                4,
                closing_tx,
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
            ),
        ] {
            registry.conns.push(Conn {
                id,
                tx,
                terminal_tx: watch::channel(None).0,
                mode,
                tier,
                approval_ready: true,
            });
        }
        // Connection 4 claims read-write but is not the current writer (1 is).
        let generation = fence.generation;
        let cases = [
            (99, 1, generation, InterruptRefusal::NotWriter),
            (2, 1, generation, InterruptRefusal::ReadOnly),
            (3, 1, generation, InterruptRefusal::PeerTier),
            (4, 1, generation, InterruptRefusal::NotWriter),
            (1, 7, generation, InterruptRefusal::NotWriter),
            (
                1,
                1,
                TurnGeneration(generation.0 + 1),
                InterruptRefusal::Stale,
            ),
        ];
        for (conn, writer, target, reason) in cases {
            assert_eq!(
                registry.interrupt_terminal_turn(conn, writer, target),
                refused(reason),
                "conn {conn} writer {writer}"
            );
            assert!(!fence.cancel.is_cancelled(), "{reason:?} must not fire");
        }
        // A writer whose connection is already terminating is not a writer.
        registry.conns[0]
            .terminal_tx
            .send_replace(Some(ProtocolError::ReadOnly));
        assert_eq!(
            registry.interrupt_terminal_turn(1, 1, generation),
            refused(InterruptRefusal::NotWriter)
        );
        registry.conns[0].terminal_tx.send_replace(None);
        assert!(!fence.cancel.is_cancelled());
        // Positive control, then the duplicate.
        assert_eq!(
            registry.interrupt_terminal_turn(1, 1, generation),
            InterruptOutcome::Interrupted
        );
        assert!(fence.cancel.is_cancelled());
        assert_eq!(
            registry.interrupt_terminal_turn(1, 1, generation),
            refused(InterruptRefusal::AlreadyInterrupted)
        );
    }
}

#[tokio::test]
async fn story21_2_slot_clears_only_for_its_own_generation_and_projects_to_the_writer() {
    let mut fence = fence_with_active_turn().await;
    assert!(matches!(
        fence.writer_rx.try_recv(),
        Ok(DaemonFrame::ActiveTurn { generation: Some(g) }) if g == fence.generation
    ));
    let newer_cancel = CancellationToken::new();
    let mut registry = fence.server.registry.lock().await;
    let newer = registry.begin_turn(Some(&newer_cancel));
    assert!(newer.0 > fence.generation.0);
    let _ = fence.writer_rx.try_recv();
    // The older turn finishing late must not clear the newer slot.
    registry.end_turn(fence.generation);
    assert!(
        fence.writer_rx.try_recv().is_err(),
        "a mismatched end projects nothing"
    );
    assert_eq!(
        registry.interrupt_terminal_turn(1, 1, fence.generation),
        refused(InterruptRefusal::Stale)
    );
    assert_eq!(
        registry.interrupt_terminal_turn(1, 1, newer),
        InterruptOutcome::Interrupted
    );
    assert!(newer_cancel.is_cancelled() && !fence.cancel.is_cancelled());
    registry.end_turn(newer);
    assert!(matches!(
        fence.writer_rx.try_recv(),
        Ok(DaemonFrame::ActiveTurn { generation: None })
    ));
    assert_eq!(
        registry.interrupt_terminal_turn(1, 1, newer),
        refused(InterruptRefusal::NoActiveTurn)
    );
}

#[tokio::test]
async fn story21_2_only_a_trusted_local_writer_is_told_the_live_generation() {
    let workspace = tempfile::tempdir().unwrap();
    let (core, _) = super::super::tests::mock_core(workspace.path(), vec![]);
    let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
    let server = AttachServer::new(
        core,
        Arc::new(Mutex::new(Conversation::default())),
        domain_tx,
    );
    let mut registry = server.registry.lock().await;
    let cancel = CancellationToken::new();
    let generation = registry.begin_turn(Some(&cancel));
    let mut receivers = Vec::new();
    for (id, mode, tier) in [
        (1, AttachMode::ReadOnly, ConnectionTier::TrustedLocal),
        (2, AttachMode::ReadOnly, ConnectionTier::Peer),
        (3, AttachMode::ReadWrite, ConnectionTier::TrustedLocal),
    ] {
        let (tx, rx) = mpsc::channel(8);
        registry.conns.push(Conn {
            id,
            tx,
            terminal_tx: watch::channel(None).0,
            mode,
            tier,
            approval_ready: true,
        });
        registry.project_active_turn_to(id);
        receivers.push(rx);
    }
    assert!(receivers[0].try_recv().is_err(), "read-only is not told");
    assert!(receivers[1].try_recv().is_err(), "peer is not told");
    assert!(matches!(
        receivers[2].try_recv(),
        Ok(DaemonFrame::ActiveTurn { generation: Some(g) }) if g == generation
    ));
    // A non-terminal turn is never projected.
    registry.end_turn(generation);
    let _ = receivers[2].try_recv();
    registry.begin_turn(None);
    registry.project_active_turn_to(3);
    assert!(
        receivers[2].try_recv().is_err(),
        "channel turns are not offered"
    );
}

/// Keystone for the `run_turn` loop-top guard: a tool that reports its
/// cancellation as an ERROR lets the scheduler surface `ToolCall::Error`, and
/// without the guard the loop would then start a second provider call and may
/// finish a turn already reported `Interrupted`. Many rounds, because the
/// scheduler's own cancel-vs-execute branch choice is random.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_interrupt_starts_no_second_provider_call_when_a_tool_errors_on_cancel() {
    const ROUNDS: usize = 24;
    let rig = Rig::start(vec![Step::Tool]).await;
    rig.probes.fail_on_cancel.store(true, Ordering::SeqCst);
    let mut writer = rig.writer(1).await;
    for round in 1..=ROUNDS {
        send(
            &mut writer,
            ClientFrame::UserMessage {
                text: format!("round {round}"),
                images: vec![],
            },
        )
        .await;
        let generation = live_generation(&mut writer).await;
        rig.probes.held_point_reached().await;
        send(&mut writer, ClientFrame::InterruptTurn { generation }).await;
        let seen = collect(&mut writer, |seen| {
            !seen.results.is_empty() && turn_over(seen)
        })
        .await;
        assert_eq!(
            seen.results,
            [(generation, InterruptOutcome::Interrupted)],
            "round {round}"
        );
        assert_eq!(seen.terminals, [StopReason::Cancelled], "round {round}");
        assert_eq!(
            rig.probes.stream_calls.load(Ordering::SeqCst),
            round,
            "round {round}: an interrupted turn must not start another provider call"
        );
    }
    rig.finish().await;
}

/// A-1: the interrupted turn's fold must not be able to wake the NEXT queued
/// turn's fold wait. Many fresh rigs, because the race is a scheduling one: each
/// round queues a second message behind the held first turn, interrupts the first
/// and requires exactly one terminal per turn (`Cancelled`, then `EndTurn`) and
/// the second answer committed to the persisted conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn story21_2_interrupted_turn_leaves_the_queued_turn_its_own_fold_and_commit() {
    const ROUNDS: usize = 12;
    for round in 1..=ROUNDS {
        let rig = Rig::start(vec![Step::Tool, Step::Text("queued answer")]).await;
        rig.probes.fail_on_cancel.store(true, Ordering::SeqCst);
        let mut writer = rig.writer(1).await;
        send(
            &mut writer,
            ClientFrame::UserMessage {
                text: "first".into(),
                images: vec![],
            },
        )
        .await;
        let first = live_generation(&mut writer).await;
        rig.probes.held_point_reached().await;
        send(
            &mut writer,
            ClientFrame::UserMessage {
                text: "second".into(),
                images: vec![],
            },
        )
        .await;
        send(
            &mut writer,
            ClientFrame::InterruptTurn { generation: first },
        )
        .await;
        let mut seen = Seen::default();
        while !(seen.results.len() == 1 && seen.terminals.len() == 2) {
            seen.take(next(&mut writer).await);
        }
        assert_eq!(
            seen.terminals,
            [StopReason::Cancelled, StopReason::EndTurn],
            "round {round}: one terminal per turn"
        );
        assert_eq!(seen.text, "queued answer", "round {round}");
        // The commit follows the terminal; wait (bounded) for it to be saved.
        tokio::time::timeout(DEADLINE, async {
            loop {
                let saved = rig.storage.load_conversation(CONVERSATION).await;
                if let Ok(Some(conversation)) = saved
                    && serde_json::to_string(&conversation)
                        .unwrap()
                        .contains("queued answer")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("round {round}: the queued answer was never committed"));
        rig.finish().await;
    }
}

struct C2bReplyProvider {
    calls: AtomicUsize,
    disconnect: bool,
}

#[async_trait::async_trait]
impl StreamingProvider for C2bReplyProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        let chunks = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => vec![
                StreamChunk::Text {
                    content: "Checking.".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::ToolUse {
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "fixture.txt"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            1 if self.disconnect => vec![],
            1 => vec![
                StreamChunk::Text {
                    content: "Answer: 42".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
            n => panic!("unexpected provider call {n}"),
        };
        Ok(futures::stream::iter(chunks).boxed())
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "c2b-reply".into()
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

struct C2bReadFile;

#[async_trait::async_trait]
impl ToolSetPort for C2bReadFile {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_file".into(),
            description: "Return the fixture result".into(),
            input_schema: serde_json::json!({"type": "object"}),
            parallel_safe: true,
        }]
    }

    async fn execute(
        &self,
        tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        assert_eq!(tool_name, "read_file");
        Ok(ToolResult {
            tool_use_id: "t1".into(),
            content: "SECRET-RESULT-C2B".into(),
            is_error: false,
            images: vec![],
        })
    }
}

async fn c2b_channel_tool_turn_case(disconnect: bool) {
    let workspace = tempfile::Builder::new()
        .prefix("c-2b-channel-")
        .tempdir_in("/tmp")
        .unwrap();
    let ws = workspace.path().to_path_buf();
    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        crate::infrastructure::paths::sessions_dir(&ws),
        ws.clone(),
    ));
    // `read_file` is an unknown (elevated) name to the permission chain.
    // Yolo lets the fixture reach the real tool scheduler without an operator.
    let security = crate::adapters::security_adapter::SecurityAdapter::new(ws.clone());
    crate::domain::ports::SecurityPort::set_mode(
        &security,
        crate::domain::models::PermissionMode::Yolo,
    );
    let security: Arc<dyn crate::domain::ports::SecurityPort> = Arc::new(security);
    let factory_security = security.clone();
    let provider = Arc::new(C2bReplyProvider {
        calls: AtomicUsize::new(0),
        disconnect,
    });
    let factory_provider = provider.clone();
    let factory_storage = storage.clone();
    let factory_ws = ws.clone();
    let core = Arc::new(DaemonCore::new(
        ws.clone(),
        Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        Arc::new(NoOpMemory),
        storage.clone(),
        security,
        Arc::new(NoOpPersona),
        Arc::new(crate::adapters::rap::PeerTopicStore::new()),
        Box::new(move || {
            let mut runtime = Arc::try_unwrap(mock_runtime(
                factory_provider.clone(),
                factory_storage.clone(),
                &factory_ws,
            ))
            .ok()
            .expect("new unshared runtime");
            let tools: Arc<dyn ToolSetPort> = Arc::new(C2bReadFile);
            runtime.security = factory_security.clone();
            runtime.tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
                runtime.security.clone(),
                tools.clone(),
                runtime.approval.clone(),
                64,
            );
            runtime.tools = tools;
            Ok(Arc::new(runtime))
        }),
    ));
    let conversation = Arc::new(Mutex::new(Conversation {
        id: "c2b-channel".into(),
        ..Default::default()
    }));
    let (bus, domain_rx) = EventBus::new(64);
    let (channel_tx, channel_rx) = mpsc::unbounded_channel();
    let listener = UnixListener::bind(ws.join("c2b-channel.sock")).unwrap();
    let server = AttachServer::new(core, conversation.clone(), bus.domain_tx.clone());
    let shutdown = CancellationToken::new();
    let run = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            server
                .run(listener, domain_rx, Some(channel_rx), None, shutdown)
                .await
        })
    };

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    channel_tx
        .send(ChannelTurnRequest {
            text: "What is the answer?".into(),
            origin: ChannelKind::Telegram,
            response_tx,
        })
        .unwrap();
    let response = tokio::time::timeout(DEADLINE, response_rx)
        .await
        .expect("channel reply timed out")
        .expect("channel reply sender dropped");
    assert_eq!(
        response,
        if disconnect {
            "Checking.\n[tool use: read_file (id: t1)]\n\n\n(The agent stopped before finishing this reply.)"
        } else {
            "Checking.\n[tool use: read_file (id: t1)]\nAnswer: 42"
        }
    );
    assert!(!response.contains("SECRET-RESULT-C2B"));
    assert!(!response.contains("[tool result"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    // A completed reply is also a fence for the forwarder's final commit.
    let conv = conversation.lock().await;
    let assistants: Vec<_> = conv
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    assert_eq!(assistants.len(), if disconnect { 1 } else { 2 });
    assert_eq!(assistants[0].content, "Checking.");
    assert_eq!(assistants[0].stop_reason, Some(StopReason::ToolUse));
    assert_eq!(assistants[0].tool_calls.len(), 1);
    assert_eq!(assistants[0].tool_calls[0].wire_id(), "t1");
    assert_eq!(assistants[0].tool_calls[0].name, "read_file");
    assert_eq!(
        assistants[0].tool_calls[0].result.as_ref().unwrap().content,
        "SECRET-RESULT-C2B"
    );
    if !disconnect {
        assert_eq!(assistants[1].content, "Answer: 42");
        assert_eq!(assistants[1].stop_reason, Some(StopReason::EndTurn));
        assert!(assistants[1].tool_calls.is_empty());
    }
    drop(conv);
    shutdown.cancel();
    tokio::time::timeout(DEADLINE, run)
        .await
        .expect("server shutdown timed out")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2b_channel_returns_whole_tool_turn_without_tool_result() {
    c2b_channel_tool_turn_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2b_channel_second_response_disconnect_returns_whole_partial_reply() {
    c2b_channel_tool_turn_case(true).await;
}
mod c2b_budget_channel {
    use super::*;
    use crate::domain::errors::StorageError;
    use crate::domain::models::pricing::PricingConfig;
    use crate::domain::models::usage::{TokenUsage, UsageLedgerEntry};
    use crate::domain::models::{EscalationReason, ModelTier};
    use crate::domain::ports::UsageLedgerPort;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct C2bBudgetChannelLedger {
        rows: tokio::sync::Mutex<Vec<UsageLedgerEntry>>,
        unreadable: bool,
        last_since: AtomicI64,
    }

    #[async_trait::async_trait]
    impl UsageLedgerPort for C2bBudgetChannelLedger {
        async fn append(&self, entry: UsageLedgerEntry) -> Result<(), StorageError> {
            self.rows.lock().await.push(entry);
            Ok(())
        }

        async fn read_session(
            &self,
            session_id: &str,
        ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
            Ok(self
                .rows
                .lock()
                .await
                .iter()
                .filter(|row| row.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn read_since(
            &self,
            since_unix_ms: i64,
            _: crate::domain::ports::LedgerReadMode,
        ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
            self.last_since.store(since_unix_ms, Ordering::SeqCst);
            if self.unreadable {
                return Err(StorageError::Other(
                    "C2bBudgetChannel ledger unavailable".into(),
                ));
            }
            Ok(self
                .rows
                .lock()
                .await
                .iter()
                .filter(|row| row.timestamp_ms >= since_unix_ms)
                .cloned()
                .collect())
        }
    }

    fn c2b_budget_channel_row(timestamp_ms: i64, tokens_in: u32) -> UsageLedgerEntry {
        UsageLedgerEntry {
            timestamp_ms,
            session_id: "C2bBudgetChannel-session".into(),
            conversation_id: "C2bBudgetChannel-conversation".into(),
            provider_id: "C2bBudgetChannel-provider".into(),
            model: "C2bBudgetChannel-model".into(),
            tier: ModelTier::Flagship,
            step_kind: None,
            escalation_reason: EscalationReason::None,
            usage: TokenUsage {
                tokens_in,
                tokens_out: 0,
                parent_ctx: 0,
                cache_creation_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
            },
            provenance: None,
        }
    }

    async fn c2b_budget_channel_case(
        limit: Option<f64>,
        unreadable: bool,
        today_tokens: u32,
        yesterday_tokens: u32,
        expected: &str,
        expected_calls: usize,
    ) {
        c2b_budget_channel_case_inner(
            limit,
            unreadable,
            today_tokens,
            yesterday_tokens,
            expected,
            expected_calls,
            false,
        )
        .await;
    }

    async fn c2b_budget_channel_case_inner(
        limit: Option<f64>,
        unreadable: bool,
        today_tokens: u32,
        yesterday_tokens: u32,
        expected: &str,
        expected_calls: usize,
        crossing: bool,
    ) {
        let workspace = tempfile::Builder::new()
            .prefix("c-2b-budget-channel-")
            .tempdir_in("/tmp")
            .unwrap();
        let day_start = crate::infrastructure::clock_util::today_start_unix_ms();
        let ledger = Arc::new(C2bBudgetChannelLedger {
            rows: tokio::sync::Mutex::new(vec![
                c2b_budget_channel_row(day_start, today_tokens),
                c2b_budget_channel_row(day_start - 1, yesterday_tokens),
            ]),
            unreadable,
            last_since: AtomicI64::new(-1),
        });
        let probes = Probes::new();
        let provider = Arc::new(HeldProvider {
            calls: AtomicUsize::new(0),
            script: vec![if crossing {
                Step::Billable
            } else {
                Step::Text("C2bBudgetChannel normal reply")
            }],
            probes: probes.clone(),
        });
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace.path()),
            workspace.path().to_path_buf(),
        ));
        let mut config = AppConfig::default();
        config.model = "C2bBudgetChannel-model".into();
        config.budget.daily_limit_usd = limit;
        config.pricing.insert(
            "C2bBudgetChannel-model".into(),
            PricingConfig {
                input_per_million: 1.0,
                output_per_million: 1.0,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        let config = Arc::new(ArcSwap::from_pointee(config));
        let ws = workspace.path().to_path_buf();
        let factory_storage = storage.clone();
        let factory_config = config.clone();
        let factory_ledger = ledger.clone();
        let core = Arc::new(DaemonCore::new(
            ws.clone(),
            config,
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                let mut runtime =
                    Arc::try_unwrap(mock_runtime(provider.clone(), factory_storage.clone(), &ws))
                        .ok()
                        .expect("new unshared runtime");
                runtime.app_config = factory_config.clone();
                runtime.usage_ledger = factory_ledger.clone();
                Ok(Arc::new(runtime))
            }),
        ));
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "C2bBudgetChannel-conversation".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let (channel_tx, channel_rx) = mpsc::unbounded_channel::<ChannelTurnRequest>();
        let listener =
            UnixListener::bind(workspace.path().join("c2b-budget-channel.sock")).unwrap();
        let server = AttachServer::new(core, conversation.clone(), bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                server
                    .run(listener, domain_rx, Some(channel_rx), None, shutdown)
                    .await
            }
        });
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        channel_tx
            .send(ChannelTurnRequest {
                text: "C2bBudgetChannel request".into(),
                origin: ChannelKind::Telegram,
                response_tx,
            })
            .unwrap();
        if crossing {
            probes.held_point_reached().await;
        }
        let second = if crossing {
            let (tx, rx) = tokio::sync::oneshot::channel();
            channel_tx
                .send(ChannelTurnRequest {
                    text: "second request".into(),
                    origin: ChannelKind::Telegram,
                    response_tx: tx,
                })
                .unwrap();
            Some(rx)
        } else {
            None
        };
        if crossing {
            probes.release.add_permits(1);
        }
        let reply = tokio::time::timeout(DEADLINE, response_rx)
            .await
            .expect("channel reply timed out")
            .expect("channel response sender dropped");
        assert_eq!(reply, expected);
        if let Some(second) = second {
            let reply = tokio::time::timeout(DEADLINE, second)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                reply,
                "Daily budget of $1.00 reached ($1.00 spent today). Raise [budget] daily_limit_usd or wait until tomorrow."
            );
            assert!(
                ledger
                    .rows
                    .lock()
                    .await
                    .iter()
                    .any(|row| row.usage.tokens_in == 10_000)
            );
        }
        assert_eq!(probes.stream_calls.load(Ordering::SeqCst), expected_calls);
        if expected_calls == 0 {
            assert!(
                conversation.lock().await.messages.is_empty(),
                "refusal must not append a user or assistant message"
            );
            assert_eq!(ledger.last_since.load(Ordering::SeqCst), day_start);
        }
        shutdown.cancel();
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c2b_budget_channel_equal_and_above_limit_refused() {
        c2b_budget_channel_case(Some(1.0), false, 1_000_000, 0,
            "Daily budget of $1.00 reached ($1.00 spent today). Raise [budget] daily_limit_usd or wait until tomorrow.", 0).await;
        c2b_budget_channel_case(Some(1.0), false, 1_500_000, 0,
            "Daily budget of $1.00 reached ($1.50 spent today). Raise [budget] daily_limit_usd or wait until tomorrow.", 0).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c2b_budget_channel_unreadable_ledger_refused() {
        c2b_budget_channel_case(
            Some(1.0),
            true,
            0,
            0,
            "Daily budget cannot be checked: usage ledger unreadable.",
            0,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c2b_budget_channel_below_limit_and_yesterday_are_allowed() {
        c2b_budget_channel_case(
            Some(1.0),
            false,
            990_000,
            50_000_000,
            "C2bBudgetChannel normal reply",
            1,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c2b_budget_channel_unset_limit_allows_unreadable_ledger() {
        c2b_budget_channel_case(
            None,
            true,
            50_000_000,
            0,
            "C2bBudgetChannel normal reply",
            1,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_message_sees_previous_turns_ledger_row() {
        c2b_budget_channel_case_inner(
            Some(1.0),
            false,
            990_000,
            0,
            "C2bBudgetChannel normal reply",
            1,
            true,
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_completion_does_not_settle_an_active_channel_turn() {
    let rig = Rig::start(vec![Step::HeldStream]).await;
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let server = rig.server.clone();
    let task = tokio::spawn(async move {
        server
            .drive_channel_turn(ChannelTurnRequest {
                text: "operator".into(),
                origin: ChannelKind::Telegram,
                response_tx: tx,
            })
            .await;
    });
    rig.probes.held_point_reached().await;
    rig.server
        .collect_peer_chunk(
            &Arc::new(AgentId::new()),
            &StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled,
            },
        )
        .await;
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(!task.is_finished());
    rig.probes.release.add_permits(1);
    assert_eq!(
        tokio::time::timeout(DEADLINE, rx).await.unwrap().unwrap(),
        "partial"
    );
    task.await.unwrap();
    rig.finish().await;
}
#[cfg(feature = "telegram")]
mod c2b_telegram_wire {
    use super::*;
    use crate::adapters::channel::telegram::TelegramChannelAdapter;
    use crate::domain::ports::ChannelPort;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const ALLOWED: i64 = 101;
    const DENIED: i64 = 202;
    const EXPECTED: &str = "Checking.\n[tool use: read_file (id: t1)]\nAnswer: 42";

    // A second getUpdates proves the first update was fully processed, including
    // the awaited sendMessage for an allowed chat. No time-based negative check.
    async fn case(inbound_chat: i64, expected_provider_calls: usize) {
        let api = MockServer::start().await;
        let polls = Arc::new(AtomicUsize::new(0));
        let processed = Arc::new(Semaphore::new(0));
        let responder_polls = polls.clone();
        let responder_processed = processed.clone();
        Mock::given(method("POST"))
            .and(path("/bottesttoken/GetUpdates"))
            .respond_with(move |_: &Request| {
                let count = responder_polls.fetch_add(1, Ordering::SeqCst);
                let result = if count == 0 {
                    serde_json::json!([{
                        "update_id": 1,
                        "message": {
                            "message_id": 10,
                            "date": 1,
                            "chat": {"id": inbound_chat, "type": "private", "first_name": "Fixture"},
                            "text": "What is the answer?"
                        }
                    }])
                } else {
                    responder_processed.add_permits(1);
                    serde_json::json!([])
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "result": result}))
            })
            .mount(&api).await;
        Mock::given(method("POST"))
            .and(path("/bottesttoken/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {"message_id": 11, "date": 1,
                    "chat": {"id": ALLOWED, "type": "private", "first_name": "Fixture"},
                    "text": EXPECTED}
            })))
            .mount(&api)
            .await;

        let workspace = tempfile::Builder::new()
            .prefix("c-2b-telegram-")
            .tempdir_in("/tmp")
            .unwrap();
        let ws = workspace.path().to_path_buf();
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(&ws),
            ws.clone(),
        ));
        let security = crate::adapters::security_adapter::SecurityAdapter::new(ws.clone());
        crate::domain::ports::SecurityPort::set_mode(
            &security,
            crate::domain::models::PermissionMode::Yolo,
        );
        let security: Arc<dyn crate::domain::ports::SecurityPort> = Arc::new(security);
        let factory_security = security.clone();
        let provider = Arc::new(C2bReplyProvider {
            calls: AtomicUsize::new(0),
            disconnect: false,
        });
        let factory_provider = provider.clone();
        let factory_storage = storage.clone();
        let factory_ws = ws.clone();
        let core = Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            security,
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                let mut runtime = Arc::try_unwrap(mock_runtime(
                    factory_provider.clone(),
                    factory_storage.clone(),
                    &factory_ws,
                ))
                .ok()
                .expect("new unshared runtime");
                let tools: Arc<dyn ToolSetPort> = Arc::new(C2bReadFile);
                runtime.security = factory_security.clone();
                runtime.tool_scheduler =
                    crate::domain::services::tool_scheduler::ToolScheduler::new(
                        runtime.security.clone(),
                        tools.clone(),
                        runtime.approval.clone(),
                        64,
                    );
                runtime.tools = tools;
                Ok(Arc::new(runtime))
            }),
        ));
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "c2b-telegram-wire".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let (channel_tx, channel_rx) = mpsc::unbounded_channel();
        let listener = UnixListener::bind(ws.join("c2b-telegram.sock")).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let run = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                server
                    .run(listener, domain_rx, Some(channel_rx), None, shutdown)
                    .await
            }
        });
        let telegram = TelegramChannelAdapter::new_with_api_url_for_test(
            "testtoken".into(),
            vec![ALLOWED],
            channel_tx,
            api.uri().parse().unwrap(),
        )
        .unwrap();
        telegram.start_loop().await.unwrap();
        if tokio::time::timeout(DEADLINE, processed.acquire())
            .await
            .is_err()
        {
            panic!(
                "adapter did not finish processing; requests: {:?}",
                api.received_requests().await
            );
        }
        telegram.shutdown_loop().await.unwrap();
        shutdown.cancel();
        tokio::time::timeout(DEADLINE, run)
            .await
            .expect("daemon shutdown timed out")
            .unwrap();

        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            expected_provider_calls
        );
        let requests = api.received_requests().await.unwrap();
        let sends: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/bottesttoken/SendMessage")
            .collect();
        if inbound_chat == ALLOWED {
            assert_eq!(sends.len(), 1, "exactly one whole-turn message");
            let body: serde_json::Value = serde_json::from_slice(&sends[0].body).unwrap();
            assert_eq!(body["chat_id"], ALLOWED);
            assert_eq!(body["text"], EXPECTED);
            assert!(!body["text"].as_str().unwrap().contains("SECRET-RESULT-C2B"));
            assert!(!body["text"].as_str().unwrap().contains("[tool result"));
        } else {
            assert!(sends.is_empty(), "denied chat must not receive a message");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn allowed_chat_receives_one_whole_tool_turn() {
        case(ALLOWED, 2).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn denied_chat_never_reaches_provider_or_receives_a_message() {
        case(DENIED, 0).await;
    }
}

struct C2bFenceProvider {
    first: C2bReplyProvider,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl StreamingProvider for C2bFenceProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 | 1 => self.first.stream_completion(messages, options).await,
            2 => Ok(futures::stream::iter(vec![
                StreamChunk::Text {
                    content: "second reply".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .boxed()),
            n => panic!("unexpected provider call {n}"),
        }
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "c2b-fence".into()
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

struct C2bFenceLedger {
    appends: AtomicUsize,
    final_append: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl crate::domain::ports::UsageLedgerPort for C2bFenceLedger {
    async fn append(
        &self,
        _: crate::domain::models::usage::UsageLedgerEntry,
    ) -> Result<(), crate::domain::errors::StorageError> {
        if self.appends.fetch_add(1, Ordering::SeqCst) == 1 {
            self.final_append.add_permits(1);
        }
        Ok(())
    }
    async fn read_session(
        &self,
        _: &str,
    ) -> Result<
        Vec<crate::domain::models::usage::UsageLedgerEntry>,
        crate::domain::errors::StorageError,
    > {
        Ok(vec![])
    }
    async fn read_since(
        &self,
        _: i64,
        _: crate::domain::ports::LedgerReadMode,
    ) -> Result<
        Vec<crate::domain::models::usage::UsageLedgerEntry>,
        crate::domain::errors::StorageError,
    > {
        Ok(vec![])
    }
}

struct C2bParkFinalSave {
    inner: Arc<dyn StoragePort>,
    entered: tokio::sync::Semaphore,
    parked: std::sync::atomic::AtomicBool,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl StoragePort for C2bParkFinalSave {
    async fn read_snapshot(
        &self,
        id: &str,
        checkpoint: crate::domain::models::checkpoint::CheckpointId,
        path: &std::path::Path,
    ) -> Result<Option<Vec<u8>>, crate::domain::errors::StorageError> {
        self.inner.read_snapshot(id, checkpoint, path).await
    }
    async fn save_conversation(
        &self,
        conversation: &Conversation,
    ) -> Result<(), crate::domain::errors::StorageError> {
        if conversation
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content == "Answer: 42")
            && !self.parked.swap(true, Ordering::SeqCst)
        {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
        self.inner.save_conversation(conversation).await
    }
    async fn load_conversation(
        &self,
        id: &str,
    ) -> Result<Option<Conversation>, crate::domain::errors::StorageError> {
        self.inner.load_conversation(id).await
    }
    async fn list_conversations(
        &self,
    ) -> Result<Vec<crate::domain::models::ConversationSummary>, crate::domain::errors::StorageError>
    {
        self.inner.list_conversations().await
    }
}

struct C2bFenceRig {
    server: Arc<AttachServer>,
    provider: Arc<C2bFenceProvider>,
    storage: Arc<C2bParkFinalSave>,
    channel: mpsc::UnboundedSender<ChannelTurnRequest>,
    shutdown: CancellationToken,
    run: tokio::task::JoinHandle<()>,
    _bus: EventBus,
    _workspace: tempfile::TempDir,
}

impl C2bFenceRig {
    async fn start() -> Self {
        let workspace = tempfile::Builder::new()
            .prefix("c-2b-fence-")
            .tempdir_in("/tmp")
            .unwrap();
        let ws = workspace.path().to_path_buf();
        let inner: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(&ws),
            ws.clone(),
        ));
        let storage = Arc::new(C2bParkFinalSave {
            inner,
            parked: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let ledger = Arc::new(C2bFenceLedger {
            appends: AtomicUsize::new(0),
            final_append: tokio::sync::Semaphore::new(0),
        });
        let provider = Arc::new(C2bFenceProvider {
            first: C2bReplyProvider {
                calls: AtomicUsize::new(0),
                disconnect: false,
            },
            calls: AtomicUsize::new(0),
        });
        let security = crate::adapters::security_adapter::SecurityAdapter::new(ws.clone());
        crate::domain::ports::SecurityPort::set_mode(
            &security,
            crate::domain::models::PermissionMode::Yolo,
        );
        let security: Arc<dyn crate::domain::ports::SecurityPort> = Arc::new(security);
        let factory_security = security.clone();
        let factory_provider = provider.clone();
        let factory_storage = storage.clone();
        let factory_ledger = ledger.clone();
        let factory_ws = ws.clone();
        let core = Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            security,
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                let mut runtime = Arc::try_unwrap(mock_runtime(
                    factory_provider.clone(),
                    factory_storage.clone(),
                    &factory_ws,
                ))
                .ok()
                .expect("new unshared runtime");
                let tools: Arc<dyn ToolSetPort> = Arc::new(C2bReadFile);
                runtime.security = factory_security.clone();
                runtime.tool_scheduler =
                    crate::domain::services::tool_scheduler::ToolScheduler::new(
                        runtime.security.clone(),
                        tools.clone(),
                        runtime.approval.clone(),
                        64,
                    );
                runtime.tools = tools;
                runtime.usage_ledger = factory_ledger.clone();
                Ok(Arc::new(runtime))
            }),
        ));
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "c2b-fence".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let (channel, channel_rx) = mpsc::unbounded_channel();
        let listener = UnixListener::bind(ws.join("fence.sock")).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let run = {
            let server = server.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                server
                    .run(listener, domain_rx, Some(channel_rx), None, shutdown)
                    .await
            })
        };
        Self {
            server,
            provider,
            storage,
            channel,
            shutdown,
            run,
            _bus: bus,
            _workspace: workspace,
        }
    }

    fn request(&self, text: &str) -> tokio::sync::oneshot::Receiver<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.channel
            .send(ChannelTurnRequest {
                text: text.into(),
                origin: ChannelKind::Telegram,
                response_tx,
            })
            .unwrap();
        response_rx
    }

    async fn final_commit_parked(&self) {
        self.storage.entered.acquire().await.unwrap().forget();
        self.server.fence_enqueued.acquire().await.unwrap().forget();
    }
}

#[tokio::test(start_paused = true)]
async fn c2b_fence_waits_without_timeout_and_serializes_channel_turns() {
    let rig = C2bFenceRig::start().await;
    let mut first = rig.request("first");
    rig.final_commit_parked().await;
    let mut second = rig.request("second");
    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        first.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        second.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(
        rig.provider.calls.load(Ordering::SeqCst),
        2,
        "queued turn started before the fence"
    );

    rig.storage.release.add_permits(1);
    assert_eq!(
        first.await.unwrap(),
        "Checking.\n[tool use: read_file (id: t1)]\nAnswer: 42"
    );
    assert_eq!(second.await.unwrap(), "second reply");
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 3);
    rig.shutdown.cancel();
    rig.run.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn c2b_fence_shutdown_resolves_pending_channel_with_fallback() {
    let rig = C2bFenceRig::start().await;
    let first = rig.request("first");
    rig.final_commit_parked().await;
    rig.shutdown.cancel();
    rig.run.await.unwrap();
    assert_eq!(first.await.unwrap(), CHANNEL_TURN_FAILED_REPLY);
}

#[tokio::test]
async fn c2b_fence_lifecycle_abort_releases_conversation_and_resolves_reply() {
    use crate::adapters::daemon::lifecycle::{DaemonRuntime, graceful_shutdown};
    use crate::adapters::noop::{NoOpChannel, NoOpScheduler, NoopRecallProvider};

    let rig = C2bFenceRig::start().await;
    let mut first = rig.request("first");
    tokio::time::timeout(DEADLINE, rig.final_commit_parked())
        .await
        .expect("final save and fence did not park");
    assert!(rig.server.conversation.try_lock().is_err());
    assert!(rig.server.turn_serial.try_lock().is_err());
    assert!(matches!(
        first.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let workspace = rig._workspace.path().to_path_buf();
    let rt = DaemonRuntime {
        config: AppConfig::default(),
        memory: Arc::new(NoOpMemory),
        recall: Arc::new(NoopRecallProvider),
        pid_path: workspace.join("daemon.pid"),
        socket_path: workspace.join("fence.sock"),
        workspace,
        server: rig.server.clone(),
        channel: Arc::new(NoOpChannel),
        channel_adapter: "terminal".into(),
        scheduler: Arc::new(NoOpScheduler),
        domain_rx: None,
        channel_turn_rx: None,
        #[cfg(feature = "cron")]
        cron_completion_rx: None,
        conversation: rig.server.conversation.clone(),
        core: rig.server.core.clone(),
    };

    // Run the production cancel/abort/join and shutdown snapshot without
    // releasing the save. The snapshot must not wait on a detached forwarder.
    tokio::time::timeout(DEADLINE, graceful_shutdown(&rt, rig.shutdown, rig.run))
        .await
        .expect("lifecycle shutdown blocked on the parked save");
    assert!(rt.conversation.try_lock().is_ok());
    assert!(!rt.socket_path.exists());
    assert_eq!(first.try_recv().unwrap(), CHANNEL_TURN_FAILED_REPLY);
    let _turn_guard = tokio::time::timeout(DEADLINE, rig.server.turn_serial.lock())
        .await
        .expect("closing the fence did not release the serialized turn");
}

#[tokio::test]
async fn channel_empty_failed_and_incomplete_turns_are_distinct() {
    for (step, expected) in [
        (Step::Empty, "(The agent finished without a text reply.)"),
        (Step::Private, "visible"),
        (Step::Fail, CHANNEL_TURN_FAILED_REPLY),
        (
            Step::Disconnect,
            "partial\n\n(The agent stopped before finishing this reply.)",
        ),
    ] {
        let rig = Rig::start(vec![step]).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        rig.channel_tx
            .send(ChannelTurnRequest {
                text: "request".into(),
                origin: ChannelKind::Telegram,
                response_tx: tx,
            })
            .unwrap();
        assert_eq!(
            tokio::time::timeout(DEADLINE, rx).await.unwrap().unwrap(),
            expected
        );
        rig.finish().await;
    }
}

mod slack_wire_tests;
