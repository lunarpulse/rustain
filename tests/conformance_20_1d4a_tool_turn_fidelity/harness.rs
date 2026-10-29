//! Shared front doors: the fixture tool set (an MCP-projected image result that
//! finishes after its sibling), the in-process `ask`, the TUI composition
//! (`LocalTurnDriver` → `run_turn` → the event loop's consumer → session store)
//! and the real daemon server, each over any provider, plus the byte-prefix
//! oracle.
#![allow(dead_code)]

use crate::conformance_20_1d1b_codex_replay_discovery::common::{GUARD, storage};
use crate::conformance_20_1d4_gemini_cli::replay::fold;
use arc_swap::ArcSwap;
use rmcp::model::{CallToolResult, Content};
use rustain::adapters::cli::ask::AskOpts;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::mcp::tool_projection::project_rmcp_result;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpContext, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpUsageLedger,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ToolError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AppConfig, ChannelTurnRequest, Conversation, PermissionMode, SessionManager, SessionState,
    StopReason, StreamChunk, StreamingState, ToolDefinition, ToolResult,
};
use rustain::domain::ports::{
    ContextAssemblerPort, ContextPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
    UsageLedgerPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::CliCore;
use rustain::infrastructure::runtime::event_bus::RawEventKind;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::telemetry::ActiveRatioWindow;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

/// Base64 of `IMAGE-BYTES-MARKER`: the image bytes every outward path must lack.
pub(crate) const IMAGE: &str = "SU1BR0UtQllURVMtTUFSS0VS";
pub(crate) const IMAGE_TOOL: &str = "fixture_image";
pub(crate) const ECHO_TOOL: &str = "fixture_echo";

// ── the fixture tool set ────────────────────────────────────────────────────

/// Attempts answered in order; the last one repeats.
pub(crate) struct Seq(
    Vec<wiremock::ResponseTemplate>,
    std::sync::atomic::AtomicUsize,
);
impl wiremock::Respond for Seq {
    fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
        let index = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0[index.min(self.0.len() - 1)].clone()
    }
}

pub(crate) async fn mount(
    server: &wiremock::MockServer,
    route: &str,
    attempts: Vec<wiremock::ResponseTemplate>,
) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(route))
        .respond_with(Seq(attempts, Default::default()))
        .mount(server)
        .await;
}

/// Raw bodies of every POST to `route`, in arrival order.
pub(crate) async fn bodies(server: &wiremock::MockServer, route: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == route)
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect()
}

/// `fixture_image` returns an `rmcp` text + image result through the real
/// `project_rmcp_result`, and finishes only after `fixture_echo` has: the two
/// calls of one response finish in reverse order. `lookup` serves the Codex
/// fixture's calls. `images: false` is the pre-story projection (placeholder
/// text only), the baseline of every non-Gemini request.
pub(crate) struct Tools {
    echoed: tokio::sync::Notify,
    images: bool,
}

impl Tools {
    pub(crate) fn port(images: bool) -> Arc<dyn ToolSetPort> {
        Arc::new(Self {
            echoed: tokio::sync::Notify::new(),
            images,
        })
    }
}

fn tool(name: &str, field: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("Fixture tool {name}"),
        input_schema: json!({
            "type": "object",
            "properties": {field: {"type": "string"}},
            "required": [field],
        }),
        parallel_safe: true,
    }
}

#[async_trait::async_trait]
impl ToolSetPort for Tools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![
            tool(IMAGE_TOOL, "label"),
            tool(ECHO_TOOL, "text"),
            tool("lookup", "q"),
        ]
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let arg = |key: &str| input[key].as_str().unwrap_or_default().to_owned();
        Ok(match tool_name {
            IMAGE_TOOL => {
                self.echoed.notified().await;
                let mut content = vec![Content::text(format!("image {}", arg("label")))];
                content.push(Content::image(IMAGE, "image/png"));
                let mut result =
                    project_rmcp_result(CallToolResult::success(content), String::new());
                if !self.images {
                    result.images.clear();
                }
                result
            }
            ECHO_TOOL => {
                self.echoed.notify_one();
                text_result(format!("echo {}", arg("text")))
            }
            _ => text_result("looked up".into()),
        })
    }
}

fn text_result(content: String) -> ToolResult {
    ToolResult {
        tool_use_id: String::new(),
        content,
        is_error: false,
        images: Vec::new(),
    }
}

/// `--yolo` security, as `build_cli_core` composes it.
pub(crate) fn yolo(ws: &Path) -> Arc<dyn SecurityPort> {
    let adapter = SecurityAdapter::new(ws.to_path_buf());
    SecurityPort::set_mode(&adapter, PermissionMode::Yolo);
    Arc::new(adapter)
}

// ── `ask` ───────────────────────────────────────────────────────────────────

/// The `--yolo` CLI core `ask` and ACP run on.
pub(crate) fn cli_core(
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<dyn ToolSetPort>,
    ws: &Path,
) -> CliCore {
    let security = yolo(ws);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage: storage(ws),
        event_tx,
        event_rx,
        ledger: Arc::new(NoOpUsageLedger),
    }
}

/// In-process `ask` over the real turn loop and session persistence.
pub(crate) async fn ask(
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<dyn ToolSetPort>,
    cfg: &AppConfig,
    ws: &Path,
    opts: AskOpts,
) -> (ExitCode, String, String) {
    let core = cli_core(provider.clone(), tools, ws);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = tokio::time::timeout(
        GUARD,
        rustain::adapters::cli::ask::run_ask_core(
            provider, core, opts, cfg, ws, &mut out, &mut err,
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("in-process ask did not finish within {GUARD:?}"));
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

// ── the TUI composition (K1) ────────────────────────────────────────────────

/// One TUI turn: `LocalTurnDriver::submit` builds the history from
/// `conversation` and launches `run_turn`; its provider chunks are folded by the
/// event loop's consumer into `conversation`. Returns the folded chunks.
pub(crate) async fn tui_turn(
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<dyn ToolSetPort>,
    cfg: &AppConfig,
    ws: &Path,
    conversation: &mut Conversation,
    text: &str,
) -> Vec<StreamChunk> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let security = yolo(ws);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
    let fs = storage(ws);
    let driver = LocalTurnDriver::new(
        provider,
        Arc::new(ArcSwap::from_pointee(cfg.clone())),
        event_tx,
        security,
        tools,
        scheduler,
        Arc::new(SkillActivator::new()),
        Arc::new(NoOpPersona),
        Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn ContextPort>
        )),
        Arc::new(ArcSwap::from_pointee(
            None as Option<Arc<dyn ContextAssemblerPort>>,
        )),
        ws.to_path_buf(),
        fs.clone(),
        fs as Arc<dyn StoragePort>,
        Arc::new(DefaultPlanInjector::new()),
        Arc::new(NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
        ActiveRatioWindow::new_in_memory(),
    );
    let mut streaming = StreamingState::default();
    let mut state = TuiState::new(80, 24);
    let mut lifecycle =
        rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
    let mut session_manager = SessionManager::new(SessionState::Empty);
    driver
        .submit(
            UserSubmission {
                text: text.into(),
                images: vec![],
                synthetic: false,
                activation_set: None,
                agent_snapshot: None,
                turn_cancel: CancellationToken::new(),
            },
            TurnViewState {
                conversation: &mut *conversation,
                streaming: &mut streaming,
                state: &mut state,
                lifecycle: &mut lifecycle,
                session_manager: &mut session_manager,
            },
        )
        .await;
    let handle = lifecycle
        .take_task(&conversation.id)
        .expect("LocalTurnDriver launches run_turn");
    tokio::time::timeout(GUARD, handle)
        .await
        .expect("TUI turn hang guard")
        .expect("turn task must not panic");
    drop(driver);
    let mut chunks = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(event) {
            chunks.push(chunk);
        }
    }
    fold(conversation, chunks.clone());
    chunks
}

// ── the daemon (K3, K5) ─────────────────────────────────────────────────────

fn runtime(
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<dyn ToolSetPort>,
    cfg: AppConfig,
    storage: Arc<dyn StoragePort>,
    ws: &Path,
) -> Arc<DaemonTurnRuntime> {
    let security = yolo(ws);
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
    Arc::new(DaemonTurnRuntime {
        recipient_context: Arc::new(Default::default()),
        provider,
        app_config: Arc::new(ArcSwap::from_pointee(cfg)),
        security,
        tools,
        tool_scheduler,
        persona: Arc::new(NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(None)),
        context: Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn ContextPort>
        )),
        storage,
        fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(ws),
            ws.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(DefaultPlanInjector::new()),
        approval,
        workspace: ws.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    })
}

pub(crate) struct Daemon {
    pub(crate) socket: PathBuf,
    pub(crate) channel: tokio::sync::mpsc::UnboundedSender<ChannelTurnRequest>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// The production server (socket attach, forwarder, commit, fan-out) over
    /// `conversation` on the workspace session store.
    pub(crate) fn start(
        provider: Arc<dyn StreamingProvider>,
        cfg: &AppConfig,
        ws: &Path,
        conversation: Conversation,
        socket: &str,
    ) -> Self {
        let storage: Arc<dyn StoragePort> = storage(ws);
        let core = {
            let (ws, storage, cfg) = (ws.to_path_buf(), storage.clone(), cfg.clone());
            let tools = Tools::port(true);
            Arc::new(DaemonCore::new(
                ws.clone(),
                Arc::new(ArcSwap::from_pointee(cfg.clone())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    Ok(runtime(
                        provider.clone(),
                        tools.clone(),
                        cfg.clone(),
                        storage.clone(),
                        &ws,
                    ))
                }),
            ))
        };
        let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let server = AttachServer::new(
            core,
            Arc::new(tokio::sync::Mutex::new(conversation)),
            domain_tx,
        );
        let socket = ws.join(socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (channel, channel_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.run(
            listener,
            domain_rx,
            Some(channel_rx),
            None,
            shutdown.clone(),
        ));
        Self {
            socket,
            channel,
            shutdown,
            task,
        }
    }

    /// A restarted daemon on the same store restores what the last one saved.
    pub(crate) async fn restart(
        self,
        provider: Arc<dyn StreamingProvider>,
        cfg: &AppConfig,
        ws: &Path,
        id: &str,
        socket: &str,
    ) -> Self {
        self.stop().await;
        let restored = storage(ws)
            .load_conversation(id)
            .await
            .unwrap()
            .expect("the daemon persisted the conversation");
        Self::start(provider, cfg, ws, restored, socket)
    }

    pub(crate) async fn stop(self) {
        self.shutdown.cancel();
        self.task.abort();
        let _ = self.task.await;
    }

    /// One channel turn; returns the channel reply.
    pub(crate) async fn channel_turn(&self, text: &str) -> String {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.channel
            .send(ChannelTurnRequest {
                text: text.into(),
                origin: rustain::domain::models::ChannelKind::Telegram,
                response_tx,
            })
            .unwrap();
        tokio::time::timeout(GUARD, response_rx)
            .await
            .expect("channel reply hang guard")
            .expect("channel reply")
    }
}

pub(crate) async fn daemon_frame(stream: &mut UnixStream) -> DaemonFrame {
    tokio::time::timeout(GUARD, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("daemon frame hang guard")
        .expect("decode daemon frame")
        .expect("daemon frame present")
}

/// Attach at `tier` (a writer unless `read_only`); returns the stream and its
/// `AttachAck`.
pub(crate) async fn attach(
    socket: &Path,
    tier: ConnectionTier,
    read_only: bool,
) -> (UnixStream, DaemonFrame) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let DaemonFrame::AttachChallenge { nonce } = daemon_frame(&mut stream).await else {
        panic!("challenge expected")
    };
    let keys = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .unwrap();
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: read_only,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .unwrap();
    let ack = daemon_frame(&mut stream).await;
    assert!(matches!(ack, DaemonFrame::AttachAck { .. }), "{ack:?}");
    (stream, ack)
}

/// Frames up to and including the operator turn's final `EndTurn` event.
pub(crate) async fn until_end_turn(stream: &mut UnixStream) -> Vec<DaemonFrame> {
    let mut frames = Vec::new();
    loop {
        let next = daemon_frame(stream).await;
        let done = matches!(
            &next,
            DaemonFrame::Event(event) if matches!(
                &event.kind,
                RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn })
            )
        );
        frames.push(next);
        if done {
            return frames;
        }
    }
}

/// One operator turn from an attached writer; every frame up to its `EndTurn`.
pub(crate) async fn daemon_turn(stream: &mut UnixStream, text: &str) -> Vec<DaemonFrame> {
    write_frame(
        stream,
        &ClientFrame::UserMessage {
            text: text.into(),
            images: vec![],
        },
    )
    .await
    .unwrap();
    until_end_turn(stream).await
}

/// A `HistoryRequest` page and every frame before it.
pub(crate) async fn history(stream: &mut UnixStream) -> Vec<DaemonFrame> {
    write_frame(
        stream,
        &ClientFrame::HistoryRequest {
            before_index: None,
            count: 50,
        },
    )
    .await
    .unwrap();
    let mut frames = Vec::new();
    loop {
        let next = daemon_frame(stream).await;
        let done = matches!(next, DaemonFrame::History { .. });
        frames.push(next);
        if done {
            return frames;
        }
    }
}

// ── the oracle ──────────────────────────────────────────────────────────────

/// The raw bytes of the JSON array that follows the first `"key":` in `raw`.
pub(crate) fn raw_array<'a>(raw: &'a str, key: &str) -> &'a str {
    let marker = format!("\"{key}\":[");
    let start = raw
        .find(&marker)
        .unwrap_or_else(|| panic!("no {key}: {raw}"))
        + marker.len()
        - 1;
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (offset, byte) in raw.as_bytes()[start..].iter().enumerate() {
        match (in_string, escaped, byte) {
            (true, true, _) => escaped = false,
            (true, false, b'\\') => escaped = true,
            (true, false, b'"') => in_string = false,
            (true, false, _) => {}
            (false, _, b'"') => in_string = true,
            (false, _, b'[' | b'{') => depth += 1,
            (false, _, b']' | b'}') => {
                depth -= 1;
                if depth == 0 {
                    return &raw[start..=start + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated {key}: {raw}")
}

/// The oracle: the history array of `after` starts with the history array of
/// `before` byte for byte. Returns the elements that follow it.
pub(crate) fn assert_prefix(before: &str, after: &str, key: &str) -> Vec<Value> {
    let (old, new) = (raw_array(before, key), raw_array(after, key));
    let head = &old[..old.len() - 1];
    if !(new.starts_with(head) && new.as_bytes().get(head.len()) == Some(&b',')) {
        let old_items: Vec<Value> = serde_json::from_str(old).unwrap();
        let new_items: Vec<Value> = serde_json::from_str(new).unwrap();
        let at = old_items
            .iter()
            .zip(&new_items)
            .position(|(a, b)| a != b)
            .unwrap_or(old_items.len().min(new_items.len()));
        panic!(
            "{key}[{at}] differs after the reload\n in-turn:  {}\n reloaded: {}\n\nin-turn {key}: {old}\n\nreloaded {key}: {new}",
            old_items.get(at).map(Value::to_string).unwrap_or_default(),
            new_items.get(at).map(Value::to_string).unwrap_or_default(),
        );
    }
    let new_items: Vec<Value> = serde_json::from_str(new).unwrap();
    let old_len = serde_json::from_str::<Vec<Value>>(old).unwrap().len();
    new_items[old_len..].to_vec()
}
