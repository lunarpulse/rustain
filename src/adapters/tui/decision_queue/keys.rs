//! Keys of the focused decision frame (Story 21.6 A6, A7, A9, A12).
//!
//! The registry rows of the focused item's context (a kind's own rows, then its
//! `Context::Decision` parent) resolve the key; the item's kind maps the command to
//! its existing effect. Rail keys are inert for 500 ms after the first paint
//! (dropped, never replayed); `Esc`, `Ctrl+C`, `Ctrl+Q` and scrolling never decide.
//! The Review reader and the Deny + feedback field are sub-contexts of the frame:
//! `Esc` / `Ctrl+C` leave them before they would defer the item.

use super::compose::{self, FieldKey};
use super::{
    DecisionEffect, DecisionItem, DecisionKey, DecisionKind, DecisionSubject, EscPolicy,
    PermissionPayload, Responder, current, request_carryover,
};
use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::table_permission::PermissionCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::presentation::text_field::TextField;
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

/// The commands a decision frame understands, whichever registry row named them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Quit,
    Back,
    Esc,
    ScrollUp,
    ScrollDown,
    Enter,
    Yes,
    No,
    Switch,
    Pause,
    Compact,
    Allow,
    Session,
    Always,
    Deny,
    Feedback,
    Review,
    ConsentOnce,
    ConsentAlways,
    ConsentDecline,
}

fn command_of(id: CommandId) -> Option<Cmd> {
    Some(match id {
        CommandId::Overlay(OverlayCmd::PaneQuit) => Cmd::Quit,
        CommandId::Overlay(OverlayCmd::PaneBack) => Cmd::Back,
        CommandId::Overlay(OverlayCmd::PaneEsc) => Cmd::Esc,
        CommandId::Overlay(OverlayCmd::PaneScrollUp) => Cmd::ScrollUp,
        CommandId::Overlay(OverlayCmd::PaneScrollDown) => Cmd::ScrollDown,
        CommandId::Overlay(OverlayCmd::PaneEnter) => Cmd::Enter,
        CommandId::Overlay(OverlayCmd::PaneYes) => Cmd::Yes,
        CommandId::Overlay(OverlayCmd::PaneNo) => Cmd::No,
        CommandId::Overlay(OverlayCmd::PaneSwitch) => Cmd::Switch,
        CommandId::Overlay(OverlayCmd::PanePause) => Cmd::Pause,
        CommandId::Overlay(OverlayCmd::PaneCompact) => Cmd::Compact,
        CommandId::Permission(PermissionCmd::AllowOnce) => Cmd::Allow,
        CommandId::Permission(PermissionCmd::AllowSession) => Cmd::Session,
        CommandId::Permission(PermissionCmd::AllowAlways) => Cmd::Always,
        CommandId::Permission(PermissionCmd::Deny) => Cmd::Deny,
        CommandId::Permission(PermissionCmd::DenyWithFeedback) => Cmd::Feedback,
        CommandId::Permission(PermissionCmd::Review) => Cmd::Review,
        CommandId::Card(CardCmd::Yes) => Cmd::ConsentOnce,
        CommandId::Card(CardCmd::Always) => Cmd::ConsentAlways,
        CommandId::Card(CardCmd::No) => Cmd::ConsentDecline,
        _ => return None,
    })
}

/// Apply one key to the focused pane. Without a decidable focused item it
/// consumes the key and heals the focus.
pub fn handle_key(state: &mut TuiState, key: PaneKey, facts: &LogFacts) -> PaneOutcome {
    execute(state, key.key(), facts, Host::Local)
}

pub(crate) fn clear_feedback(state: &mut TuiState) {
    if state.decisions.feedback.take().is_some() {
        state.needs_redraw = true;
    }
}

/// Apply a domain event. User keys, paste and mouse are consumed;
/// resize/focus events (and an unfocused pane) return `None` so the caller
/// handles them normally.
pub fn handle_domain_event(
    state: &mut TuiState,
    event: &DomainInputEvent,
    facts: &LogFacts,
    host: Host,
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
    Some(execute(state, key, facts, host))
}

/// The Deny + feedback field of a permission item, when it is open.
fn composing(item: &DecisionItem) -> bool {
    matches!(&item.subject, DecisionSubject::Permission(payload) if payload.feedback.is_some())
}

/// The registry context that resolves this item's keys now.
pub(crate) fn context_of(item: &DecisionItem) -> Context {
    if item.is_stale() {
        Context::Decision
    } else if composing(item) {
        Context::DecisionCompose
    } else {
        item.kind.context()
    }
}

fn payload_mut<'a>(
    state: &'a mut TuiState,
    key: &DecisionKey,
) -> Option<&'a mut PermissionPayload> {
    match &mut state.decisions.item_mut(key)?.subject {
        DecisionSubject::Permission(payload) => Some(payload),
        _ => None,
    }
}

/// Edit the open field of the focused permission with `edit` (a paste, a key).
/// False when no field is open.
pub(crate) fn edit_field(state: &mut TuiState, edit: impl FnOnce(&mut TextField) -> bool) -> bool {
    let Some(key) = state
        .modal_stack
        .all_frames()
        .find(|frame| frame.surface == Surface::Decision)
        .and_then(|frame| frame.decision.clone())
    else {
        return false;
    };
    let Some(field) = payload_mut(state, &key).and_then(|payload| payload.feedback.as_mut()) else {
        return false;
    };
    edit(field);
    state.needs_redraw = true;
    true
}

/// Paste into the focused frame's open field.
pub(crate) fn paste(state: &mut TuiState, text: &str) -> bool {
    edit_field(state, |field| compose::paste(field, text))
}

/// Resolve the key in the focused item's context (letters fold case in `Decision`)
/// and run its command against the item's kind; a command the kind does not offer is
/// swallowed.
fn execute(state: &mut TuiState, key: Key, facts: &LogFacts, host: Host) -> PaneOutcome {
    state.needs_redraw = true;
    let Some(item) = current(state, facts) else {
        presentation::reducer::close(state, Surface::Decision);
        let quit = matches!(
            keymap::resolve(Context::Decision, host, &[key]),
            Resolved::Hit(row) if command_of(row.command) == Some(Cmd::Quit)
        );
        return if quit {
            PaneOutcome::Quit
        } else {
            PaneOutcome::Consumed
        };
    };
    let item_key = item.key();
    let kind = item.kind;
    let stale = item.is_stale();
    let in_field = composing(item);
    let reviewing = matches!(&item.subject, DecisionSubject::Permission(p) if p.reviewing);
    let conversation_id = item.conversation_id.clone();
    let context = context_of(item);
    let command = match keymap::resolve(context, host, &[key]) {
        Resolved::Hit(row) => command_of(row.command),
        _ => None,
    };
    if command == Some(Cmd::Quit) {
        return PaneOutcome::Quit;
    }
    if stale {
        // Its source settled elsewhere: no mutation key acts, Close removes it (A7).
        if matches!(command, Some(Cmd::Esc | Cmd::Back)) {
            close_stale(state, &item_key);
        }
        return PaneOutcome::Consumed;
    }
    if in_field {
        return field_key(state, facts, host, &item_key, key);
    }
    match command {
        Some(Cmd::Back | Cmd::Esc) if reviewing => {
            leave_review(state, &item_key);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::Back) => {
            defer_current(state, &item_key);
            return PaneOutcome::Consumed;
        }
        // Setting an item aside decides nothing, so it is never inert (A6): only the
        // resolving Esc policies (Recovery Continue, Delete Cancel) wait for the window.
        Some(Cmd::Esc) if kind.esc() == EscPolicy::Defer => {
            defer_current(state, &item_key);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::ScrollUp) => {
            scroll(state, &item_key, false);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::ScrollDown) => {
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
    let effect = match (kind, command) {
        (DecisionKind::Recovery, Cmd::Enter) => Some(DecisionEffect::RecoveryContinue),
        (DecisionKind::Recovery, Cmd::No) => Some(DecisionEffect::RecoveryNewSession),
        (DecisionKind::Carryover, Cmd::Yes) => Some(DecisionEffect::CarryoverAccept),
        (DecisionKind::Carryover, Cmd::No) => Some(DecisionEffect::CarryoverDecline),
        (DecisionKind::Budget, Cmd::Yes) => Some(DecisionEffect::BudgetContinue),
        (DecisionKind::Budget, Cmd::Switch) => Some(DecisionEffect::BudgetSwitchCheaper),
        (DecisionKind::Budget, Cmd::Pause) => Some(DecisionEffect::BudgetPause),
        (DecisionKind::ContextWarning, Cmd::Compact) => Some(DecisionEffect::ContextCompact),
        (DecisionKind::LargeImage, Cmd::Yes) => Some(DecisionEffect::AttachLargeImage),
        (DecisionKind::LargeImage, Cmd::No) => Some(DecisionEffect::CancelLargeImage),
        (DecisionKind::DeleteConversations, Cmd::Yes) => Some(DecisionEffect::ConfirmDelete),
        (DecisionKind::DeleteConversations, Cmd::No) => Some(DecisionEffect::CancelDelete),
        (DecisionKind::Permission, Cmd::Allow) => Some(DecisionEffect::PermissionAllowOnce),
        (DecisionKind::Permission, Cmd::Session) => Some(DecisionEffect::PermissionAllowSession),
        (DecisionKind::Permission, Cmd::Always) => Some(DecisionEffect::PermissionAllowAlways),
        (DecisionKind::Permission, Cmd::Deny) => Some(DecisionEffect::PermissionDeny),
        (DecisionKind::Consent, Cmd::ConsentOnce) => Some(DecisionEffect::ConsentOnce),
        (DecisionKind::Consent, Cmd::ConsentAlways) => Some(DecisionEffect::ConsentAlways),
        (DecisionKind::Consent, Cmd::ConsentDecline) => Some(DecisionEffect::ConsentDecline),
        (kind, Cmd::Esc) => match kind.esc() {
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
    match (kind, command) {
        (DecisionKind::ContextWarning, Cmd::No) => {
            // "Start fresh" asks about the carried summary first (RU-21: never `c`).
            state.decisions.remove(&item_key);
            request_carryover(state, conversation_id);
        }
        (DecisionKind::Permission, Cmd::Feedback) => {
            if let Some(payload) = payload_mut(state, &item_key) {
                payload.feedback = Some(TextField::default());
                state.decisions.scroll = 0;
            }
        }
        (DecisionKind::Permission, Cmd::Review) => {
            if let Some(payload) = payload_mut(state, &item_key)
                && payload.ready()
            {
                payload.reviewing = true;
                state.decisions.scroll = 0;
            }
        }
        _ => {}
    }
    PaneOutcome::Consumed
}

/// One key while the Deny + feedback field is open: it edits, sends or leaves.
fn field_key(
    state: &mut TuiState,
    facts: &LogFacts,
    host: Host,
    item_key: &DecisionKey,
    key: Key,
) -> PaneOutcome {
    let Some(field) = payload_mut(state, item_key).and_then(|payload| payload.feedback.as_mut())
    else {
        return PaneOutcome::Consumed;
    };
    match compose::apply(field, key, host) {
        FieldKey::Send => resolve(
            state,
            facts,
            item_key,
            DecisionEffect::PermissionDenyFeedback,
        ),
        FieldKey::Back => {
            if let Some(payload) = payload_mut(state, item_key) {
                payload.feedback = None;
            }
            PaneOutcome::Consumed
        }
        FieldKey::Quit => PaneOutcome::Quit,
        FieldKey::Edited | FieldKey::Swallowed => PaneOutcome::Consumed,
    }
}

/// Run the item's effect exactly once. A local item leaves the queue first, its
/// payload waits for the effect arm and the frame pops; an attached item stays
/// queued (and its frame open) until the host's send succeeds (A8, A9).
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
    let attached = state
        .decisions
        .item(key)
        .is_some_and(|item| item.responder == Responder::Attached);
    if attached {
        state.decisions.sending = Some((key.clone(), effect));
        return PaneOutcome::Effect(effect);
    }
    if let Some(item) = state.decisions.remove(key) {
        state.decisions.resolved = Some(item.subject);
    }
    after_answer(state, facts, key);
    PaneOutcome::Effect(effect)
}

/// The attached host sent the effect of the item `key`: only now does the item
/// leave the queue and its frame pop (the `ApprovalResponse` retire-after-send rule).
pub fn settle_sent(state: &mut TuiState, facts: &LogFacts, key: &DecisionKey) {
    state.decisions.remove(key);
    after_answer(state, facts, key);
    state.needs_redraw = true;
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

fn leave_review(state: &mut TuiState, key: &DecisionKey) {
    if let Some(payload) = payload_mut(state, key) {
        payload.reviewing = false;
    }
    state.decisions.scroll = 0;
}

fn scroll(state: &mut TuiState, key: &DecisionKey, forward: bool) {
    let Some(item) = state.decisions.item(key) else {
        return;
    };
    let rows = decision_frame::body_rows(item, state);
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
