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
use crate::infrastructure::subagent::node_tree::NodeTree;

use super::agent::{
    AcpCoreFactory, CoreFactory, PermissionAsk, RustainAcpAgent, SessionNotify, SharedSessions,
};

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
    let event_observer = tokio::spawn(observe_acp_events(event_rx));
    let node_tree = NodeTree::with_event_tx(event_tx, now_fn);
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
    let event_observer = tokio::spawn(observe_acp_events(event_rx));
    let node_tree = NodeTree::with_event_tx(event_tx, now_fn);
    let acp_factory: AcpCoreFactory = Rc::new(move |cwd, mcp_servers| {
        core_factory(cwd, mcp_servers).map(crate::infrastructure::composition::AcpCore::from)
    });
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
            tokio::task::spawn_local(async move {
                while let Some(PermissionAsk { request, ack }) = permission_rx.recv().await {
                    let _ = ack.send(permission_conn.request_permission(request).await);
                }
            });

            let result = handle_io.await.map_err(anyhow::Error::from);
            // The session map is the ownership ledger for this ACP server.
            // Never infer transport membership from NodeTree ownership: the
            // tree may be shared with unrelated Self-rooted runtimes.
            let session_ids: Vec<String> = cleanup_sessions.borrow().keys().cloned().collect();
            for session_id in session_ids {
                let agent_id = AgentId::from_validated(session_id.clone());
                let terminal_state = if cleanup_sessions
                    .borrow()
                    .get(&session_id)
                    .is_some_and(|s| s.cancel.is_cancelled())
                {
                    NodeState::Cancelled
                } else {
                    NodeState::Completed
                };
                cleanup_tree.set_state(&agent_id, terminal_state).await;
                cleanup_tree.deregister(&agent_id).await;
            }
            result
        })
        .await
}
