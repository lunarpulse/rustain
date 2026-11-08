//! Keys of the focused decision frame (Story 21.6 A6, A7, A9, A12).
//!
//! The registry rows of the focused item's context (a kind's own rows, then its
//! `Context::Decision` parent) resolve the key; the item's kind maps the command to
//! its existing effect. Rail keys are inert for 500 ms after the first paint
//! (dropped, never replayed); `Esc`, `Ctrl+C`, `Ctrl+Q`, `c` (copy) and scrolling never
//! decide. The Review reader, the Deny + feedback field, the question's answer field and
//! the skill file reader are sub-contexts of the frame: `Esc` / `Ctrl+C` leave them
//! before they would defer the item.

use super::compose::{self, FieldKey};
use super::question::ANSWER_MAX_BYTES;
use super::skill_trust;
use super::{
    DecisionEffect, DecisionItem, DecisionKey, DecisionKind, DecisionSubject, EscPolicy,
    PermissionPayload, Responder, current, focused_key, request_carryover,
};
use crate::adapters::tui::handlers::clipboard::notify_failure;
use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::table_permission::PermissionCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::presentation::text_field::TextField;
use crate::adapters::tui::presentation::{self, Surface};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::decision_frame::{self, CopySource};
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
    CtrlD,
    CtrlQ,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// Any other user input: consumed, never leaked to the surface below.
    Other,
}

/// What the host must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOutcome {
    /// Swallowed (inert window, unmapped key, deferral, a new item raised).
    Consumed,
    /// `Ctrl+Q`: run the host's S-QUIT guard.
    Quit,
    /// Run this existing effect exactly once.
    Effect(DecisionEffect),
    /// `c`: copy this text (the item's original, bounded to the copy limit) with the
    /// host's clipboard and its one copy notice.
    Copy(String),
}

impl PaneKey {
    fn key(self) -> Key {
        match self {
            PaneKey::Char(c) => Key::Char(c),
            PaneKey::Enter => Key::Enter,
            PaneKey::Esc => Key::Esc,
            PaneKey::CtrlC => Key::Ctrl('c'),
            PaneKey::CtrlD => Key::Ctrl('d'),
            PaneKey::CtrlQ => Key::Ctrl('q'),
            PaneKey::PageUp => Key::PageUp,
            PaneKey::PageDown => Key::PageDown,
            PaneKey::Up => Key::Up,
            PaneKey::Down => Key::Down,
            PaneKey::Left => Key::Left,
            PaneKey::Right => Key::Right,
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
    Copy,
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
    /// A card row (consent, question, skill trust, plan, delegation, spawn gate, …).
    Card(CardCmd),
}

fn command_of(id: CommandId) -> Option<Cmd> {
    Some(match id {
        CommandId::Overlay(OverlayCmd::PaneQuit) => Cmd::Quit,
        CommandId::Overlay(OverlayCmd::PaneBack) => Cmd::Back,
        CommandId::Overlay(OverlayCmd::PaneEsc) => Cmd::Esc,
        CommandId::Overlay(OverlayCmd::PaneScrollUp) => Cmd::ScrollUp,
        CommandId::Overlay(OverlayCmd::PaneScrollDown) => Cmd::ScrollDown,
        CommandId::Overlay(OverlayCmd::PaneCopy) => Cmd::Copy,
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
        CommandId::Card(card) => Cmd::Card(card),
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

/// The registry context that resolves this item's keys now: its kind's, or its open
/// sub-context's (the Deny + feedback field, the question's answer field, the skill file).
pub(crate) fn context_of(item: &DecisionItem) -> Context {
    if item.is_stale() {
        return Context::Decision;
    }
    match &item.subject {
        DecisionSubject::Permission(payload) if payload.feedback.is_some() => {
            Context::DecisionCompose
        }
        DecisionSubject::Question(payload) if payload.editing => Context::Question,
        DecisionSubject::SkillTrust(payload) if payload.inspecting => Context::SkillTrustInspect,
        _ => item.kind.context(),
    }
}

/// True while the item's own text field is open and takes the keys.
fn field_open(item: &DecisionItem) -> bool {
    matches!(
        context_of(item),
        Context::DecisionCompose | Context::Question
    )
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

/// THE edit target of a decision frame (Rule A, M8): the one text field the item owns
/// while it is open (the Deny + feedback field, the question's answer field) and its byte
/// bound. A frame's keys and paste edit this field and never `state.input_buffer`.
fn edit_target(subject: &mut DecisionSubject) -> Option<(&mut TextField, usize)> {
    match subject {
        DecisionSubject::Permission(payload) => payload
            .feedback
            .as_mut()
            .map(|field| (field, super::permission::FEEDBACK_MAX_BYTES)),
        DecisionSubject::Question(payload) if payload.editing => {
            Some((&mut payload.field, ANSWER_MAX_BYTES))
        }
        _ => None,
    }
}

/// Edit the open field of the focused frame with `edit` (a paste, a key); `edit` gets
/// the field and its byte bound. False when no field is open.
pub(crate) fn edit_field(
    state: &mut TuiState,
    edit: impl FnOnce(&mut TextField, usize) -> bool,
) -> bool {
    let Some(key) = focused_key(state) else {
        return false;
    };
    let Some(item) = state.decisions.item_mut(&key) else {
        return false;
    };
    if item.is_stale() {
        return false;
    }
    let Some((field, max)) = edit_target(&mut item.subject) else {
        return false;
    };
    edit(field, max);
    state.needs_redraw = true;
    true
}

/// Paste into the focused frame's open field.
pub(crate) fn paste(state: &mut TuiState, text: &str) -> bool {
    edit_field(state, |field, max| compose::paste(field, text, max))
}

/// Resolve the key in the focused item's context (letters fold case in `Decision`)
/// and run its command against the item's kind; a command the kind does not offer is
/// swallowed.
fn execute(state: &mut TuiState, key: Key, facts: &LogFacts, host: Host) -> PaneOutcome {
    // A fork or rewind whose message the conversation no longer reaches is stale: its only
    // key is Close, so a stale target never forks or rewinds anything (A7, K9).
    refresh_targets(state, facts);
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
    let in_field = field_open(item);
    let reviewing = matches!(&item.subject, DecisionSubject::Permission(p) if p.reviewing);
    let conversation_id = item.conversation_id.clone();
    let retract_armed =
        !matches!(&item.subject, DecisionSubject::TeamRetract(payload) if !payload.armed);
    let context = context_of(item);
    let inspecting = context == Context::SkillTrustInspect;
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
        return field_key(state, facts, host, &item_key, context, key);
    }
    match command {
        Some(Cmd::Back | Cmd::Esc) if reviewing => {
            leave_review(state, &item_key);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::Card(CardCmd::Back)) if inspecting => {
            skill_trust::leave_inspect(state, &item_key);
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
            scroll(state, &item_key, false, SCROLL_PAGE);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::ScrollDown) => {
            scroll(state, &item_key, true, SCROLL_PAGE);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::Card(CardCmd::Up)) if inspecting => {
            scroll(state, &item_key, false, 1);
            return PaneOutcome::Consumed;
        }
        Some(Cmd::Card(CardCmd::Down)) if inspecting => {
            scroll(state, &item_key, true, 1);
            return PaneOutcome::Consumed;
        }
        // Copying decides nothing, so it is never inert either.
        Some(Cmd::Copy) => return copy(state, &item_key),
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
        (DecisionKind::Consent, Cmd::Card(CardCmd::Yes)) => Some(DecisionEffect::ConsentOnce),
        (DecisionKind::Consent, Cmd::Card(CardCmd::Always)) => Some(DecisionEffect::ConsentAlways),
        (DecisionKind::Consent, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::ConsentDecline),
        (DecisionKind::Question, Cmd::Card(CardCmd::Skip)) => Some(DecisionEffect::QuestionSkip),
        (DecisionKind::SkillTrust, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::SkillTrustAccept)
        }
        (DecisionKind::SkillTrust, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::SkillTrustDecline)
        }
        (DecisionKind::PlanApproval, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::PlanApproveNormal)
        }
        (DecisionKind::PlanApproval, Cmd::Card(CardCmd::Always)) => {
            Some(DecisionEffect::PlanApproveAutoEdit)
        }
        (DecisionKind::PlanApproval, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::PlanReject),
        (DecisionKind::PlanCard, Cmd::Card(CardCmd::Yes)) => Some(DecisionEffect::PlanCardApprove),
        (DecisionKind::PlanCard, Cmd::Card(CardCmd::Edit)) => Some(DecisionEffect::PlanCardEdit),
        (DecisionKind::PlanCard, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::PlanCardReject),
        (DecisionKind::PlanDeviation, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::PlanDeviationApprove)
        }
        (DecisionKind::PlanDeviation, Cmd::Card(CardCmd::Edit)) => {
            Some(DecisionEffect::PlanDeviationEdit)
        }
        (DecisionKind::PlanDeviation, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::PlanDeviationReject)
        }
        (DecisionKind::SkipCascade, Cmd::Card(CardCmd::CascadeSkip)) => {
            Some(DecisionEffect::SkipCascadeSkip)
        }
        (DecisionKind::SkipCascade, Cmd::Card(CardCmd::Continue)) => {
            Some(DecisionEffect::SkipCascadeContinue)
        }
        (DecisionKind::SkipCascade, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::SkipCascadeCancel)
        }
        (DecisionKind::CancelPlan, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::CancelPlanConfirm)
        }
        (DecisionKind::CancelPlan, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::CancelPlanDecline)
        }
        (DecisionKind::AgentKill, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::AgentKillConfirm)
        }
        (DecisionKind::AgentKill, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::AgentKillDecline),
        (DecisionKind::Delegation, Cmd::Card(CardCmd::Delegate)) => {
            Some(DecisionEffect::DelegateTask)
        }
        (DecisionKind::Delegation, Cmd::Card(CardCmd::RunLocal)) => {
            Some(DecisionEffect::RunTaskLocally)
        }
        (DecisionKind::Delegation, Cmd::Card(CardCmd::CancelPlan)) => {
            Some(DecisionEffect::DelegationCardCancel)
        }
        (DecisionKind::SpawnGate, Cmd::Card(CardCmd::Confirm)) => {
            Some(DecisionEffect::SpawnGateConfirm)
        }
        (DecisionKind::SpawnGate, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::SpawnGateCancel),
        (DecisionKind::Consolidation, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::ConsolidationAccept)
        }
        (DecisionKind::Consolidation, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::ConsolidationDecline)
        }
        (DecisionKind::Forget, Cmd::Card(CardCmd::Yes)) => Some(DecisionEffect::ForgetAccept),
        (DecisionKind::Forget, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::ForgetDecline),
        (DecisionKind::ArtifactApply | DecisionKind::ArtifactResolve, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::ArtifactAccept)
        }
        (DecisionKind::ArtifactApply | DecisionKind::ArtifactResolve, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::ArtifactDecline)
        }
        (DecisionKind::PeerAdd, Cmd::Card(CardCmd::Yes)) => Some(DecisionEffect::PeerAddConfirm),
        (DecisionKind::PeerAdd, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::PeerAddDecline),
        // A disarmed retract consumes `y`: it is never painted and never a confirm.
        (DecisionKind::TeamRetract, Cmd::Card(CardCmd::Yes)) if retract_armed => {
            Some(DecisionEffect::TeamRetractConfirm)
        }
        (DecisionKind::TeamRetract, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::TeamRetractDecline)
        }
        (DecisionKind::Fork, Cmd::Card(CardCmd::Yes)) => Some(DecisionEffect::ForkConfirm),
        (DecisionKind::Fork, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::ForkCancel),
        (DecisionKind::Rewind, Cmd::Card(CardCmd::Yes)) => Some(DecisionEffect::RewindConfirm),
        (DecisionKind::Rewind, Cmd::Card(CardCmd::ForkInstead)) => {
            Some(DecisionEffect::RewindForkInstead)
        }
        (DecisionKind::Rewind, Cmd::Card(CardCmd::No)) => Some(DecisionEffect::RewindCancel),
        (DecisionKind::ExportOverwrite, Cmd::Card(CardCmd::Yes)) => {
            Some(DecisionEffect::ExportOverwriteConfirm)
        }
        (DecisionKind::ExportOverwrite, Cmd::Card(CardCmd::No)) => {
            Some(DecisionEffect::ExportOverwriteCancel)
        }
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
        // The per-row selection of `/memory forget` lives inside its frame.
        (
            DecisionKind::Forget,
            Cmd::Card(card @ (CardCmd::Toggle | CardCmd::Up | CardCmd::Down)),
        ) => {
            if let Some(DecisionSubject::Forget(forget)) =
                state.decisions.item_mut(&item_key).map(|i| &mut i.subject)
            {
                match card {
                    CardCmd::Toggle => forget.toggle_focused(),
                    CardCmd::Up => forget.move_focus(false),
                    _ => forget.move_focus(true),
                }
            }
            *state.decisions.body_layout.get_mut() = None;
            if let Some(item) = state.decisions.item(&item_key)
                && let Some(row) = decision_frame::forget_focus_row(
                    item,
                    state,
                    decision_frame::inner_width(state.terminal_width),
                )
            {
                state.decisions.scroll = row;
            }
        }
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
        (DecisionKind::Question, Cmd::Card(CardCmd::Answer)) => {
            if let Some(DecisionSubject::Question(payload)) =
                state.decisions.item_mut(&item_key).map(|i| &mut i.subject)
            {
                payload.editing = true;
            }
        }
        // Reading the file decides nothing: the item stays queued. A file already read
        // opens at once; otherwise the host reads it off the paint path.
        (DecisionKind::SkillTrust, Cmd::Card(CardCmd::Inspect)) => {
            if !skill_trust::reopen_inspect(state, &item_key) {
                return PaneOutcome::Effect(DecisionEffect::SkillTrustInspect);
            }
        }
        // Opening the editor decides nothing: the plan stays awaiting approval.
        (DecisionKind::PlanApproval, Cmd::Card(CardCmd::Edit)) => {
            return PaneOutcome::Effect(DecisionEffect::PlanRevise);
        }
        (
            DecisionKind::SpawnGate,
            Cmd::Card(card @ (CardCmd::Cap | CardCmd::Left | CardCmd::Right)),
        ) => {
            if let Some(DecisionSubject::SpawnGate(gate)) =
                state.decisions.item_mut(&item_key).map(|i| &mut i.subject)
            {
                match card {
                    CardCmd::Cap => gate.cap_at_threshold(),
                    CardCmd::Left => gate.adjust_left(),
                    _ => gate.adjust_right(),
                }
            }
        }
        _ => {}
    }
    PaneOutcome::Consumed
}

/// One key while a frame-owned text field is open: it edits, sends or leaves.
fn field_key(
    state: &mut TuiState,
    facts: &LogFacts,
    host: Host,
    item_key: &DecisionKey,
    context: Context,
    key: Key,
) -> PaneOutcome {
    let kind = item_key.kind;
    let Some((field, max)) = state
        .decisions
        .item_mut(item_key)
        .and_then(|item| edit_target(&mut item.subject))
    else {
        return PaneOutcome::Consumed;
    };
    let empty = field.is_empty();
    match compose::apply(field, context, key, host, max) {
        FieldKey::Send => {
            // An empty answer sends nothing; a send inside the inert window is dropped
            // like every deciding key (the field itself types at once).
            if kind == DecisionKind::Question && empty {
                return PaneOutcome::Consumed;
            }
            if state.decisions.inert(state) {
                return PaneOutcome::Consumed;
            }
            let effect = match kind {
                DecisionKind::Question => DecisionEffect::QuestionSend,
                _ => DecisionEffect::PermissionDenyFeedback,
            };
            resolve(state, facts, item_key, effect)
        }
        FieldKey::Skip => {
            if state.decisions.inert(state) {
                return PaneOutcome::Consumed;
            }
            resolve(state, facts, item_key, DecisionEffect::QuestionSkip)
        }
        FieldKey::Back => {
            match state.decisions.item_mut(item_key).map(|i| &mut i.subject) {
                Some(DecisionSubject::Permission(payload)) => payload.feedback = None,
                Some(DecisionSubject::Question(payload)) => payload.editing = false,
                _ => {}
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

fn scroll(state: &mut TuiState, key: &DecisionKey, forward: bool, step: usize) {
    let Some(item) = state.decisions.item(key) else {
        return;
    };
    let rows = decision_frame::body_rows(item, state);
    // A re-wrap (resize) may have shortened the body since the offset was kept.
    let at = state.decisions.scroll.min(rows.saturating_sub(1));
    state.decisions.scroll = if forward {
        (at + step).min(rows.saturating_sub(1))
    } else {
        at.saturating_sub(step)
    };
}

/// `c`: the item's original text, or the refusal when it is above the copy limit.
fn copy(state: &mut TuiState, key: &DecisionKey) -> PaneOutcome {
    let Some(item) = state.decisions.item(key) else {
        return PaneOutcome::Consumed;
    };
    match decision_frame::copy_source(item, state) {
        CopySource::Text(text) => PaneOutcome::Copy(text),
        CopySource::TooLarge => {
            notify_failure(state, "decision text exceeds the 8 MiB copy limit");
            PaneOutcome::Consumed
        }
    }
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

/// Mark the focused fork or rewind stale once its message is out of reach of the
/// conversation it was started in: such an item can only be closed (A7).
fn refresh_targets(state: &mut TuiState, facts: &LogFacts) {
    let Some(key) = focused_key(state) else {
        return;
    };
    let gone = state.decisions.item(&key).is_some_and(|item| {
        !item.is_stale()
            && item.conversation_id.as_deref() == facts.active_conversation_id.as_deref()
            && super::confirm::target_gone(item, facts)
    });
    if gone && let Some(item) = state.decisions.item_mut(&key) {
        item.validity = super::Validity::Stale(super::NO_LONGER_PENDING);
    }
}
