//! Keys of the focused decision frame (Story 21.6 A6, A7, A9).
//!
//! The registry's `Context::Decision` rows resolve the key; the item's kind maps
//! the command to its existing effect. Rail keys are inert for 500 ms after the
//! first paint (dropped, never replayed); `Esc`, `Ctrl+C`, `Ctrl+Q` and scrolling
//! never decide.

use super::{
    DecisionEffect, DecisionKey, DecisionKind, EscPolicy, Validity, current, request_carryover,
};
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::presentation::{self, Surface};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::decision_frame;
use crate::domain::events::DomainInputEvent;

/// Rows one `PgUp`/`PgDn` moves the body.
const SCROLL_PAGE: usize = 5;

/// Host-neutral key classes the pane understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKey {
    Char(char),
    Enter,
    Esc,
    CtrlC,
    CtrlQ,
    PageUp,
    PageDown,
    /// Any other user input: consumed, never leaked to the surface below.
    Other,
}

/// What the host must do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneOutcome {
    /// Swallowed (inert window, unmapped key, deferral, a new item raised).
    Consumed,
    /// `Ctrl+Q`: run the host's S-QUIT guard.
    Quit,
    /// Run this existing effect exactly once.
    Effect(DecisionEffect),
}

impl PaneKey {
    fn key(self) -> Key {
        match self {
            PaneKey::Char(c) => Key::Char(c),
            PaneKey::Enter => Key::Enter,
            PaneKey::Esc => Key::Esc,
            PaneKey::CtrlC => Key::Ctrl('c'),
            PaneKey::CtrlQ => Key::Ctrl('q'),
            PaneKey::PageUp => Key::PageUp,
            PaneKey::PageDown => Key::PageDown,
            PaneKey::Other => Key::Other,
        }
    }
}

/// Apply one key to the focused pane. Without a decidable focused item it
/// consumes the key and heals the focus.
pub fn handle_key(state: &mut TuiState, key: PaneKey, facts: &LogFacts) -> PaneOutcome {
    execute(state, key.key(), facts)
}

pub(crate) fn clear_feedback(state: &mut TuiState) {
    if state.decisions.feedback.take().is_some() {
        state.needs_redraw = true;
    }
}

/// Apply a local domain event. User keys, paste and mouse are consumed;
/// resize/focus events (and an unfocused pane) return `None` so the caller
/// handles them normally.
pub fn handle_domain_event(
    state: &mut TuiState,
    event: &DomainInputEvent,
    facts: &LogFacts,
) -> Option<PaneOutcome> {
    if state
        .modal_stack
        .top()
        .is_none_or(|frame| frame.surface != Surface::Decision)
    {
        return None;
    }
    if current(state, facts).is_none() {
        // The focused item left (tab switch, condition cleared): heal and let
        // the key reach whoever owns it now.
        presentation::reducer::close(state, Surface::Decision);
        return None;
    }
    if matches!(
        event,
        DomainInputEvent::Resize(..) | DomainInputEvent::FocusGained | DomainInputEvent::FocusLost
    ) {
        return None;
    }
    let key = Key::from_domain(event).unwrap_or(Key::Other);
    Some(execute(state, key, facts))
}

/// Resolve the key in `Decision` (letters fold case) and run its command
/// against the focused item's kind; a command the kind does not offer is swallowed.
fn execute(state: &mut TuiState, key: Key, facts: &LogFacts) -> PaneOutcome {
    state.needs_redraw = true;
    let command = match keymap::resolve(Context::Decision, Host::Local, &[key]) {
        Resolved::Hit(row) => match row.command {
            CommandId::Overlay(command) => Some(command),
            _ => None,
        },
        _ => None,
    };
    if command == Some(OverlayCmd::PaneQuit) {
        return PaneOutcome::Quit;
    }
    let Some(item) = current(state, facts) else {
        presentation::reducer::close(state, Surface::Decision);
        return PaneOutcome::Consumed;
    };
    let item_key = item.key();
    if matches!(item.validity, Validity::Stale(_)) {
        // Its source settled elsewhere: no mutation key acts, Close removes it (A7).
        if matches!(command, Some(OverlayCmd::PaneEsc | OverlayCmd::PaneBack)) {
            close_stale(state, &item_key);
        }
        return PaneOutcome::Consumed;
    }
    match command {
        Some(OverlayCmd::PaneBack) => {
            defer_current(state, &item_key);
            return PaneOutcome::Consumed;
        }
        // Setting an item aside decides nothing, so it is never inert (A6): only the
        // resolving Esc policies (Recovery Continue, Delete Cancel) wait for the window.
        Some(OverlayCmd::PaneEsc) if item.kind.esc() == EscPolicy::Defer => {
            defer_current(state, &item_key);
            return PaneOutcome::Consumed;
        }
        Some(OverlayCmd::PaneScrollUp) => {
            scroll(state, &item_key, false);
            return PaneOutcome::Consumed;
        }
        Some(OverlayCmd::PaneScrollDown) => {
            scroll(state, &item_key, true);
            return PaneOutcome::Consumed;
        }
        _ => {}
    }
    if state.decisions.inert(state) {
        // Dropped, never replayed once the window passes.
        return PaneOutcome::Consumed;
    }
    let Some(command) = command else {
        return PaneOutcome::Consumed;
    };
    let kind = item.kind;
    let conversation_id = item.conversation_id.clone();
    let effect = match (kind, command) {
        (DecisionKind::Recovery, OverlayCmd::PaneEnter) => Some(DecisionEffect::RecoveryContinue),
        (DecisionKind::Recovery, OverlayCmd::PaneNo) => Some(DecisionEffect::RecoveryNewSession),
        (DecisionKind::Carryover, OverlayCmd::PaneYes) => Some(DecisionEffect::CarryoverAccept),
        (DecisionKind::Carryover, OverlayCmd::PaneNo) => Some(DecisionEffect::CarryoverDecline),
        (DecisionKind::Budget, OverlayCmd::PaneYes) => Some(DecisionEffect::BudgetContinue),
        (DecisionKind::Budget, OverlayCmd::PaneSwitch) => Some(DecisionEffect::BudgetSwitchCheaper),
        (DecisionKind::Budget, OverlayCmd::PanePause) => Some(DecisionEffect::BudgetPause),
        (DecisionKind::ContextWarning, OverlayCmd::PaneCompact) => {
            Some(DecisionEffect::ContextCompact)
        }
        (DecisionKind::LargeImage, OverlayCmd::PaneYes) => Some(DecisionEffect::AttachLargeImage),
        (DecisionKind::LargeImage, OverlayCmd::PaneNo) => Some(DecisionEffect::CancelLargeImage),
        (DecisionKind::DeleteConversations, OverlayCmd::PaneYes) => {
            Some(DecisionEffect::ConfirmDelete)
        }
        (DecisionKind::DeleteConversations, OverlayCmd::PaneNo) => {
            Some(DecisionEffect::CancelDelete)
        }
        (kind, OverlayCmd::PaneEsc) => match kind.esc() {
            EscPolicy::Resolve(effect) => Some(effect),
            EscPolicy::Defer => {
                defer_current(state, &item_key);
                return PaneOutcome::Consumed;
            }
        },
        _ => None,
    };
    if let Some(effect) = effect {
        return resolve(state, facts, &item_key, effect);
    }
    if (kind, command) == (DecisionKind::ContextWarning, OverlayCmd::PaneNo) {
        // "Start fresh" asks about the carried summary first (RU-21: never `c`).
        state.decisions.remove(&item_key);
        request_carryover(state, conversation_id);
    }
    PaneOutcome::Consumed
}

/// Run the item's effect exactly once: the item leaves the queue first, its
/// payload waits for the effect arm, and the frame pops (A8, A9).
fn resolve(
    state: &mut TuiState,
    facts: &LogFacts,
    key: &DecisionKey,
    effect: DecisionEffect,
) -> PaneOutcome {
    if let keymap::Availability::Unavailable(remedy) =
        keymap::table::pane_effect_availability(effect)
    {
        state.decisions.feedback = Some(remedy);
        return PaneOutcome::Consumed;
    }
    if let Some(item) = state.decisions.remove(key) {
        state.decisions.resolved = Some(item.subject);
    }
    after_answer(state, facts, key);
    PaneOutcome::Effect(effect)
}

/// AD-2106: popping the frame restores exactly what it interrupted, and a waiting
/// item is never opened by surprise. The one legacy exception (A9): a user who
/// answered from the base (nothing beneath) and has a not-deferred item of the
/// same kind waiting is mid-sequence, so that item opens at once.
fn after_answer(state: &mut TuiState, facts: &LogFacts, answered: &DecisionKey) {
    let nothing_beneath = state.modal_stack.len() <= 1;
    let next = nothing_beneath
        .then(|| state.decisions.next_of_kind(answered.kind, facts))
        .flatten();
    match next {
        Some(key) => presentation::reducer::focus_decision(state, key),
        None => presentation::reducer::close(state, Surface::Decision),
    }
}

fn scroll(state: &mut TuiState, key: &DecisionKey, forward: bool) {
    let Some(item) = state.decisions.item(key) else {
        return;
    };
    let rows = decision_frame::body_rows(item, state.terminal_width);
    // A re-wrap (resize) may have shortened the body since the offset was kept.
    let at = state.decisions.scroll.min(rows.saturating_sub(1));
    state.decisions.scroll = if forward {
        (at + SCROLL_PAGE).min(rows.saturating_sub(1))
    } else {
        at.saturating_sub(SCROLL_PAGE)
    };
}

fn defer_current(state: &mut TuiState, key: &DecisionKey) {
    if let Some(item) = state.decisions.item_mut(key) {
        item.deferred = true;
    }
    state.decisions.last_defer_at = Some(state.notifications.now());
    presentation::reducer::close(state, Surface::Decision);
}

/// Close on a stale item removes it: nothing was decided, nothing is deferred.
fn close_stale(state: &mut TuiState, key: &DecisionKey) {
    state.decisions.remove(key);
    presentation::reducer::close(state, Surface::Decision);
}
