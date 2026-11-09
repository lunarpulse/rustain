//! Pull-only projection of board reply counts and journaled thread reads.

use super::{
    Arrival, DecisionDraft, DecisionKind, DecisionSubject, NO_LONGER_PENDING, raise, withdraw_key,
};
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::state::TuiState;
use crate::domain::services::peer_text::sanitize_display_line;
use std::collections::HashMap;

/// Bound a scalar after consuming terminal escapes and collapsing line breaks.
pub(crate) fn display(text: &str) -> String {
    crate::domain::services::transparency::sanitize_disclosable(
        &sanitize_display_line(text),
        crate::domain::services::transparency::MAX_PEER_ID_BYTES,
    )
}

/// The original roster route and peer identity remain separate from display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyPayload {
    pub peer: String,
    pub alias: String,
    pub item: String,
    pub reply_count: u64,
}

#[derive(Debug, Default)]
pub(super) struct ReplyProjection {
    board: HashMap<(String, String), ReplyPayload>,
    observed: HashMap<(String, String), u64>,
    announced: HashMap<(String, String), u64>,
}

fn source(peer: &str, item: &str) -> String {
    format!("{}:{peer}{item}", peer.len())
}

/// Record one already-pulled board count; this function performs no I/O.
pub fn observe(state: &mut TuiState, peer: &str, alias: &str, item: &str, reply_count: u64) {
    let key = (peer.to_owned(), item.to_owned());
    state.decisions.replies.board.insert(
        key.clone(),
        ReplyPayload {
            peer: peer.to_owned(),
            alias: alias.to_owned(),
            item: item.to_owned(),
            reply_count,
        },
    );
    project_one(state, &key);
}

/// Consume the observations emitted by the local board pull, without further I/O.
pub fn observe_board(state: &mut TuiState, rows: Vec<(String, String, String, u64)>) {
    for (peer, alias, item, count) in rows {
        observe(state, &peer, &alias, &item, count);
    }
}

/// Replace the held journal projection with the observer's last successful fold.
pub fn journal_counts(state: &mut TuiState, counts: &HashMap<(String, String), u64>) {
    if state.decisions.replies.observed != *counts {
        state.decisions.replies.observed.clone_from(counts);
        refresh(state);
    }
}

/// Recompute solely from data already held by this host.
pub fn refresh(state: &mut TuiState) {
    let keys: Vec<_> = state.decisions.replies.board.keys().cloned().collect();
    for key in keys {
        project_one(state, &key);
    }
}

fn project_one(state: &mut TuiState, key: &(String, String)) {
    let Some(payload) = state.decisions.replies.board.get(key).cloned() else {
        return;
    };
    let seen = state
        .decisions
        .replies
        .observed
        .get(key)
        .copied()
        .unwrap_or(0);
    let decision_key = super::DecisionKey {
        kind: DecisionKind::ReplyArrived,
        source: source(&payload.peer, &payload.item),
        conversation_id: None,
    };
    if payload.reply_count <= seen {
        withdraw_key(state, &decision_key, NO_LONGER_PENDING);
        return;
    }
    let previous = state
        .decisions
        .replies
        .announced
        .get(key)
        .copied()
        .unwrap_or(0);
    if payload.reply_count > previous {
        state
            .decisions
            .replies
            .announced
            .insert(key.clone(), payload.reply_count);
        notifications::info(
            state,
            NoticeSource::Team,
            format!("{} replied", display(&payload.alias)),
        );
    } else if !state.decisions.contains_key(&decision_key) {
        // A dismissed count stays dismissed until the peer reports a newer count.
        return;
    }
    let summary = format!("{} · {}", display(&payload.alias), display(&payload.item));
    raise(
        state,
        DecisionDraft::new(DecisionKind::ReplyArrived, "")
            .source(decision_key.source)
            .summary(summary)
            .subject(DecisionSubject::ReplyArrived(Box::new(payload))),
        Arrival::Accumulate,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_reply_projection_uses_roster_alias_and_sanitizes_spans() {
        let mut state = TuiState::new(80, 24);
        observe(
            &mut state,
            "pin-principal",
            "roster\n\u{1b}[31mname",
            "ri_1\nforged",
            1,
        );
        let item = state.decisions.items().next().unwrap();
        let lines =
            crate::adapters::tui::widgets::decision_frame::body_lines_for_test(item, &state, 80);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("roster name"), "{text}");
        assert!(text.contains("pin-principal"), "{text}");
        assert!(!text.contains('\n') && !text.contains('\u{1b}'), "{text:?}");
        let notice = state.notifications.records().next().unwrap();
        assert_eq!(notice.message, "roster name replied");
        assert!(notice.action.is_none());
    }

    #[test]
    fn story21_6_reply_refresh_reconstructs_from_held_counts_without_a_poll() {
        let mut state = TuiState::new(80, 24);
        let key = ("principal".to_owned(), "ri_1".to_owned());
        state.decisions.replies.board.insert(
            key,
            ReplyPayload {
                peer: "principal".into(),
                alias: "Alice".into(),
                item: "ri_1".into(),
                reply_count: 1,
            },
        );
        assert!(state.decisions.is_empty());
        super::super::open_waiting(
            &mut state,
            &crate::adapters::tui::notification_log::LogFacts::default(),
        );
        assert_eq!(state.decisions.len(), 1);
        refresh(&mut state);
        assert_eq!(state.notifications.records().count(), 1);
    }

    #[test]
    fn story21_6_reply_local_raw_keys_require_plain_press() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut state = TuiState::new(80, 24);
        observe(&mut state, "principal", "Alice", "ri_1", 1);
        let key = state.decisions.items().next().unwrap().key();
        assert!(super::super::open(&mut state, &key));
        for code in [KeyCode::Enter, KeyCode::Char('x')] {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                for modifiers in [
                    KeyModifiers::NONE,
                    KeyModifiers::SHIFT,
                    KeyModifiers::ALT,
                    KeyModifiers::CONTROL,
                ] {
                    let event = Event::Key(KeyEvent::new_with_kind(code, modifiers, kind));
                    assert_eq!(
                        crate::adapters::tui::app::recipient_surface_ignores_raw_key(
                            &state, &event
                        ),
                        (kind != KeyEventKind::Press || modifiers != KeyModifiers::NONE)
                            // a plain Ctrl+X press is the leader chord, never ignored-as-modified
                            && !(code == KeyCode::Char('x')
                                && modifiers == KeyModifiers::CONTROL
                                && kind == KeyEventKind::Press)
                    );
                }
            }
        }
    }
}
