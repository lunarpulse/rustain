//! K02 (daemon-hosted persistence and replay, also after restart) and the
//! daemon side of K07 (frames, transcript snapshots, history, channel response).
use super::common::*;
use arc_swap::ArcSwap;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ChannelKind, ChannelTurnRequest, ContentBlockType, Conversation, StreamChunk,
};
use rustain::domain::ports::{SecurityPort, StoragePort, StreamingProvider, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::runtime::event_bus::RawEventKind;
use serial_test::serial;
use std::path::Path;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

const MARK: &str = "k02";
const CONVERSATION: &str = "k02-daemon";

fn runtime(
    provider: Arc<dyn StreamingProvider>,
    storage: Arc<dyn StoragePort>,
    ws: &Path,
) -> Arc<DaemonTurnRuntime> {
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval.clone(),
        64,
    );
    Arc::new(DaemonTurnRuntime {
        recipient_context: Arc::new(Default::default()),
        provider,
        app_config: Arc::new(ArcSwap::from_pointee(app_config())),
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
            rustain::infrastructure::paths::sessions_dir(ws),
            ws.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(
            rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
        ),
        approval,
        workspace: ws.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    })
}

struct Daemon {
    socket: std::path::PathBuf,
    channel: tokio::sync::mpsc::UnboundedSender<ChannelTurnRequest>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// The production server: socket attach, forwarder, commit and fan-out.
    fn start(ws: &Path, conversation: Conversation, socket: &str) -> Self {
        let storage: Arc<dyn StoragePort> = storage(ws);
        let provider = codex();
        let core = {
            let (ws, storage) = (ws.to_path_buf(), storage.clone());
            Arc::new(DaemonCore::new(
                ws.clone(),
                Arc::new(ArcSwap::from_pointee(app_config())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || Ok(runtime(provider.clone(), storage.clone(), &ws))),
            ))
        };
        let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let server = AttachServer::new(core, Arc::new(Mutex::new(conversation)), domain_tx);
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

    /// Shutdown is requested, then the server task is torn down; every
    /// persistence the test reads has already completed by then.
    async fn stop(self) {
        self.shutdown.cancel();
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn frame(stream: &mut UnixStream) -> DaemonFrame {
    tokio::time::timeout(GUARD, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("daemon frame hang guard")
        .expect("decode daemon frame")
        .expect("daemon frame present")
}

/// A signed trusted-local attachment; returns the stream and its acknowledgement.
async fn attach(socket: &Path) -> (UnixStream, DaemonFrame) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let DaemonFrame::AttachChallenge { nonce } = frame(&mut stream).await else {
        panic!("challenge expected")
    };
    let keys = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .unwrap();
    let tier = ConnectionTier::TrustedLocal;
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), false);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: false,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .unwrap();
    let ack = frame(&mut stream).await;
    assert!(matches!(ack, DaemonFrame::AttachAck { .. }), "{ack:?}");
    (stream, ack)
}

/// Submit one operator turn; every frame up to its completion is returned.
async fn turn(stream: &mut UnixStream, text: &str) -> Vec<DaemonFrame> {
    write_frame(
        stream,
        &ClientFrame::UserMessage {
            text: text.into(),
            images: vec![],
        },
    )
    .await
    .unwrap();
    let mut frames = Vec::new();
    loop {
        let next = frame(stream).await;
        let done = matches!(
            &next,
            DaemonFrame::Event(event) if matches!(&event.kind, RawEventKind::Provider(StreamChunk::TurnComplete { .. }))
        );
        frames.push(next);
        if done {
            return frames;
        }
    }
}

fn clean(frames: &[DaemonFrame]) {
    for frame in frames {
        let encoded = serde_json::to_string(frame).unwrap();
        assert!(!mentions(&encoded, MARK), "{encoded}");
        assert!(!encoded.contains("providerReplay"), "{encoded}");
    }
}

async fn stored_items(ws: &Path) -> usize {
    storage(ws)
        .load_conversation(CONVERSATION)
        .await
        .unwrap()
        .unwrap()
        .messages
        .iter()
        .flat_map(|m| m.content_blocks.iter())
        .filter(|b| matches!(b, ContentBlockType::ProviderReplay(item) if item.payload["encrypted_content"] == enc(MARK)))
        .count()
}

#[tokio::test]
#[serial]
async fn k02_daemon_persists_once_replays_after_restart_and_never_projects() {
    assert_eq!(PROTOCOL_VERSION, 15, "replay adds no frame shape");
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k02-account").await;
    mount_script(
        &server,
        vec![
            sse(&[reasoning(MARK), text("daemon answer"), completed()]),
            sse(&[text("second answer"), completed()]),
            sse(&[text("channel answer"), completed()]),
            sse(&[text("restarted answer"), completed()]),
        ],
    )
    .await;
    let ws = world.workspace();
    let daemon = Daemon::start(
        &ws,
        Conversation {
            id: CONVERSATION.into(),
            ..Default::default()
        },
        "first.sock",
    );
    let (mut client, ack) = attach(&daemon.socket).await;
    clean(&[ack]);
    let frames = turn(&mut client, "first").await;
    clean(&frames);
    assert!(
        frames.iter().any(|f| matches!(f, DaemonFrame::Event(e) if matches!(&e.kind, RawEventKind::Provider(StreamChunk::Text { .. })))),
        "the visible answer is still observed"
    );
    assert_eq!(
        stored_items(&ws).await,
        1,
        "the daemon persists the item exactly once"
    );

    // A newly attached client's snapshot and history never carry it.
    let (mut observer, snapshot) = attach(&daemon.socket).await;
    let DaemonFrame::AttachAck {
        snapshot: ref state,
        ..
    } = snapshot
    else {
        unreachable!()
    };
    assert!(
        state
            .transcript
            .iter()
            .any(|m| m.content == "daemon answer")
    );
    clean(std::slice::from_ref(&snapshot));
    write_frame(
        &mut observer,
        &ClientFrame::HistoryRequest {
            before_index: None,
            count: 50,
        },
    )
    .await
    .unwrap();
    loop {
        let next = frame(&mut observer).await;
        let history = matches!(next, DaemonFrame::History { .. });
        clean(&[next]);
        if history {
            break;
        }
    }

    // The next daemon turn replays it before that turn's output.
    clean(&turn(&mut client, "second").await);
    let bodies = posts(&server).await;
    assert_replays(&bodies[1], MARK);
    let item = position(&bodies[1], |i| i["type"] == "reasoning").unwrap();
    let output = position(&bodies[1], |i| i["role"] == "assistant").unwrap();
    assert!(item < output);

    // The channel response is the plain text.
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    daemon
        .channel
        .send(ChannelTurnRequest {
            text: "via channel".into(),
            origin: ChannelKind::Telegram,
            response_tx,
        })
        .unwrap();
    let response = tokio::time::timeout(GUARD, response_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, "channel answer");
    assert!(!mentions(&response, MARK));
    assert_eq!(stored_items(&ws).await, 1);
    drop((client, observer));
    daemon.stop().await;

    // After a daemon restart the persisted item is still replayed.
    let restored = storage(&ws)
        .load_conversation(CONVERSATION)
        .await
        .unwrap()
        .unwrap();
    let daemon = Daemon::start(&ws, restored, "second.sock");
    let (mut client, ack) = attach(&daemon.socket).await;
    clean(&[ack]);
    clean(&turn(&mut client, "after restart").await);
    let bodies = posts(&server).await;
    assert_eq!(bodies.len(), 4);
    assert_replays(&bodies[3], MARK);
    drop(client);
    daemon.stop().await;
}
