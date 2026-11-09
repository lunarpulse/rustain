//! Team message proposals awaiting the operator's decision.

use std::sync::Arc;

use super::{
    Arrival, DecisionDraft, DecisionKind, DecisionSubject, Raised, Responder, StaleCopy, raise,
    withdraw_where,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::TeamProposal;

pub const PROPOSAL_STALE: StaleCopy =
    StaleCopy("That proposal is no longer waiting. Nothing was sent.");
pub const PROPOSAL_WITHDRAWN: StaleCopy =
    StaleCopy("Proposal withdrawn with the conversation. Nothing was sent.");

/// The immutable source snapshot and the frame's non-resolving Review state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamProposalPayload {
    pub proposal: Arc<TeamProposal>,
    pub reviewing: bool,
}

/// Bounded, single-line display of peer-provided recipient aliases.
#[must_use]
pub fn proposal_aliases(proposal: &TeamProposal) -> String {
    let mut aliases = proposal
        .recipients
        .iter()
        .take(16)
        .map(|alias| {
            let safe = crate::domain::services::peer_text::sanitize_display_line(alias);
            let mut chars = safe.chars();
            let mut compact: String = chars.by_ref().take(128).collect();
            if chars.next().is_some() {
                compact.push('…');
            }
            compact
        })
        .collect::<Vec<_>>();
    if proposal.recipients.len() > 16 {
        aliases.push(format!("… and {} more", proposal.recipients.len() - 16));
    }
    aliases.join(", ")
}

pub fn raise_team_proposal(
    state: &mut TuiState,
    proposal: Arc<TeamProposal>,
    responder: Responder,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::TeamProposal, "")
        .source(proposal.id.clone())
        .conversation(proposal.conversation_id.clone())
        .summary(proposal_aliases(&proposal))
        .responder(responder)
        .subject(DecisionSubject::TeamProposal(Box::new(
            TeamProposalPayload {
                proposal,
                reviewing: false,
            },
        )));
    raise(state, draft, Arrival::Automatic)
}

#[must_use]
pub fn team_proposal_pending(state: &TuiState) -> bool {
    state
        .decisions
        .items()
        .any(|item| item.kind == DecisionKind::TeamProposal && !item.is_stale())
}

#[must_use]
pub fn team_proposal_presenting(state: &TuiState) -> bool {
    state.decisions.presenting(DecisionKind::TeamProposal)
}
/// The shared composer and an attached recipient composition both protect draft text.
#[must_use]
pub fn team_proposal_draft_blocked(state: &TuiState, responder: Responder) -> bool {
    !state.input_buffer.is_empty()
        || (responder == Responder::Attached
            && crate::adapters::tui::handlers::recipient_actions::pending_composition(state)
                .is_some_and(|draft| !draft.is_empty()))
}

pub fn withdraw_team_proposal_conversation(state: &mut TuiState, conversation_id: &str) {
    withdraw_where(state, PROPOSAL_WITHDRAWN, |item| {
        item.kind == DecisionKind::TeamProposal
            && item.conversation_id.as_deref() == Some(conversation_id)
    });
}

pub fn withdraw_team_proposal_id(
    state: &mut TuiState,
    conversation_id: &str,
    proposal_id: &str,
    withdrawn: bool,
) {
    withdraw_where(
        state,
        if withdrawn {
            PROPOSAL_WITHDRAWN
        } else {
            PROPOSAL_STALE
        },
        |item| {
            item.kind == DecisionKind::TeamProposal
                && item.conversation_id.as_deref() == Some(conversation_id)
                && item.source == proposal_id
        },
    );
}

pub fn withdraw_all_team_proposals(state: &mut TuiState) {
    withdraw_where(state, PROPOSAL_WITHDRAWN, |item| {
        item.kind == DecisionKind::TeamProposal
    });
}
