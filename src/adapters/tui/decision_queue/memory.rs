//! Memory consolidation and `/memory forget` as queue items (Story 21.6 6-D, AC18).
//!
//! The payloads moved here from the old singleton consolidation / forget card slots of
//! `TuiState` and the attached token slot (AD-2110). A consolidation is
//! one item in both hosts: the local one is keyed to the conversation it was proposed for,
//! the attached one to the daemon's `ProposalToken` (the confused-deputy guard: the frame
//! that resolves an item carries the token the item was raised under, never another one).
//! Forget keeps its per-row selection inside the frame.

use super::{Arrival, DecisionDraft, DecisionKind, DecisionSubject, Raised, Responder, raise};
use crate::adapters::daemon::protocol::{ClientFrame, ProposalToken};
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::tab::ConversationId;
use crate::domain::models::{MemoryEntry, MemoryFact};

/// Story 11.2a: memory-consolidation review awaiting the operator. A list of proposed
/// durable facts, each paired with its selection flag (`true` = promoted on accept).
/// Nothing is written until the item is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConsolidationCard {
    pub conversation_id: ConversationId,
    pub proposals: Vec<(MemoryFact, bool)>,
}

/// A consolidation item: the review card and, on an attached writer, the daemon's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationPayload {
    pub card: PendingConsolidationCard,
    /// `Some` for an attached item: the token the daemon retained the proposal under.
    pub token: Option<ProposalToken>,
}

/// Story 11.4a: `/memory forget` disambiguation and confirm card. A list of the
/// fuzzy-matched entries to permanently remove, each with its stable content key (the
/// tombstone identity) and a selection flag. Space flips the focused row, `j`/`k`/arrows
/// move focus, `y` forgets every selected row, `n` cancels. Nothing is purged until `y`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingForgetCard {
    pub conversation_id: ConversationId,
    /// `(stable u64 content key, matched entry, selected)`.
    pub candidates: Vec<(u64, MemoryEntry, bool)>,
    /// Index of the row that currently has keyboard focus (0-based).
    pub focused_index: usize,
}

impl Eq for PendingForgetCard {}

impl PendingForgetCard {
    /// Flip the selection of the focused row.
    pub fn toggle_focused(&mut self) {
        if let Some((_, _, selected)) = self.candidates.get_mut(self.focused_index) {
            *selected = !*selected;
        }
    }

    /// Move focus one row, staying inside the list.
    pub fn move_focus(&mut self, forward: bool) {
        let last = self.candidates.len().saturating_sub(1);
        self.focused_index = if forward {
            (self.focused_index + 1).min(last)
        } else {
            self.focused_index.saturating_sub(1)
        };
    }

    /// The content keys of the selected rows (what `y` purges).
    #[must_use]
    pub fn selected_keys(&self) -> Vec<u64> {
        self.candidates
            .iter()
            .filter(|(_, _, selected)| *selected)
            .map(|(key, _, _)| *key)
            .collect()
    }
}

/// The event a resolved forget card emits: the selected keys on accept, none on decline.
#[must_use]
pub fn forget_resolution(card: &PendingForgetCard, accept: bool) -> AppEvent {
    AppEvent::MemoryForgetResolved {
        conversation_id: card.conversation_id.clone(),
        keys: if accept {
            card.selected_keys()
        } else {
            Vec::new()
        },
    }
}

/// The domain event the local host emits for an answered memory decision (consolidation or
/// forget, taken from `take_resolved()`): the one place the answer becomes an event, so the
/// host arm and its tests share it. `None` for any other subject.
#[must_use]
pub fn memory_answer_event(subject: DecisionSubject, accept: bool) -> Option<AppEvent> {
    match subject {
        DecisionSubject::Consolidation(payload) => Some(AppEvent::MemoryConsolidationResolved {
            conversation_id: payload.card.conversation_id.clone(),
            accepted: accepted_facts(&payload.card, accept),
        }),
        DecisionSubject::Forget(card) => Some(forget_resolution(&card, accept)),
        _ => None,
    }
}

/// The facts a resolved consolidation promotes: every proposal on accept, none on decline.
#[must_use]
pub fn accepted_facts(card: &PendingConsolidationCard, accept: bool) -> Vec<MemoryFact> {
    if accept {
        card.proposals
            .iter()
            .map(|(fact, _)| fact.clone())
            .collect()
    } else {
        Vec::new()
    }
}

/// Raise a local consolidation review (Automatic arrival: the model proposed it). One per
/// conversation; a newer proposal for the same conversation updates it in place.
pub fn raise_consolidation(state: &mut TuiState, card: PendingConsolidationCard) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Consolidation, "")
        .conversation(card.conversation_id.clone())
        .summary("memory")
        .subject(DecisionSubject::Consolidation(Box::new(
            ConsolidationPayload { card, token: None },
        )));
    raise(state, draft, Arrival::Automatic)
}

/// Raise an attached writer's consolidation review: keyed to the daemon's token, so a
/// newer proposal is its own item and an older one still resolves under its own token.
pub fn raise_attached_consolidation(
    state: &mut TuiState,
    token: ProposalToken,
    proposals: Vec<(MemoryFact, bool)>,
) -> Raised {
    let card = PendingConsolidationCard {
        conversation_id: String::new(),
        proposals,
    };
    let draft = DecisionDraft::new(DecisionKind::Consolidation, "")
        .source(token.0.to_string())
        .summary("memory")
        .responder(Responder::Attached)
        .subject(DecisionSubject::Consolidation(Box::new(
            ConsolidationPayload {
                card,
                token: Some(token),
            },
        )));
    raise(state, draft, Arrival::Automatic)
}

/// The frame an attached consolidation answer sends. `None` unless the token the item
/// carries is the one it was raised under (its key's source): a mismatched token never
/// resolves anything (the confused-deputy guard, AC18).
#[must_use]
pub fn consolidation_frame(
    source: &str,
    payload: &ConsolidationPayload,
    accept: bool,
) -> Option<ClientFrame> {
    let token = payload.token?;
    (token.0.to_string() == source).then_some(ClientFrame::ConsolidationResolve { token, accept })
}

/// Raise a `/memory forget` card (the user asked: Requested arrival). One per conversation.
pub fn raise_forget(state: &mut TuiState, card: PendingForgetCard) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Forget, "")
        .conversation(card.conversation_id.clone())
        .summary("memory")
        .subject(DecisionSubject::Forget(Box::new(card)));
    raise(state, draft, Arrival::Requested)
}

/// True while a forget card of `conversation_id` waits (`/memory forget` refuses a second).
#[must_use]
pub fn forget_pending(state: &TuiState, conversation_id: &str) -> bool {
    holds_in(state, DecisionKind::Forget, conversation_id)
}

/// True while a consolidation of `conversation_id` waits (deleting its conversation refuses).
#[must_use]
pub fn consolidation_pending(state: &TuiState, conversation_id: &str) -> bool {
    holds_in(state, DecisionKind::Consolidation, conversation_id)
}

/// A live item of `kind` keyed to `conversation_id` is queued.
pub(super) fn holds_in(state: &TuiState, kind: DecisionKind, conversation_id: &str) -> bool {
    state.decisions.items().any(|item| {
        item.kind == kind
            && !item.is_stale()
            && item.conversation_id.as_deref() == Some(conversation_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(summary: &str) -> MemoryEntry {
        MemoryEntry {
            timestamp: chrono::Local::now(),
            summary: summary.to_owned(),
            context: None,
        }
    }

    #[test]
    fn story21_6_forget_selection_and_focus_stay_inside_the_list() {
        let mut card = PendingForgetCard {
            conversation_id: "c1".into(),
            candidates: vec![(1, entry("a"), true), (2, entry("b"), true)],
            focused_index: 0,
        };
        card.move_focus(false);
        assert_eq!(card.focused_index, 0);
        card.move_focus(true);
        card.move_focus(true);
        assert_eq!(card.focused_index, 1);
        card.toggle_focused();
        assert_eq!(card.selected_keys(), vec![1]);
        card.toggle_focused();
        assert_eq!(card.selected_keys(), vec![1, 2]);
    }

    #[test]
    fn story21_6_forget_resolution_names_the_selected_keys_only_on_accept() {
        let mut card = PendingForgetCard {
            conversation_id: "c1".into(),
            candidates: vec![(7, entry("a"), true), (9, entry("b"), false)],
            focused_index: 0,
        };
        card.focused_index = 1;
        let accepted = forget_resolution(&card, true);
        assert!(matches!(&accepted, AppEvent::MemoryForgetResolved { keys, .. } if keys == &[7]));
        let declined = forget_resolution(&card, false);
        assert!(
            matches!(&declined, AppEvent::MemoryForgetResolved { keys, .. } if keys.is_empty())
        );
    }

    #[test]
    fn story21_6_consolidation_frame_requires_the_token_the_item_was_raised_under() {
        let payload = ConsolidationPayload {
            card: PendingConsolidationCard {
                conversation_id: String::new(),
                proposals: Vec::new(),
            },
            token: Some(ProposalToken(8)),
        };
        assert!(matches!(
            consolidation_frame("8", &payload, true),
            Some(ClientFrame::ConsolidationResolve {
                token: ProposalToken(8),
                accept: true
            })
        ));
        assert!(consolidation_frame("7", &payload, true).is_none());
        let local = ConsolidationPayload {
            token: None,
            ..payload
        };
        assert!(consolidation_frame("8", &local, true).is_none());
    }
}
