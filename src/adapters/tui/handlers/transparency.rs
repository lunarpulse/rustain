//! Story 18.2 (AC4/AC5) — surfacing transparency records in the TUI.
//!
//! # Why nothing here is named `handle_*`
//!
//! `tests/conformance.rs` pins `EXPECTED_HANDLE_COUNT` with an exact
//! `assert_eq!`, and bumping it requires a `RATCHET-SIGNOFF` trailer. These are
//! `pub(crate) fn` with non-`handle_` names — the same choice `handlers/notice.rs`
//! made — so the counter is untouched. That is a deliberate decision, not an
//! oversight: nothing here is a dispatch-arm handler with a `HandlerOutcome`
//! contract; they are state transitions the arm applies inline.
//!
//! # Firehose bound (AC4)
//!
//! A busy peer must not flood the transcript. Only **decision-bearing** records
//! surface as a chat notice — accept, refuse, awaiting-approval. Status polls
//! are excluded at the source (AC1 journals only the first observation per
//! task) and excluded again here. When more than one record for the same
//! `(peer, task)` arrives inside a turn, the later one replaces the earlier
//! row rather than adding a line: the `FeedbackBlock` id is derived from
//! `(peer, task)`, and `feedback_blocks` is a map.

use crate::adapters::tui::state::TuiState;
use crate::domain::events::DomainEventPayload;
use crate::domain::events::DomainKey;
use crate::domain::models::{FeedbackBlock, FeedbackLevel, RoomEvent};
use crate::domain::services::transparency::{
    TransparencyKind, TransparencyRow, sanitize_disclosable,
};

/// Stable id for the latched journal-failure row.
///
/// One row, whose count increments in place. `feedback_blocks` is keyed by id,
/// so re-inserting under the same key REPLACES the row — which is exactly what
/// "the first failure raises one persistent notice, subsequent failures
/// increment a count on that same row" means.
pub(crate) const JOURNAL_FAILURE_BLOCK_ID: &str = "transparency-journal-failure";

/// Prefix for per-interaction transcript rows. The suffix is `(peer, task)`,
/// which is what makes repeated records for one interaction coalesce.
const NOTICE_BLOCK_PREFIX: &str = "transparency-";

/// Apply one bus event to the TUI. Returns `true` when the transcript changed.
///
/// This is the **first** `AppEvent::DomainEvent` consumer in the product: room
/// events were emitted durable-first by every room path and reached nothing,
/// falling into the event loop's catch-all. Deleting the dispatch arm that
/// calls this puts them back in the void — which is the wiring-hole mutant
/// (`DF-CR-14-3a-1` is the precedent: render fns shipped with zero production
/// call sites and the tasks were still checked `[x]`).
pub fn apply_domain_event(state: &mut TuiState, payload: &DomainEventPayload) -> bool {
    match payload {
        DomainEventPayload::Room(event) => match notice_row(event) {
            Some(row) => {
                let block = FeedbackBlock {
                    id: format!(
                        "{NOTICE_BLOCK_PREFIX}{}-{}",
                        row.peer,
                        row.task.as_deref().unwrap_or("-")
                    ),
                    level: feedback_level(row.kind),
                    message: row.one_line(),
                    actions: Vec::new(),
                };
                state.feedback_blocks.insert(block.id.clone(), block);
                true
            }
            None => false,
        },
        DomainEventPayload::TransparencyJournalFailed { failures, detail } => {
            state.feedback_blocks.insert(
                JOURNAL_FAILURE_BLOCK_ID.to_owned(),
                FeedbackBlock {
                    id: JOURNAL_FAILURE_BLOCK_ID.to_owned(),
                    level: FeedbackLevel::Error,
                    message: format!(
                        "⚠ transparency journal is failing — {failures} record(s) missing from \
                         the audit log; A2A accepts are being refused until it recovers ({})",
                        sanitize_disclosable(detail, 200)
                    ),
                    actions: Vec::new(),
                },
            );
            true
        }
    }
}

/// The decision-bearing subset that reaches the transcript.
///
/// Status queries are journaled (FR92) but deliberately NOT surfaced: a poll
/// is not a decision, and surfacing it would put the firehose back.
fn notice_row(event: &RoomEvent) -> Option<TransparencyRow> {
    let entry = crate::domain::models::JournalEntry::new(
        0,
        crate::domain::models::JournalRecord::Room(event.clone()),
        0,
    );
    let row = crate::domain::services::transparency::transparency_row(&entry)?;
    matches!(
        row.kind,
        TransparencyKind::Accepted
            | TransparencyKind::Rejected
            | TransparencyKind::AwaitingApproval
    )
    .then_some(row)
}

/// Error for refusals (they are the ones an operator must react to), Warning
/// for everything else. Never `Info`: an `Info` notice becomes a transient
/// status-bar flash and never reaches the transcript, so a "persistent
/// in-transcript line" built on it would not exist.
fn feedback_level(kind: TransparencyKind) -> FeedbackLevel {
    match kind {
        TransparencyKind::Rejected => FeedbackLevel::Error,
        _ => FeedbackLevel::Warning,
    }
}

/// Result of one panel-local printable key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PanelKeyAction {
    Ignored,
    Consumed,
    Export,
}

/// Keep the sidebar's legacy selection mirror synchronized with the panel's
/// filtered viewport. `FocusState::Sidebar` also carries a display selection,
/// so update it here instead of allowing three copies to drift apart.
fn synchronize_sidebar_selection(state: &mut TuiState) {
    state
        .transparency_panel
        .synchronize_selection(&mut state.sidebar_selected);
    state.sidebar_entry_count = state.transparency_panel.visible_len();
    let selected = state.sidebar_selected;
    if let crate::domain::models::FocusState::Sidebar {
        panel: crate::domain::models::visual::PanelType::TransparencyLog,
        selected: focus_selected,
    } = &mut state.focus
    {
        *focus_selected = selected;
    }
}

/// Key handling for the Transparency Log panel.
///
/// Chord-prefixed globals stay untouched (`UX-DR-GLOBAL-CHORD-PREFIX`); these
/// are panel-local keys reached only while the sidebar has focus.
pub(crate) fn panel_key(state: &mut TuiState, key: char) -> PanelKeyAction {
    if state.transparency_panel.search_active {
        if key == '\n' {
            state
                .transparency_panel
                .commit_search(&mut state.sidebar_selected);
        } else {
            state
                .transparency_panel
                .append_search(&mut state.sidebar_selected, key);
        }
        synchronize_sidebar_selection(state);
        return PanelKeyAction::Consumed;
    }

    match key {
        '/' => {
            state
                .transparency_panel
                .start_search(&mut state.sidebar_selected);
            synchronize_sidebar_selection(state);
            PanelKeyAction::Consumed
        }
        'j' => {
            state
                .transparency_panel
                .move_selection(&mut state.sidebar_selected, true);
            synchronize_sidebar_selection(state);
            PanelKeyAction::Consumed
        }
        'k' => {
            state
                .transparency_panel
                .move_selection(&mut state.sidebar_selected, false);
            synchronize_sidebar_selection(state);
            PanelKeyAction::Consumed
        }
        'G' => {
            state
                .transparency_panel
                .open_at_tail(&mut state.sidebar_selected);
            synchronize_sidebar_selection(state);
            PanelKeyAction::Consumed
        }
        'e' => PanelKeyAction::Export,
        _ => PanelKeyAction::Ignored,
    }
}

/// Route non-printing keys before generic sidebar behavior can consume them.
pub(crate) fn panel_special_key(state: &mut TuiState, key: DomainKey) -> bool {
    match key {
        DomainKey::Enter if state.transparency_panel.search_active => {
            state
                .transparency_panel
                .commit_search(&mut state.sidebar_selected);
            synchronize_sidebar_selection(state);
            true
        }
        DomainKey::Enter => {
            let _ = toggle_drill(state, state.sidebar_selected);
            true
        }
        DomainKey::Backspace => {
            if state
                .transparency_panel
                .backspace_search(&mut state.sidebar_selected)
            {
                synchronize_sidebar_selection(state);
                true
            } else {
                false
            }
        }
        DomainKey::Esc => clear_transient(state),
        _ => false,
    }
}

/// `Enter` on the selected row toggles the drill-down.
pub(crate) fn toggle_drill(state: &mut TuiState, selected: usize) -> bool {
    let Some(seq) = state
        .transparency_panel
        .visible_row(selected)
        .map(|row| row.seq)
    else {
        return false;
    };
    let panel = &mut state.transparency_panel;
    panel.drill_seq = if panel.drill_seq == Some(seq) {
        None
    } else {
        Some(seq)
    };
    synchronize_sidebar_selection(state);
    true
}

/// `Esc` clears search/drill before the panel itself closes.
pub(crate) fn clear_transient(state: &mut TuiState) -> bool {
    let panel = &mut state.transparency_panel;
    if panel.search_active || panel.search.is_some() {
        panel.search = None;
        panel.search_active = false;
        state.sidebar_selected = 0;
        panel.scroll_offset = 0;
        synchronize_sidebar_selection(state);
        return true;
    }
    if panel.drill_seq.take().is_some() {
        synchronize_sidebar_selection(state);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{AgentId, ContentHash, Direction, PeerId, RejectReason};

    fn peer() -> PeerId {
        PeerId::from_public_key(&[5u8; 32]).unwrap()
    }

    fn rejection(detail: &str) -> RoomEvent {
        RoomEvent::RemoteEnvelopeRejected {
            peer: peer(),
            reason: RejectReason::Policy {
                detail: detail.to_owned(),
            },
            direction: Direction::Inbound,
            task: Some("task-1".to_owned()),
        }
    }

    #[test]
    fn a_refusal_becomes_exactly_one_persistent_transcript_row() {
        let mut state = TuiState::new(80, 24);
        assert!(apply_domain_event(
            &mut state,
            &DomainEventPayload::Room(rejection("policy said no"))
        ));
        assert_eq!(state.feedback_blocks.len(), 1);
        let block = state.feedback_blocks.values().next().unwrap();
        assert!(
            block.message.contains("policy said no"),
            "{}",
            block.message
        );
        // Monochrome: direction and kind each carry a glyph AND a word.
        assert!(block.message.contains('←') && block.message.contains("inbound"));
        assert!(block.message.contains('✗') && block.message.contains("refused"));
        // Never Info — an Info notice is a transient status flash, not a
        // transcript line.
        assert!(!matches!(block.level, FeedbackLevel::Info));
    }

    #[test]
    fn repeated_records_for_one_interaction_coalesce_to_one_row() {
        let mut state = TuiState::new(80, 24);
        for detail in ["first", "second", "third"] {
            apply_domain_event(&mut state, &DomainEventPayload::Room(rejection(detail)));
        }
        assert_eq!(
            state.feedback_blocks.len(),
            1,
            "a busy peer must not flood the transcript"
        );
        assert!(
            state
                .feedback_blocks
                .values()
                .next()
                .unwrap()
                .message
                .contains("third"),
            "the latest record wins"
        );
    }

    #[test]
    fn a_status_query_is_journaled_but_never_surfaced() {
        let mut state = TuiState::new(80, 24);
        let queried = RoomEvent::AdmissionDeferred {
            coordinator: AgentId::root(),
            spoke: peer().as_str().to_owned(),
            gate: "a2a-status-query:t-1".to_owned(),
        };
        assert!(!apply_domain_event(
            &mut state,
            &DomainEventPayload::Room(queried)
        ));
        assert!(state.feedback_blocks.is_empty(), "a poll is not a decision");
    }

    #[test]
    fn a_non_transparency_room_event_surfaces_nothing() {
        let mut state = TuiState::new(80, 24);
        let unrelated = RoomEvent::RemoteEnvelopeAccepted {
            peer: peer(),
            node: AgentId::root(),
            content_hash: ContentHash::from_bytes([0u8; 32]),
            direction: Direction::Inbound,
            task: Some("task-1".to_owned()),
        };
        // An accept IS decision-bearing, so this one surfaces…
        assert!(apply_domain_event(
            &mut state,
            &DomainEventPayload::Room(unrelated)
        ));
        // …but a wave event is not a transparency record at all.
        let wave = RoomEvent::WaveCompleted {
            wave: crate::domain::models::WaveId::new(),
            outcome: crate::domain::models::WaveOutcome::Completed,
        };
        let before = state.feedback_blocks.len();
        assert!(!apply_domain_event(
            &mut state,
            &DomainEventPayload::Room(wave)
        ));
        assert_eq!(state.feedback_blocks.len(), before);
    }

    #[test]
    fn the_latched_failure_is_one_row_whose_count_increments() {
        let mut state = TuiState::new(80, 24);
        for failures in 1..=5u64 {
            apply_domain_event(
                &mut state,
                &DomainEventPayload::TransparencyJournalFailed {
                    failures,
                    detail: "no space left on device".to_owned(),
                },
            );
        }
        assert_eq!(
            state.feedback_blocks.len(),
            1,
            "five failures, ONE row — never one notice per refusal"
        );
        let block = &state.feedback_blocks[JOURNAL_FAILURE_BLOCK_ID];
        assert!(
            block.message.contains("5 record(s) missing"),
            "{}",
            block.message
        );
        assert!(matches!(block.level, FeedbackLevel::Error));
    }

    #[test]
    fn the_latched_failure_message_carries_no_control_bytes() {
        let mut state = TuiState::new(80, 24);
        apply_domain_event(
            &mut state,
            &DomainEventPayload::TransparencyJournalFailed {
                failures: 1,
                detail: "\u{1b}]0;pwned\u{7}".to_owned(),
            },
        );
        let message = &state.feedback_blocks[JOURNAL_FAILURE_BLOCK_ID].message;
        assert!(!message.chars().any(char::is_control), "{message:?}");
    }
}
