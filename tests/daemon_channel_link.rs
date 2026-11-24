//! Story C-12 AC4 (FR68, NFR60) — the daemon channel adapter's redacted link state through a
//! real daemon attach: production `AttachServer::run` on a Unix socket, signed attach
//! handshakes at the TrustedLocal and Peer tiers, and the production publisher
//! `AttachServer::spawn_channel_link_publisher` (the one `run_lifecycle` spawns) over a typed
//! fixture `ChannelPort` whose link watch the test drives. Absence is proven against a positive
//! witness event both tiers receive, never by sleeping. Timeouts are hang guards only.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustain::adapters::daemon::protocol::{
    AttachMode, ConnectionTier, DaemonFrame, answer_attach_challenge, read_frame,
};
use rustain::adapters::daemon::runtime::DaemonCore;
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpChannel, NoOpMemory, NoOpPersona, NoOpSecurity};
use rustain::adapters::rap::AgentSigner;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{AppConfig, ChannelLinkState, Conversation, NoticeLevel};
use rustain::domain::ports::{ChannelPort, StoragePort};
use rustain::infrastructure::paths::sessions_dir;
use rustain::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(20);
const WITNESS: &str = "C12-LINK-WITNESS";

/// A channel adapter that publishes link state (the Slack shape): its watch is the fixture.
struct LinkChannel(watch::Receiver<ChannelLinkState>);

#[async_trait::async_trait]
impl ChannelPort for LinkChannel {
    fn health_updates(&self) -> Option<watch::Receiver<ChannelLinkState>> {
        Some(self.0.clone())
    }
}

struct Daemon {
    server: Arc<AttachServer>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    path: PathBuf,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
    _workspace: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let workspace = tempfile::tempdir().expect("daemon workspace");
        let ws = workspace.path().to_path_buf();
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            sessions_dir(&ws),
            ws.clone(),
        ));
        let core = Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
            Box::new(|| panic!("link-state attach never builds a turn runtime")),
        ));
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(
            core,
            Arc::new(tokio::sync::Mutex::new(Conversation::default())),
            domain_tx.clone(),
        );
        let path = ws.join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind attach socket");
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.clone().run(
            listener,
            domain_rx,
            None,
            None,
            shutdown.child_token(),
        ));
        Self {
            server,
            domain_tx,
            path,
            shutdown,
            task,
            _workspace: workspace,
        }
    }

    /// A bus event every attachment of both tiers receives, after everything sent before it.
    fn witness(&self) {
        self.domain_tx
            .send(AppEvent::SystemNotice {
                conversation_id: None,
                level: NoticeLevel::Info,
                message: WITNESS.to_owned(),
            })
            .expect("forwarder running");
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .expect("attach server stops")
            .expect("attach server task");
    }
}

async fn next_frame(stream: &mut UnixStream) -> DaemonFrame {
    tokio::time::timeout(DEADLINE, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("frame deadline")
        .expect("decode daemon frame")
        .expect("connection open")
}

/// Signed attach; returns the stream positioned right after the AttachAck.
async fn attach(path: &Path, tier: ConnectionTier, read_only_ok: bool, seed: u8) -> UnixStream {
    let mut stream = tokio::time::timeout(DEADLINE, UnixStream::connect(path))
        .await
        .expect("connect deadline")
        .expect("connect daemon");
    let signer = AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]));
    {
        let (mut read, mut write) = stream.split();
        answer_attach_challenge(&mut read, &mut write, read_only_ok, tier, &signer)
            .await
            .expect("signed attach");
    }
    match next_frame(&mut stream).await {
        DaemonFrame::AttachAck { granted_mode, .. } => {
            let expected = if tier == ConnectionTier::TrustedLocal && !read_only_ok {
                AttachMode::ReadWrite
            } else {
                AttachMode::ReadOnly
            };
            assert_eq!(granted_mode, expected);
        }
        other => panic!("AttachAck expected, got {other:?}"),
    }
    stream
}

fn link_state(frame: &DaemonFrame) -> Option<ChannelLinkState> {
    match frame {
        DaemonFrame::Event(RawEvent {
            kind: RawEventKind::ChannelLinkChanged { adapter, state },
            ..
        }) => {
            assert_eq!(adapter, "slack", "the catalog name, nothing else");
            Some(*state)
        }
        _ => None,
    }
}

async fn next_link(stream: &mut UnixStream) -> ChannelLinkState {
    loop {
        if let Some(state) = link_state(&next_frame(stream).await) {
            return state;
        }
    }
}

/// Every link state received before the witness.
async fn links_until_witness(stream: &mut UnixStream) -> Vec<ChannelLinkState> {
    let mut links = Vec::new();
    loop {
        let frame = next_frame(stream).await;
        if let DaemonFrame::Event(RawEvent {
            kind: RawEventKind::SystemNotice { message, .. },
            ..
        }) = &frame
            && message == WITNESS
        {
            return links;
        }
        links.extend(link_state(&frame));
    }
}

#[tokio::test]
async fn trusted_local_attach_gets_the_late_snapshot_then_live_updates_and_peers_get_neither() {
    let daemon = Daemon::start().await;
    let (link_tx, link_rx) = watch::channel(ChannelLinkState::Connecting);
    let channel = LinkChannel(link_rx);
    let publisher_stop = CancellationToken::new();
    let publisher = daemon
        .server
        .spawn_channel_link_publisher("slack", &channel, publisher_stop.clone())
        .expect("a link-publishing adapter is published");

    // An observer attached before anything was published sees `connecting` arrive live: after
    // that the daemon has fanned it out and retained it.
    let mut observer = attach(&daemon.path, ConnectionTier::TrustedLocal, true, 41).await;
    assert_eq!(next_link(&mut observer).await, ChannelLinkState::Connecting);

    // Late attach: the writer's first frame after its AttachAck is the current state, which
    // can only be the snapshot (the live fanout already happened). Starting is not connected.
    let mut writer = attach(&daemon.path, ConnectionTier::TrustedLocal, false, 42).await;
    assert_eq!(
        link_state(&next_frame(&mut writer).await),
        Some(ChannelLinkState::Connecting),
        "the late snapshot is the first frame after the AttachAck"
    );
    // A Peer-tier attachment gets no snapshot and, below, no live update.
    let mut peer = attach(&daemon.path, ConnectionTier::Peer, true, 43).await;

    for state in [
        ChannelLinkState::Connected,
        ChannelLinkState::Reconnecting,
        ChannelLinkState::Offline,
    ] {
        link_tx.send(state).expect("publisher subscribed");
        assert_eq!(next_link(&mut writer).await, state, "writer live");
        assert_eq!(next_link(&mut observer).await, state, "observer live");
    }

    // A later trusted-local attach gets the latest state, not a stale one.
    let mut late = attach(&daemon.path, ConnectionTier::TrustedLocal, true, 44).await;
    assert_eq!(
        link_state(&next_frame(&mut late).await),
        Some(ChannelLinkState::Offline)
    );

    // Positive witness: both tiers receive it, so the peer stream is drained up to a point
    // after every link frame; it carried none. Each trusted stream had nothing extra either.
    daemon.witness();
    assert_eq!(links_until_witness(&mut peer).await, Vec::new(), "peer");
    assert_eq!(links_until_witness(&mut writer).await, Vec::new(), "writer");
    assert_eq!(links_until_witness(&mut late).await, Vec::new(), "late");
    assert_eq!(
        links_until_witness(&mut observer).await,
        Vec::new(),
        "observer"
    );

    // Shutdown cancels and joins the publisher; a later change publishes nothing.
    publisher_stop.cancel();
    tokio::time::timeout(DEADLINE, publisher)
        .await
        .expect("publisher joins on cancel")
        .expect("publisher task");
    let _ = link_tx.send(ChannelLinkState::Connected);
    daemon.witness();
    assert_eq!(links_until_witness(&mut observer).await, Vec::new());
    daemon.stop().await;
}

#[tokio::test]
async fn a_channel_without_link_state_publishes_nothing() {
    let daemon = Daemon::start().await;
    // The terminal channel (like Telegram) answers `health_updates` with `None`.
    assert!(
        daemon
            .server
            .spawn_channel_link_publisher("terminal", &NoOpChannel, CancellationToken::new())
            .is_none()
    );
    let mut local = attach(&daemon.path, ConnectionTier::TrustedLocal, false, 51).await;
    daemon.witness();
    assert_eq!(links_until_witness(&mut local).await, Vec::new());
    daemon.stop().await;
}

#[tokio::test]
async fn a_dropped_link_sender_ends_the_publisher_after_its_last_state() {
    let daemon = Daemon::start().await;
    let (link_tx, link_rx) = watch::channel(ChannelLinkState::Connecting);
    let channel = LinkChannel(link_rx);
    let mut local = attach(&daemon.path, ConnectionTier::TrustedLocal, true, 61).await;
    let publisher = daemon
        .server
        .spawn_channel_link_publisher("slack", &channel, CancellationToken::new())
        .expect("published");
    assert_eq!(next_link(&mut local).await, ChannelLinkState::Connecting);
    // The adapter stopped: its sender and every receiver clone go away.
    drop(link_tx);
    drop(channel);
    tokio::time::timeout(DEADLINE, publisher)
        .await
        .expect("publisher ends when the adapter drops its sender")
        .expect("publisher task");
    daemon.stop().await;
}
