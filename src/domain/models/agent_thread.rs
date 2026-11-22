//! The peer exchange an Agents row (a delegation to a peer rustain host) can open as a sent
//! Thread (story `agents-thread`).
//!
//! The Agents roster knows only node ids; the A2A adapter alone can say which row binds to
//! which peer exchange. These two types carry that answer across the seam without teaching
//! the subagent tree anything about A2A (`AgentRowView` is unchanged).

/// The verified binding of one Agents row to a peer exchange (D2, D3).
///
/// Offered only for a delegation row whose remote task this host dispatched to that peer.
/// Both fields are raw: every painted use sanitizes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentThreadOffer {
    /// The roster alias of the peer, decoded from the row's node id and found in the roster.
    pub peer: String,
    /// The remote task id the delegation was minted from (the sender's own `messageId` on a
    /// rustain host).
    pub task: String,
}

/// What resolving an offer on the peer's host produced (D4). Built by the A2A adapter, applied
/// by the host bridge; carries no handle and no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentThreadOutcome {
    /// Exactly one item of the peer's scoped list names the task: its thread, rendered by
    /// the same renderer `/team thread <peer> <item>` uses.
    Opened {
        peer: String,
        item: String,
        /// The sender-side projection of the thread (or its refusal sentence).
        message: String,
        /// The newest eligible follow-up turn of the structured read, if any (D6).
        follow_up: Option<String>,
    },
    /// Two or more items name the task: nothing was opened or read; these are the exact
    /// candidate item ids in list order, raw.
    Candidates { peer: String, items: Vec<String> },
    /// Nothing was opened; the sentence says why (no item for the task, a failed list read,
    /// an offer that could no longer be verified).
    Missing(String),
}
