//! S-QUEUE: the Needs-you list (Story 21.6 A10, AC7).
//!
//! `Ctrl+X q` always opens the list of every waiting decision of this host, never
//! an item by itself: `Up`/`Down`/`j`/`k` select, `Enter` opens the selected item
//! as its own frame (replacing the list, so the item's Back returns to the list's
//! caller), `Esc`/`Ctrl+C` close. The list never decides and never reopens a
//! deferred item automatically: the initial selection is the oldest item the
//! user has not set aside, else the first row.

use super::{DecisionItem, DecisionKey};
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::presentation::{self, Surface};
use crate::adapters::tui::state::TuiState;

/// What the host must do after a key on the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListOutcome {
    Consumed,
    /// `Ctrl+Q`: run the host's S-QUIT guard.
    Quit,
    /// Switch to a proposal's origin, then open its decision.
    OpenTab {
        conversation_id: String,
        key: DecisionKey,
    },
}

/// Open the list over whatever is open. False when nothing waits.
pub fn open_list(state: &mut TuiState, _facts: &LogFacts) -> bool {
    super::reply::refresh(state);
    if state.decisions.is_empty() {
        return false;
    }
    let selected = state
        .decisions
        .items()
        .find(|item| !item.deferred)
        .or_else(|| state.decisions.items().next())
        .map(DecisionItem::key);
    state.decisions.list_selected = selected;
    presentation::reducer::open(state, Surface::DecisionQueue);
    state.needs_redraw = true;
    true
}

/// The selected row's index, defaulting to the first row.
pub(crate) fn selected_index(state: &TuiState) -> usize {
    state
        .decisions
        .list_selected
        .as_ref()
        .and_then(|key| {
            state
                .decisions
                .items()
                .position(|item| item.matches_key(key))
        })
        .unwrap_or(0)
}

/// Apply one key to the list.
pub fn handle_list_key(
    state: &mut TuiState,
    host: Host,
    key: Key,
    facts: &LogFacts,
) -> ListOutcome {
    state.needs_redraw = true;
    let command = match keymap::resolve(Context::DecisionQueue, host, &[key]) {
        Resolved::Hit(row) => match row.command {
            CommandId::Overlay(command) => Some(command),
            _ => None,
        },
        _ => None,
    };
    match command {
        Some(OverlayCmd::QuitRequest) => return ListOutcome::Quit,
        Some(OverlayCmd::QueueBack) => {
            presentation::reducer::close(state, Surface::DecisionQueue);
        }
        Some(OverlayCmd::QueueNext | OverlayCmd::QueuePrev) => {
            let last = state.decisions.len().saturating_sub(1);
            let at = selected_index(state).min(last);
            let moved = if command == Some(OverlayCmd::QueueNext) {
                (at + 1).min(last)
            } else {
                at.saturating_sub(1)
            };
            let next = state.decisions.items().nth(moved).map(DecisionItem::key);
            state.decisions.list_selected = next;
        }
        Some(OverlayCmd::QueueOpen) => {
            if let Some(outcome) = open_selected(state, facts) {
                return outcome;
            }
        }
        _ => {}
    }
    ListOutcome::Consumed
}

/// Open the selected row in place of the list, or ask the local host to switch tabs.
fn open_selected(state: &mut TuiState, facts: &LogFacts) -> Option<ListOutcome> {
    let item = state.decisions.items().nth(selected_index(state))?;
    if !item.actionable(facts) && !item.is_stale() {
        if item.kind == super::DecisionKind::TeamProposal {
            return item
                .conversation_id
                .clone()
                .map(|conversation_id| ListOutcome::OpenTab {
                    conversation_id,
                    key: item.key(),
                });
        }
        return None;
    }
    let key: DecisionKey = item.key();
    if key.kind == super::DecisionKind::ReplyArrived {
        super::reply::refresh(state);
        if !state.decisions.contains_key(&key) {
            return None;
        }
    }
    presentation::reducer::close(state, Surface::DecisionQueue);
    presentation::reducer::focus_decision(state, key);
    None
}
