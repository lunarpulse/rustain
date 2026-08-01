//! `InboundPeerRuntime` — the narrow domain seam through which a network front
//! door hands an **admitted** remote task to the local execution core.
//!
//! Story 18.1b. The A2A HTTP server is a *protocol adapter*; the thing that
//! knows how to register a `OwnershipKind::Peer` node and drive a turn is the
//! daemon. Coupling them directly would make `adapters/a2a` depend on
//! `adapters/daemon`, so the dependency is inverted here exactly as
//! [`RoomJournal`](super::RoomJournal) and `SupervisedNodes` do it: the port
//! carries no A2A, JSON-RPC, axum or crypto type, and the concrete
//! implementation is named only at a composition root.
//!
//! # The two invariants this seam exists to preserve
//!
//! 1. **The node handle stays LOCAL.** `start` registers the task as a local
//!    `Peer`/`Remote` node under *our* authority. The remote submitter never
//!    receives a live handle into our tree — "a remote peer never receives
//!    authority by discovery alone".
//! 2. **Nothing here awaits a human.** [`InboundPeerRuntime::request_admission_approval`]
//!    *raises* an approval and returns a ticket; the caller MUST NOT block its
//!    request on the returned receiver. A remote HTTP request held open across
//!    an operator keypress dies on the server's request deadline and every
//!    client retry queues another prompt.

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::models::{AgentId, NodeState, PeerId};
use crate::domain::ports::PeerResponsePolicy;

/// One admitted inbound task, ready to execute as a local peer node.
#[derive(Debug, Clone)]
pub struct InboundPeerTask {
    /// The node id to register. Minted by the adapter so `(peer, task)` stays
    /// reversible; the runtime treats it as opaque.
    pub node_id: AgentId,
    /// Cryptographic identity of the submitting peer, for authority scoping and
    /// room-event observability.
    pub peer_id: PeerId,
    /// The submitted instruction text.
    pub text: String,
    /// The `subagent_type` marker to stamp on the node.
    ///
    /// Supplied by the protocol adapter so this seam never has to know one
    /// protocol's naming, and so restart reconciliation can select exactly the
    /// nodes this front door owns.
    pub subagent_type: String,
    /// Response behavior snapshotted when the front door admitted this task.
    pub response_policy: PeerResponsePolicy,
}

/// Operator decision for one pending sender-consent card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundApprovalDecision {
    /// Release the tasks currently waiting without creating durable trust.
    AllowOnce,
    /// Durably trust the sender, then release every waiting task.
    AllowAlways,
    /// Decline every task currently waiting for this sender.
    Decline,
}

/// Outcome of raising an admission approval without waiting for it.
pub struct InboundApprovalTicket {
    /// `true` when a *human* decision is outstanding. `false` means policy
    /// already resolved it and [`Self::decision`] is immediately ready.
    ///
    /// This flag is the whole point of the type: it is what lets the caller
    /// answer `auth-required` instead of holding the request open.
    pub pending: bool,
    /// Resolution applied to every task grouped under this sender's card.
    pub decision: oneshot::Receiver<InboundApprovalDecision>,
}

impl std::fmt::Debug for InboundApprovalTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundApprovalTicket")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

/// Failure surface of the seam. Carries sanitized strings so no infrastructure
/// error type leaks into `domain/`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundPeerError {
    /// Registering the peer node in the tree failed.
    Register(String),
    /// The execution core could not be built or is not available.
    Unavailable(String),
}

impl InboundPeerError {
    /// Construct a sanitized registration failure for runtime implementors
    /// outside this crate; the enum remains non-exhaustive for forward
    /// compatibility.
    #[must_use]
    pub fn registration(detail: impl Into<String>) -> Self {
        Self::Register(detail.into())
    }

    /// Construct a sanitized execution-unavailable failure for runtime
    /// implementors outside this crate.
    #[must_use]
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::Unavailable(detail.into())
    }
}

impl std::fmt::Display for InboundPeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Register(detail) => write!(f, "registering inbound peer node: {detail}"),
            Self::Unavailable(detail) => write!(f, "inbound peer execution unavailable: {detail}"),
        }
    }
}

impl std::error::Error for InboundPeerError {}

/// Execute admitted inbound peer tasks on the local core.
#[async_trait::async_trait]
pub trait InboundPeerRuntime: Send + Sync {
    /// Resolve response automation for a verified transport identity.
    fn response_policy(&self, peer_id: &PeerId) -> PeerResponsePolicy {
        let _ = peer_id;
        PeerResponsePolicy::default()
    }

    /// Register `task.node_id` as a local `Peer`/`Remote` node and start driving
    /// its turn. Returns as soon as the node exists and the turn is spawned —
    /// it never awaits turn completion.
    ///
    /// `cancel` is the task's cancellation token: the same token the driven turn
    /// selects on, so `tasks/cancel` reaches a running turn.
    ///
    /// The returned watch is the node's real lifecycle channel. Callers project
    /// it onto their own protocol vocabulary; they never mutate it.
    async fn start(
        &self,
        task: InboundPeerTask,
        cancel: CancellationToken,
    ) -> Result<watch::Receiver<NodeState>, InboundPeerError>;

    /// Whether this runtime has the durable sender-consent gate composed.
    ///
    /// The default preserves lightweight test and discovery runtimes. Production
    /// daemon composition returns `true`, making coarse listener `allow` policy
    /// insufficient to bypass per-sender consent.
    fn enforces_sender_consent(&self) -> bool {
        false
    }

    /// Raise a human admission approval for `peer_id` **without awaiting it**.
    ///
    /// Implementations MUST return before any human interaction occurs.
    async fn request_admission_approval(
        &self,
        peer_id: &PeerId,
        summary: &str,
    ) -> Result<InboundApprovalTicket, InboundPeerError>;

    /// Remove and return the agent's textual answer for a finished node, if one
    /// was produced.
    ///
    /// Deliberately narrow: the seam returns *text*, never a conversation, node,
    /// or result-store handle, so a caller physically cannot project internal
    /// state it was never given. Taking it makes lifecycle transfer the sole
    /// owner and prevents completed answers accumulating for daemon lifetime.
    async fn take_result_text(&self, node_id: &AgentId) -> Option<String>;

    /// Host-sensitive text fragments that must never be disclosed to a remote
    /// submitter. Implementations derive these from local-only state (for
    /// example, long system-prompt lines); the adapter uses them solely as
    /// scrub needles and never serves the fragments themselves.
    async fn disclosure_forbidden_fragments(&self) -> Vec<String>;

    /// Resolve inbound peer nodes a previous process left non-terminal, and
    /// return their ids.
    ///
    /// Called once at listener startup. Durable host-side *resumption* is out of
    /// scope, but a task that vanished with its process must not read as a
    /// zombie `working` forever — the front door turns each returned id back
    /// into a `failed` task carrying an explicit restart reason.
    ///
    /// `subagent_type` is supplied by the caller because the marker belongs to
    /// the protocol adapter, not to this seam: an outbound delegation this
    /// instance issued is a different node with a different owner.
    async fn reconcile_orphaned_tasks(&self, subagent_type: &str) -> Vec<AgentId>;
}
