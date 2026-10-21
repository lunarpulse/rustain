use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client as _};
use anyhow::Result;
use futures::{AsyncRead, AsyncWrite, future::LocalBoxFuture};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::adapters::cli::commands::AcpClientProfile;
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::events::AppEvent;
use crate::domain::models::{AgentId, AppConfig, NodeState};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::subagent::{NodeJournal, NodeTree};

use super::agent::{
    AcpCoreFactory, CoreFactory, PermissionAsk, RustainAcpAgent, SessionNotify, SharedSessions,
};

/// Deterministic checkpoints for the real session bridge and SDK pump.
/// Entirely absent from ordinary builds; no production timers or telemetry.
#[cfg(feature = "test-instrumentation")]
pub mod permission_test_probe {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use tokio::sync::{broadcast, mpsc, oneshot};

    type Fence = mpsc::UnboundedSender<oneshot::Sender<usize>>;
    thread_local! {
        static BRIDGE: broadcast::Sender<(String, bool)> = broadcast::channel(64).0;
        static PUMP: RefCell<Option<Fence>> = const { RefCell::new(None) };
    }

    pub fn subscribe_bridge() -> broadcast::Receiver<(String, bool)> {
        BRIDGE.with(|bridge| bridge.subscribe())
    }

    pub(crate) fn reviewed(session: &str, forwarded: bool) {
        BRIDGE.with(|bridge| {
            let _ = bridge.send((session.to_owned(), forwarded));
        });
    }

    pub async fn pump_fence() -> usize {
        let sender = PUMP.with(|pump| pump.borrow().as_ref().expect("SDK pump registered").clone());
        let (tx, rx) = oneshot::channel();
        sender.send(tx).expect("SDK pump alive");
        rx.await.expect("SDK pump fence response")
    }

    pub(super) async fn instrument(
        pump: impl std::future::Future<Output = ()>,
        completed: Rc<Cell<usize>>,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel::<oneshot::Sender<usize>>();
        PUMP.with(|pump| *pump.borrow_mut() = Some(tx));
        tokio::pin!(pump);
        loop {
            tokio::select! {
                biased;
                _ = &mut pump => break,
                Some(ack) = rx.recv() => {
                    // Poll the actual pump after receiver cancellation. This is
                    // a readiness fence, not a sleep or a scheduling heuristic.
                    if futures::poll!(&mut pump).is_ready() {
                        let _ = ack.send(completed.get());
                        break;
                    }
                    let _ = ack.send(completed.get());
                }
            }
        }
        PUMP.with(|pump| *pump.borrow_mut() = None);
    }
}

/// Production id source: a unique nanoid per session (DD-2). Survives a
/// process restart — a counter would reset and collide with persisted ids.
pub fn default_acp_id_source() -> Rc<dyn Fn() -> String> {
    Rc::new(crate::domain::models::conversation::generate_conversation_id)
}

/// Deterministic id source for the in-process test seam: a monotonic counter
/// rooted at 1 so the first session is always `acp-1` (golden determinism).
/// Mirrors the injected-`Clock` pattern — production uses [`default_acp_id_source`].
pub fn deterministic_acp_id_source() -> Rc<dyn Fn() -> String> {
    let counter = Rc::new(Cell::new(1u64));
    Rc::new(move || {
        let n = counter.get();
        counter.set(n + 1);
        n.to_string()
    })
}

/// Run rustain as an ACP agent over process stdio.
pub async fn run_acp(
    app_config: AppConfig,
    workspace: PathBuf,
    model_override: Option<String>,
    client_profile: AcpClientProfile,
) -> Result<()> {
    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();
    serve_acp_with_io(
        outgoing,
        incoming,
        app_config,
        workspace,
        model_override,
        client_profile,
    )
    .await
}

/// Serve ACP over caller-provided byte streams with production composition.
///
/// Tests use this seam with in-memory duplex streams; the CLI passes stdio. The
/// implementation still uses `AgentSideConnection::new`, so both paths exercise
/// the SDK JSON-RPC dispatcher and the same `!Send` LocalSet constraints.
pub async fn serve_acp_with_io<W, R>(
    outgoing: W,
    incoming: R,
    app_config: AppConfig,
    workspace: PathBuf,
    model_override: Option<String>,
    client_profile: AcpClientProfile,
) -> Result<()>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    tracing::debug!(target: "acp", ?client_profile, "ACP client profile selected; R1 profiles are behavior-identical");
    let factory_config = app_config.clone();
    let core_factory: AcpCoreFactory = Rc::new(move |cwd, mcp_servers| {
        crate::infrastructure::composition::build_acp_core(&factory_config, cwd, false, mcp_servers)
            .map_err(|e| {
                tracing::error!("ACP build_acp_core failed: {e:#}");
                acp::Error::internal_error()
            })
    });
    serve_acp_with_acp_core_factory(
        outgoing,
        incoming,
        app_config,
        workspace,
        model_override,
        core_factory,
    )
    .await
}

/// Serve ACP over caller-provided byte streams with injected turn composition.
///
/// This is the deterministic harness seam: tests can provide a `CliCore` backed
/// by a scripted provider while still driving real ACP JSON-RPC dispatch and the
/// real `turn::run_turn` call inside `RustainAcpAgent::prompt`.
async fn observe_acp_events(mut events: mpsc::UnboundedReceiver<AppEvent>) {
    while let Some(event) = events.recv().await {
        tracing::info!(?event, "ACP local lifecycle event");
    }
}
/// Decorate a per-session ACP factory once the durable ACP lifecycle seams
/// exist. Each resulting composite receives one shared runtime for all of its
/// MCP clients, so disconnect reaps the nodes created by that composite.
#[cfg(feature = "mcp")]
fn with_mcp_task_runtime(
    core_factory: AcpCoreFactory,
    node_tree: NodeTree,
    node_journal: Arc<NodeJournal>,
    workspace: std::path::PathBuf,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
) -> AcpCoreFactory {
    Rc::new(move |cwd, mcp_servers| {
        let core = core_factory(cwd, mcp_servers)?;
        if let Some(composite) = core
            .tools
            .as_any()
            .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>(
        ) {
            let task_nodes: Arc<dyn crate::domain::ports::TaskNodes> = Arc::new(node_tree.clone());
            let supervised: Arc<dyn crate::domain::ports::SupervisedNodes> =
                Arc::new(node_tree.clone());
            let room: Arc<dyn crate::domain::ports::RoomJournal> =
                Arc::new(crate::infrastructure::subagent::NodeRoomJournal::new(
                    node_journal.clone(),
                    Some(domain_tx.clone()),
                ));
            let task_clock: Arc<dyn crate::domain::clock::Clock> =
                Arc::new(crate::domain::clock::SystemClock::default());
            let task_runtime = Arc::new(crate::adapters::mcp::task_driver::McpTaskRuntime::new(
                task_nodes,
                supervised,
                room.clone(),
                task_clock,
            ));
            // 17.5b (Task 6): wire the input-request artifact sink (AC3).
            let artifact_store: Arc<dyn crate::domain::ports::ArtifactStore> = Arc::new(
                crate::adapters::artifact::FileSystemArtifactStore::new(&workspace),
            );
            let artifact_host = crate::domain::models::HostBinding::new(
                "local",
                format!("workspace:{}", workspace.display()),
            );
            let coordinator_authority = crate::domain::models::CapabilityToken::r1_root(
                crate::domain::models::AgentId::root(),
            );
            let sink: Arc<dyn crate::domain::ports::ArtifactSink> =
                Arc::new(crate::infrastructure::subagent::JournalArtifactSink::new(
                    artifact_store,
                    room,
                    coordinator_authority.id,
                    artifact_host,
                    crate::domain::models::AgentId::local_operator(),
                ));
            task_runtime.set_artifact_sink(sink);
            for client in composite.mcp_clients() {
                client.set_task_runtime(Arc::clone(&task_runtime));
            }
        }
        Ok(core)
    })
}

pub async fn serve_acp_with_acp_core_factory<W, R>(
    outgoing: W,
    incoming: R,
    app_config: AppConfig,
    workspace: PathBuf,
    model_override: Option<String>,
    core_factory: AcpCoreFactory,
) -> Result<()>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let clock = Arc::new(SystemClock::default());
    let now_fn = {
        let clock = clock.clone();
        Arc::new(move || clock.wall_now_ms())
    };
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    #[cfg(feature = "mcp")]
    let domain_tx = event_tx.clone();
    let event_observer = tokio::spawn(observe_acp_events(event_rx));
    let node_journal = Arc::new(NodeJournal::open_workspace(&workspace).await?);
    let node_tree = NodeTree::with_event_tx(event_tx, now_fn)
        .with_journal(node_journal.clone())
        .with_host_binding(crate::infrastructure::subagent::current_host_binding(
            &workspace,
        ));
    #[cfg(feature = "mcp")]
    let core_factory = with_mcp_task_runtime(
        core_factory,
        node_tree.clone(),
        node_journal,
        workspace.clone(),
        domain_tx,
    );
    let result = serve_acp_with_acp_core_factory_and_node_tree(
        outgoing,
        incoming,
        app_config,
        model_override,
        core_factory,
        node_tree,
        workspace,
        default_acp_id_source(),
    )
    .await;
    event_observer.abort();
    result
}

pub async fn serve_acp_with_core_factory<W, R>(
    outgoing: W,
    incoming: R,
    app_config: AppConfig,
    workspace: PathBuf,
    model_override: Option<String>,
    core_factory: CoreFactory,
) -> Result<()>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let clock = Arc::new(SystemClock::default());
    let now_fn = {
        let clock = clock.clone();
        Arc::new(move || clock.wall_now_ms())
    };
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    #[cfg(feature = "mcp")]
    let domain_tx = event_tx.clone();
    let event_observer = tokio::spawn(observe_acp_events(event_rx));
    let node_journal = Arc::new(NodeJournal::open_workspace(&workspace).await?);
    let node_tree = NodeTree::with_event_tx(event_tx, now_fn)
        .with_journal(node_journal.clone())
        .with_host_binding(crate::infrastructure::subagent::current_host_binding(
            &workspace,
        ));
    let acp_factory: AcpCoreFactory = Rc::new(move |cwd, mcp_servers| {
        core_factory(cwd, mcp_servers).map(crate::infrastructure::composition::AcpCore::from)
    });
    #[cfg(feature = "mcp")]
    let acp_factory = with_mcp_task_runtime(
        acp_factory,
        node_tree.clone(),
        node_journal,
        workspace.clone(),
        domain_tx,
    );
    let result = serve_acp_with_acp_core_factory_and_node_tree(
        outgoing,
        incoming,
        app_config,
        model_override,
        acp_factory,
        node_tree,
        workspace,
        deterministic_acp_id_source(),
    )
    .await;
    event_observer.abort();
    result
}

pub async fn serve_acp_with_core_factory_and_node_tree<W, R>(
    outgoing: W,
    incoming: R,
    app_config: AppConfig,
    model_override: Option<String>,
    core_factory: CoreFactory,
    node_tree: NodeTree,
    default_workspace: PathBuf,
) -> Result<()>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let acp_factory: AcpCoreFactory = Rc::new(move |cwd, mcp_servers| {
        core_factory(cwd, mcp_servers).map(crate::infrastructure::composition::AcpCore::from)
    });
    serve_acp_with_acp_core_factory_and_node_tree(
        outgoing,
        incoming,
        app_config,
        model_override,
        acp_factory,
        node_tree,
        default_workspace,
        deterministic_acp_id_source(),
    )
    .await
}

/// Serve ACP with injected turn composition and an injected `NodeTree`.
///
/// This keeps production stdio unchanged while allowing integration tests to
/// assert that `session/new` materializes a Self node and EOF teardown removes it.
pub async fn serve_acp_with_acp_core_factory_and_node_tree<W, R>(
    outgoing: W,
    incoming: R,
    app_config: AppConfig,
    model_override: Option<String>,
    core_factory: AcpCoreFactory,
    node_tree: NodeTree,
    default_workspace: PathBuf,
    id_source: Rc<dyn Fn() -> String>,
) -> Result<()>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let auth_store: Arc<dyn AuthStorePort> =
        Arc::new(crate::adapters::auth_store::FileAuthStore::new());
    serve_acp_with_acp_core_factory_and_node_tree_and_auth_store(
        outgoing,
        incoming,
        app_config,
        model_override,
        core_factory,
        node_tree,
        default_workspace,
        id_source,
        auth_store,
    )
    .await
}

pub async fn serve_acp_with_acp_core_factory_and_node_tree_and_auth_store<W, R>(
    outgoing: W,
    incoming: R,
    app_config: AppConfig,
    model_override: Option<String>,
    core_factory: AcpCoreFactory,
    node_tree: NodeTree,
    default_workspace: PathBuf,
    id_source: Rc<dyn Fn() -> String>,
    auth_store: Arc<dyn AuthStorePort>,
) -> Result<()>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let sessions: SharedSessions = Rc::new(RefCell::new(HashMap::new()));
            let cleanup_sessions = sessions.clone();
            let cleanup_tree = node_tree.clone();
            let (session_update_tx, mut session_update_rx) =
                mpsc::unbounded_channel::<SessionNotify>();
            let (permission_tx, mut permission_rx) = mpsc::unbounded_channel::<PermissionAsk>();
            let agent = RustainAcpAgent::new(
                app_config,
                core_factory,
                model_override,
                default_workspace,
                session_update_tx,
                permission_tx,
                sessions,
                node_tree,
                auth_store,
                id_source,
            );
            let (conn, handle_io) = acp::AgentSideConnection::new(
                agent,
                outgoing,
                incoming,
                |fut: LocalBoxFuture<'static, ()>| {
                    tokio::task::spawn_local(fut);
                },
            );
            let conn = Rc::new(conn);

            let update_conn = conn.clone();
            tokio::task::spawn_local(async move {
                while let Some(SessionNotify { notification, ack }) = session_update_rx.recv().await
                {
                    let _ = ack.send(update_conn.session_notification(notification).await);
                }
            });

            let permission_conn = conn.clone();
            #[cfg(feature = "test-instrumentation")]
            let completed = Rc::new(Cell::new(0));
            #[cfg(feature = "test-instrumentation")]
            let observed_completed = completed.clone();
            let permission_pump = async move {
                while let Some(PermissionAsk { request, mut ack }) = permission_rx.recv().await {
                    // Story 19.20c: cancellation drops the asking receiver.
                    // Retire an unanswered SDK call before serving the next ask;
                    // its late response must never settle a later approval.
                    tokio::select! {
                        biased;
                        _ = ack.closed() => {}
                        response = permission_conn.request_permission(request) => {
                            let _ = ack.send(response);
                        }
                    }
                    #[cfg(feature = "test-instrumentation")]
                    completed.set(completed.get() + 1);
                }
            };
            #[cfg(feature = "test-instrumentation")]
            tokio::task::spawn_local(permission_test_probe::instrument(
                permission_pump,
                observed_completed,
            ));
            #[cfg(not(feature = "test-instrumentation"))]
            tokio::task::spawn_local(permission_pump);

            let result = handle_io.await.map_err(anyhow::Error::from);
            // The session map is the ownership ledger for this ACP server.
            // Never infer transport membership from NodeTree ownership: the
            // tree may be shared with unrelated Self-rooted runtimes.
            let cleanup: Vec<_> = cleanup_sessions
                .borrow()
                .iter()
                .map(|(session_id, state)| {
                    (
                        session_id.clone(),
                        state.cancel.clone(),
                        Arc::clone(&state.core.approval),
                        state.conversation_id.clone(),
                        Arc::clone(&state.core.tools),
                    )
                })
                .collect();
            for (session_id, cancel, approval, conversation_id, tools) in cleanup {
                let was_cancelled = cancel.is_cancelled();
                // EOF must cancel execution before the LocalSet is dropped.
                // Settle here as well: the scheduler need not be polled again
                // after transport shutdown. cancel_by_source is idempotent.
                cancel.cancel();
                approval
                    .cancel_by_source(
                        &crate::domain::models::ApprovalSource::AcpSession {
                            session_id: session_id.clone(),
                            conversation_id,
                        },
                        crate::domain::services::approval_runtime::CancelReason::SourceAborted,
                    )
                    .await;
                // Task 4 teardown order: terminal + deregister BEFORE the
                // (time-bounded) MCP reap, so a hung child cannot leave a
                // non-terminal registered node.
                let agent_id = AgentId::from_validated(session_id);
                let terminal_state = if was_cancelled {
                    NodeState::Cancelled
                } else {
                    NodeState::Completed
                };
                cleanup_tree.set_state(&agent_id, terminal_state).await;
                cleanup_tree.deregister(&agent_id).await;
                if let Some(composite) = tools.as_any().downcast_ref::<
                    crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter,
                >() {
                    composite.stop_mcp_connections().await;
                }
            }
            result
        })
        .await
}
