//! `SessionHolderPort` — tri-state query for the daemon-held session in a workspace.
//!
//! Story 13.5b uses this to refuse deletion of a session a running daemon is
//! holding. The port is intentionally minimal and redaction-clean: it carries
//! only `conversation_id`, `pid`, and `channels` — never socket paths, nonces,
//! or boot ids.

use std::path::Path;

use crate::domain::models::channel_kind::ChannelKind;

/// A live daemon's hold on a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldSession {
    pub conversation_id: String,
    pub pid: u32,
    pub channels: Vec<ChannelKind>,
}

/// Result of asking whether a session is currently held in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderState {
    /// No PID file or the daemon is not alive/verified.
    NoDaemon,
    /// A live daemon holds this session.
    HeldBy(HeldSession),
    /// A daemon appears to be alive but could not be queried (timeout, IO,
    /// protocol error). Fail-closed: treat as potentially held.
    Unknown,
}

/// Query side of the in-use guard. Implementations must be side-effect-free:
/// they may touch the local Unix socket but must not register as an attached
/// writer or disturb the daemon's turn.
#[async_trait::async_trait]
pub trait SessionHolderPort: Send + Sync {
    /// Return the holder state for `workspace`, completing within a bounded
    /// time (≤ 2 s). On timeout or unqueryable daemon, return `Unknown`.
    async fn live_holder(&self, workspace: &Path) -> HolderState;
}
