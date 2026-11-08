//! `/artifact apply`, `/artifact resolve` and `/peer add` as queue items (Story 21.6 6-D,
//! AC19). The payloads moved here from the old artifact / peer-add card slots of
//! `TuiState`; their saved-focus fields are gone (a decision is a frame, so
//! closing it restores exactly what it interrupted). Every item is keyed to the
//! conversation whose command raised it: another tab neither answers nor abandons it.

use super::memory::holds_in;
use super::{Arrival, DecisionDraft, DecisionKind, DecisionSubject, Raised, raise};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{ArtifactRef, OperatorApplyFinding, PeerId, PeerTicket};
use crate::domain::ports::PeerAddress;

/// What a [`PendingArtifactCard`] is asking the operator to confirm.
///
/// ⛔ **The mode parameterises CONTENT, never dispatch.** Both modes use the same two
/// keys, the same effects and the same render branch; `apply_card::bindings_for` carries
/// a compile-time assertion that the key→choice mapping is identical across modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactCardMode {
    /// `/artifact apply <id>` — a confirmed **workspace write**.
    Apply,
    /// `/artifact resolve <id> present|absent` — a confirmed durable record of what the
    /// operator reports they saw. ⛔ Performs no workspace write, and the card must say so
    /// before `y`.
    Resolve(OperatorApplyFinding),
}

/// Confirmed decision card for `/artifact apply` and `/artifact resolve`.
///
/// ⚠ **The rename is deliberately partial** (Story 18.3a-f, ruling P3):
/// `InputAction::ApplyCardAccept`, `InputAction::ApplyCardDecline` and
/// `render_apply_card_lines` keep their apply-flavoured names because
/// `conformance_18_3a_d_apply.rs` pins them by name. ⛔ Do not "fix" the asymmetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingArtifactCard {
    pub conversation_id: crate::domain::models::tab::ConversationId,
    pub artifact: ArtifactRef,
    pub files: Vec<String>,
    pub workspace: std::path::PathBuf,
    pub predates_apply_records: bool,
    pub mode: ArtifactCardMode,
}

/// A `/peer add` awaiting the operator's confirm (Story 18.4b, AC3).
///
/// # Why this is its own payload and not the apply card's
///
/// The apply card's key table is single-sourced and consulted **mode-blind**
/// (`apply_card::choice_for_key` always reads `APPLY_CARD_BINDINGS`), so borrowing it
/// would give any peer surface an unpainted `y` that resolves. For a pin that is a
/// single-keystroke rebind, which is precisely what the key-mismatch alarm exists to
/// forbid — so this prompt has its own kind, its own two effects and its own two-key
/// table. ⛔ Nothing here routes through `choice_for_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPeerAdd {
    pub conversation_id: String,
    /// The alias the operator typed. ⛔ Never a name the ticket suggested.
    pub alias: String,
    pub ticket: PeerTicket,
    /// Derived once, through the one identity derivation, before the card is shown — so
    /// the fingerprint on screen is the one that gets pinned.
    pub peer_id: PeerId,
    /// The rendered card body, built by the shared copy module so the CLI and TUI
    /// confirms cannot diverge.
    pub card: String,
    /// The ticket's claimed reach, **already filtered** (Story 18.4d, D15).
    pub reach: Option<PeerAddress>,
    /// `true` when this card is a reach-only refresh of an already-pinned key: the
    /// resolution path then writes only the address (no pin, no admission record).
    pub reach_refresh: bool,
}

/// Raise the confirm of `/artifact apply` or `/artifact resolve` (the user asked:
/// Requested arrival). Keyed to the artifact; the mode picks the kind (and its rail).
pub fn raise_artifact(state: &mut TuiState, card: PendingArtifactCard) -> Raised {
    let kind = match card.mode {
        ArtifactCardMode::Apply => DecisionKind::ArtifactApply,
        ArtifactCardMode::Resolve(_) => DecisionKind::ArtifactResolve,
    };
    let id = card.artifact.id.to_string();
    let draft = DecisionDraft::new(kind, "")
        .conversation(card.conversation_id.clone())
        .source(id.clone())
        .summary(id.chars().take(12).collect::<String>())
        .subject(DecisionSubject::Artifact(Box::new(card)));
    raise(state, draft, Arrival::Requested)
}

/// True while an artifact decision (apply or resolve) of `conversation_id` waits.
#[must_use]
pub fn artifact_pending(state: &TuiState, conversation_id: &str) -> bool {
    holds_in(state, DecisionKind::ArtifactApply, conversation_id)
        || holds_in(state, DecisionKind::ArtifactResolve, conversation_id)
}

/// Raise the confirm of `/peer add` (the user asked: Requested arrival). Keyed to the
/// alias the operator typed.
pub fn raise_peer_add(state: &mut TuiState, pending: PendingPeerAdd) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::PeerAdd, "")
        .conversation(pending.conversation_id.clone())
        .source(pending.alias.clone())
        .summary(pending.alias.clone())
        .subject(DecisionSubject::PeerAdd(Box::new(pending)));
    raise(state, draft, Arrival::Requested)
}

/// True while a peer import of `conversation_id` waits for its answer.
#[must_use]
pub fn peer_add_pending(state: &TuiState, conversation_id: &str) -> bool {
    holds_in(state, DecisionKind::PeerAdd, conversation_id)
}
