//! `/team retract` and the auto-response retract as one queue kind (Story 21.6 6-D, AC20).
//!
//! The payload moved here from the old retract card slot (both hosts) and the attached
//! retract confirmation; the saved-focus field is gone. `armed` keeps its meaning: a
//! disarmed card (the confirm-time read did not resolve, or the item is already removed on
//! the peer's host) answers `[n]` only, and a `y` is consumed, never a confirm.

use super::{
    Arrival, DecisionDraft, DecisionKind, DecisionSubject, NO_LONGER_PENDING, Raised, Responder,
    raise, withdraw_key,
};
use crate::adapters::tui::state::TuiState;

/// What a retract decision marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetractTarget {
    /// `/team retract <peer> <item>`: one item on a peer's host.
    Item { peer: String, item: String },
    /// The palette's retract of one auto-sent message of this host.
    AutoResponse { message_id: String },
}

/// One retract decision awaiting `[y]`/`[n]` (Story 19.16f `AC4`).
///
/// Raised only from a confirm-time read of the peer's own list (never from a cached board
/// render) or from the palette's retract route. Cancelling is free: nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRetractPayload {
    pub target: RetractTarget,
    /// Local host-resolved identity; attached previews disclose it in `card` instead.
    pub principal: Option<String>,
    /// The conversation whose command raised it (`""` for an attached writer's items,
    /// which are decidable anywhere the writer is).
    pub conversation_id: String,
    /// The item's task as the peer listed it — the sender's ledger rows carry it.
    pub task: Option<String>,
    /// The rendered card body, one logical line per `'\n'`, without its key line (the
    /// frame generates the rail).
    pub card: String,
    pub armed: bool,
    /// The attached host's preview request this item answers (`TeamRetractDecision`).
    pub request: Option<String>,
}

impl TeamRetractPayload {
    /// The queue source identity of the item.
    #[must_use]
    pub fn source(&self) -> String {
        match &self.target {
            RetractTarget::Item { peer, item } => format!("{peer}/{item}"),
            RetractTarget::AutoResponse { message_id } => message_id.clone(),
        }
    }

    /// The row label of the item.
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.target {
            RetractTarget::Item { peer, item } => format!("{peer} {item}"),
            RetractTarget::AutoResponse { .. } => "auto-sent message".to_owned(),
        }
    }
}

/// Raise a retract decision (the user asked: Requested arrival). `responder` names the
/// route that answers it: the local runtime, or the attached writer's frames.
pub fn raise_team_retract(
    state: &mut TuiState,
    payload: TeamRetractPayload,
    responder: Responder,
) -> Raised {
    let mut draft = DecisionDraft::new(DecisionKind::TeamRetract, "")
        .source(payload.source())
        .summary(payload.summary())
        .responder(responder);
    if !payload.conversation_id.is_empty() {
        draft = draft.conversation(payload.conversation_id.clone());
    }
    let draft = draft.subject(DecisionSubject::TeamRetract(Box::new(payload)));
    raise(state, draft, Arrival::Requested)
}

/// True while a retract decision waits (a second `/team retract` is refused).
#[must_use]
pub fn team_retract_pending(state: &TuiState) -> bool {
    state
        .decisions
        .items()
        .any(|item| item.kind == DecisionKind::TeamRetract && !item.is_stale())
}

/// True while a user-started confirmation is presenting on the current tab.
#[must_use]
pub fn confirmation_pending(state: &TuiState) -> bool {
    [
        DecisionKind::ArtifactApply,
        DecisionKind::ArtifactResolve,
        DecisionKind::PeerAdd,
        DecisionKind::TeamRetract,
        DecisionKind::Fork,
        DecisionKind::Rewind,
        DecisionKind::ExportOverwrite,
    ]
    .into_iter()
    .any(|kind| state.decisions.presenting(kind))
}

/// The attached preview request settled elsewhere (answered, refused, the daemon closed
/// it): its item leaves the queue, or turns stale while its frame is open.
pub fn withdraw_team_retract_request(state: &mut TuiState, request: &str) {
    let key = state
        .decisions
        .items()
        .find(|item| {
            item.kind == DecisionKind::TeamRetract
                && matches!(
                    &item.subject,
                    DecisionSubject::TeamRetract(payload) if payload.request.as_deref() == Some(request)
                )
        })
        .map(super::DecisionItem::key);
    if let Some(key) = key {
        withdraw_key(state, &key, NO_LONGER_PENDING);
    }
}
