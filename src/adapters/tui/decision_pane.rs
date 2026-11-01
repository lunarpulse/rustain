//! Interim decision pane (Story 21.1 slice S2, AC9–AC11; RU-5/RU-6/RU-7; A9).
//!
//! The six response-key prompts that used to ride feedback blocks and a flash
//! (recovery, carryover, daily budget, context warning, large image, delete
//! conversation(s)) are items of ONE bounded presentation state, following the
//! S-QUIT / S-LOG precedent: a FIFO of items keyed by (kind, conversation), at
//! most one focused item, a shared key transition and a shared widget used by
//! the local host. 21-4 migrates it onto the modal stack and 21-6 deletes it.
//!
//! Ownership rules (RU-6): an automatic arrival never takes focus from a typing
//! composer, a recent printable key or any open surface; only the focused pane
//! receives decision keys, so a waiting or deferred item can never be answered by
//! a stray key; rail keys are inert for 500 ms after the pane's first paint and
//! are dropped, never replayed. Time comes only from the injected clock.

use std::time::{Duration, Instant};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::render::leader_follow_up_compact;
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notification_log::{LogFacts, footer_lines};
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::modal::wrapped;
use crate::adapters::tui::widgets::{ModalPart, fit_modal};
use crate::domain::events::DomainInputEvent;
use crate::domain::models::FocusState;

/// Rail keys are inert this long after the pane's first paint.
pub const INERT_AFTER_PAINT: Duration = Duration::from_millis(500);
/// A printable key this recent means the user is typing.
pub const TYPING_GRACE: Duration = Duration::from_secs(1);
/// After a deferral, an automatic arrival never takes focus for this long.
pub const DEFER_COOLDOWN: Duration = Duration::from_secs(60);
/// One item's detail is bounded before it is stored (a title can be long).
const MAX_DETAIL_CHARS: usize = 2_048;
/// The toast raised when an arrival could not take focus; the key text
/// (`Ctrl+X q`) is generated from the registry's `Ctrl+X, Q` row (Story 21.3 A10).
#[must_use]
pub fn waiting_toast() -> String {
    let compact = leader_follow_up_compact(LeaderCmd::Decisions);
    format!(
        "Decision waiting — Ctrl+{}",
        compact.strip_prefix('^').unwrap_or(&compact)
    )
}

/// The six interim decision kinds (A9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionKind {
    Recovery,
    Carryover,
    Budget,
    ContextWarning,
    LargeImage,
    DeleteConversations,
}

impl DecisionKind {
    /// Frame title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Recovery => "Session recovery",
            Self::Carryover => "Carry over context",
            Self::Budget => "Daily budget",
            Self::ContextWarning => "Context window",
            Self::LargeImage => "Large image",
            Self::DeleteConversations => "Delete conversations",
        }
    }

    /// True when `Esc` means "Later" (the item stays pending, never destructive).
    #[must_use]
    pub fn deferrable(self) -> bool {
        !matches!(self, Self::Recovery | Self::DeleteConversations)
    }

    /// The painted rail: `(key, label)` pairs, exactly the keys the handler honours. The
    /// key text is generated from the `DecisionPane` rows of the registry (Story 21.3 AC15).
    #[must_use]
    pub fn rail(self) -> Vec<(String, &'static str)> {
        let key = |command: OverlayCmd| keymap::render::rail(Context::DecisionPane, command);
        match self {
            Self::Recovery => vec![
                (
                    format!(
                        "[{}]",
                        keymap::render::rail_pair(
                            Context::DecisionPane,
                            OverlayCmd::PaneEnter,
                            OverlayCmd::PaneEsc
                        )
                    ),
                    "Continue",
                ),
                (key(OverlayCmd::PaneNo), "New session"),
            ],
            Self::Carryover => vec![
                (key(OverlayCmd::PaneYes), "Carry over a summary"),
                (key(OverlayCmd::PaneNo), "Start fresh without it"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::Budget => vec![
                (key(OverlayCmd::PaneYes), "Continue"),
                (key(OverlayCmd::PaneSwitch), "Switch model"),
                (key(OverlayCmd::PanePause), "Pause until tomorrow"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::ContextWarning => vec![
                // Letters fold case in the pane's resolver; the rail paints the capital.
                (key(OverlayCmd::PaneCompact).to_uppercase(), "Compact"),
                (key(OverlayCmd::PaneNo), "Start fresh"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::LargeImage => vec![
                (key(OverlayCmd::PaneYes), "Attach anyway"),
                (key(OverlayCmd::PaneNo), "Cancel"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::DeleteConversations => vec![
                (key(OverlayCmd::PaneYes), "Delete"),
                (key(OverlayCmd::PaneNo), "Cancel"),
                (key(OverlayCmd::PaneEsc), "Cancel"),
            ],
        }
    }
}

/// The existing effect a rail key executes — exactly once (the item is resolved
/// before the effect is returned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionEffect {
    RecoveryContinue,
    RecoveryNewSession,
    /// Compact into a summary and carry it into a fresh tab.
    CarryoverAccept,
    /// Open a fresh tab without a summary.
    CarryoverDecline,
    BudgetContinue,
    BudgetSwitchCheaper,
    BudgetPause,
    ContextCompact,
    AttachLargeImage,
    CancelLargeImage,
    ConfirmDelete,
    CancelDelete,
}

impl DecisionEffect {
    /// The host action that runs the effect (the four delete/image effects keep
    /// their existing arms).
    #[must_use]
    pub fn into_action(self) -> InputAction {
        match self {
            Self::AttachLargeImage => InputAction::ImageConfirmAttach,
            Self::CancelLargeImage => InputAction::ImageConfirmCancel,
            Self::ConfirmDelete => InputAction::ConfirmDelete,
            Self::CancelDelete => InputAction::CancelDelete,
            other => InputAction::Decision(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ItemKey {
    kind: DecisionKind,
    conversation_id: Option<String>,
}

/// One pending decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionItem {
    pub kind: DecisionKind,
    /// `None` = decidable from any tab; `Some(id)` only while `id` is active.
    pub conversation_id: Option<String>,
    pub detail: String,
    /// `Esc`/"Later" was pressed: never re-focused automatically.
    pub deferred: bool,
}

impl DecisionItem {
    fn key(&self) -> ItemKey {
        ItemKey {
            kind: self.kind,
            conversation_id: self.conversation_id.clone(),
        }
    }

    fn actionable(&self, facts: &LogFacts) -> bool {
        self.conversation_id.is_none()
            || self.conversation_id.as_deref() == facts.active_conversation_id.as_deref()
    }
}

/// What a producer asks the pane to hold.
#[derive(Debug, Clone)]
pub struct DecisionDraft {
    pub kind: DecisionKind,
    pub conversation_id: Option<String>,
    pub detail: String,
}

impl DecisionDraft {
    #[must_use]
    pub fn new(kind: DecisionKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            conversation_id: None,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn conversation(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }
}

/// Why a decision was raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// The user's own action raised it (delete, attach, S-LOG `K`/`n`): the pane
    /// takes focus immediately.
    Requested,
    /// The system raised it (startup recovery, budget, context): RU-6 applies.
    Automatic,
}

/// What `raise` did with focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raised {
    Focused,
    /// Held: a `Decision waiting — Ctrl+X q` toast and the status cue tell.
    Waiting,
}

/// The session's pending decisions. Never persisted.
#[derive(Debug, Default)]
pub struct DecisionPane {
    items: Vec<DecisionItem>,
    focused: Option<ItemKey>,
    painted_at: Option<Instant>,
    last_defer_at: Option<Instant>,
}

impl DecisionPane {
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The pane owns user keys.
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused.is_some()
    }

    pub fn items(&self) -> impl ExactSizeIterator<Item = &DecisionItem> {
        self.items.iter()
    }

    #[must_use]
    pub fn holds(&self, kind: DecisionKind) -> bool {
        self.items.iter().any(|item| item.kind == kind)
    }

    /// The focused item, only while it still exists and is decidable here.
    #[must_use]
    pub fn current(&self, facts: &LogFacts) -> Option<&DecisionItem> {
        let key = self.focused.as_ref()?;
        self.items
            .iter()
            .find(|item| item.key() == *key && item.actionable(facts))
    }

    fn focus_on(&mut self, key: ItemKey) {
        if let Some(item) = self.items.iter_mut().find(|item| item.key() == key) {
            item.deferred = false;
        }
        self.focused = Some(key);
        self.painted_at = None;
    }

    fn unfocus(&mut self) {
        self.focused = None;
        self.painted_at = None;
    }

    /// The oldest decidable item, optionally skipping deferred ones.
    fn oldest(&self, facts: &LogFacts, skip_deferred: bool) -> Option<ItemKey> {
        self.items
            .iter()
            .find(|item| item.actionable(facts) && !(skip_deferred && item.deferred))
            .map(DecisionItem::key)
    }

    fn remove(&mut self, key: &ItemKey) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.key() != *key);
        if self.focused.as_ref() == Some(key) {
            self.unfocus();
        }
        self.items.len() != before
    }
}

fn clean(detail: &str) -> String {
    detail
        .chars()
        .take(MAX_DETAIL_CHARS)
        .map(|c| {
            if c == '\n' {
                '\n'
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// A surface that owns the keyboard: an automatic arrival never competes with it.
fn protected(state: &TuiState) -> bool {
    state.quit_confirm.is_some()
        || state.notification_log.is_some()
        || state.help_overlay.active
        || state.model_selector.active
        || state.login.active
        || state.profile_switcher.active
        || state.usage_panel.active
        || state.command_palette.active
        || state.which_key.active
        || state.reverse_search.active
        || state.search_state.active
        || state.ask_user_question.is_some()
        || crate::adapters::tui::app::proposal_higher_confirmation_active(state)
        || !matches!(
            state.focus,
            FocusState::Chat | FocusState::Input | FocusState::Sidebar { .. }
        )
}

/// True while the user is composing: a non-empty draft or a printable key inside
/// the grace window.
fn typing(state: &TuiState, now: Instant) -> bool {
    (state.focus == FocusState::Input && !state.input_buffer.is_empty())
        || state
            .last_printable_at
            .is_some_and(|at| now.saturating_duration_since(at) < TYPING_GRACE)
}

/// Hold a decision. An existing item of the same (kind, conversation) is
/// updated in place. The first item (0→1) of an automatic arrival takes focus
/// only when nothing protects the user's current work and no deferral happened
/// in the last minute; otherwise one Info toast says it is waiting.
pub fn raise(state: &mut TuiState, draft: DecisionDraft, arrival: Arrival) -> Raised {
    let now = state.notifications.now();
    let detail = clean(&draft.detail);
    let key = ItemKey {
        kind: draft.kind,
        conversation_id: draft.conversation_id.clone(),
    };
    let was_empty = state.decisions.items.is_empty();
    let existing = state
        .decisions
        .items
        .iter_mut()
        .find(|item| item.key() == key);
    let is_new = existing.is_none();
    match existing {
        Some(item) => item.detail = detail,
        None => state.decisions.items.push(DecisionItem {
            kind: draft.kind,
            conversation_id: draft.conversation_id,
            detail,
            deferred: false,
        }),
    }
    state.needs_redraw = true;
    let take_focus = match arrival {
        Arrival::Requested => state.quit_confirm.is_none(),
        Arrival::Automatic => {
            was_empty
                && !protected(state)
                && !typing(state, now)
                && !state
                    .decisions
                    .last_defer_at
                    .is_some_and(|at| now.saturating_duration_since(at) < DEFER_COOLDOWN)
        }
    };
    if take_focus {
        state.decisions.focus_on(key);
        return Raised::Focused;
    }
    if is_new {
        notifications::info(state, NoticeSource::System, waiting_toast());
    }
    Raised::Waiting
}

/// Drop every item of `kind` (its condition cleared) whatever conversation it
/// belongs to: only for global conditions (the daily budget). A focused item
/// that goes away releases focus; nothing is promoted here.
pub fn withdraw(state: &mut TuiState, kind: DecisionKind) {
    withdraw_where(state, |item| item.kind == kind);
}

/// Drop the `kind` item keyed to `conversation_id` only: clearing one tab's
/// condition never silently drops another tab's pending decision.
pub fn withdraw_for(state: &mut TuiState, kind: DecisionKind, conversation_id: &str) {
    withdraw_where(state, |item| {
        item.kind == kind && item.conversation_id.as_deref() == Some(conversation_id)
    });
}

/// Drop every item keyed to a conversation that no longer exists (its tab was
/// closed or the conversation deleted): such an item could never be decided, yet
/// would keep `Needs you (N)` up and make the quit guard ask forever. Does
/// nothing without a tab list to compare against.
pub fn prune_closed(state: &mut TuiState, facts: &LogFacts) {
    if facts.titles.is_empty() {
        return;
    }
    withdraw_where(state, |item| {
        item.conversation_id.as_deref().is_some_and(|id| {
            Some(id) != facts.active_conversation_id.as_deref()
                && !facts.titles.iter().any(|(known, _)| known == id)
        })
    });
}

fn withdraw_where(state: &mut TuiState, matches: impl Fn(&DecisionItem) -> bool) {
    let keys: Vec<ItemKey> = state
        .decisions
        .items
        .iter()
        .filter(|item| matches(item))
        .map(DecisionItem::key)
        .collect();
    if keys.is_empty() {
        return;
    }
    for key in &keys {
        state.decisions.remove(key);
    }
    state.needs_redraw = true;
}

/// Raise (and focus) the carryover question for `conversation_id`: the user's
/// own "start fresh" request from S-LOG or the context-warning pane.
pub fn request_carryover(state: &mut TuiState, conversation_id: Option<String>) {
    let mut draft = DecisionDraft::new(
        DecisionKind::Carryover,
        "Carry over a summary of this conversation into a fresh tab?",
    );
    draft.conversation_id = conversation_id;
    raise(state, draft, Arrival::Requested);
}

/// `Ctrl+X q`: focus the oldest decidable item (a deferred one included).
/// False when nothing decidable waits here.
pub fn focus_oldest(state: &mut TuiState, facts: &LogFacts) -> bool {
    let Some(key) = state.decisions.oldest(facts, false) else {
        return false;
    };
    state.decisions.focus_on(key);
    state.needs_redraw = true;
    true
}

/// `Ctrl+X q` as the local host runs it: focus the oldest decidable item, or
/// say honestly why nothing could be focused.
pub fn open_waiting(state: &mut TuiState, facts: &LogFacts) {
    if focus_oldest(state, facts) {
        return;
    }
    let message = if state.decisions.is_empty() {
        "No decisions waiting"
    } else {
        "A decision is waiting in another tab — open that tab to answer it"
    };
    notifications::info(state, NoticeSource::System, message);
}

/// Hand focus to the next waiting, not-deferred decision (the user is already
/// answering: this never steals focus from anything else).
fn promote(state: &mut TuiState, facts: &LogFacts) {
    match state.decisions.oldest(facts, true) {
        Some(key) => state.decisions.focus_on(key),
        None => state.decisions.unfocus(),
    }
}

/// First paint of the focused pane: starts the 500 ms inert window.
pub fn note_painted(state: &mut TuiState) {
    if state.decisions.is_focused() && state.decisions.painted_at.is_none() {
        state.decisions.painted_at = Some(state.notifications.now());
    }
}

/// Decisions waiting, for the `Needs you (N) ^X q` status cue.
#[must_use]
pub fn waiting(state: &TuiState) -> usize {
    state.decisions.len()
}

/// An inline card answers only in Chat focus (Story 21.3 AC17), so a card that arrives while
/// focus is Input would strand the user: `y` would be composer text. On arrival, an empty
/// composer hands the card its focus (FR-21-3-c; the local permission card does the same).
/// A composer with text keeps focus (`user_mid_sentence`): the user is typing and the card
/// waits behind the paint rule. A client that cannot answer (`can_answer` false: read-only
/// attach) and every other focus, an overlay included, stay as they are.
/// Returns whether focus moved; the caller redraws.
pub fn take_chat_focus_for_card(
    focus: &mut FocusState,
    borrowed_from: &mut Option<FocusState>,
    can_answer: bool,
    user_mid_sentence: bool,
) -> bool {
    let moves = can_answer && !user_mid_sentence && *focus == FocusState::Input;
    if moves {
        borrowed_from.get_or_insert(FocusState::Input);
        *focus = FocusState::Chat;
    }
    moves
}

/// Return arrival-borrowed focus once the host's last card settles. A user focus
/// change relinquishes the borrow even while cards remain; never overwrite it.
pub fn settle_borrowed_card_focus(state: &mut TuiState, card_pending: bool) {
    if state.card_focus_borrowed_from.is_none() {
        return;
    }
    if state.focus != FocusState::Chat
        || card_focus_blocked(state)
        || state.recipient_surface.is_some()
    {
        state.card_focus_borrowed_from = None;
    } else if !card_pending {
        state.focus = state.card_focus_borrowed_from.take().unwrap();
        state.needs_redraw = true;
    }
}

/// Key-owning surfaces may keep Input focus; card arrivals must wait for dismissal.
pub fn card_focus_blocked(state: &TuiState) -> bool {
    matches!(state.focus, FocusState::Overlay(_))
        || state.quit_confirm.is_some()
        || state.notification_log.is_some()
        || state.which_key.active
        || state.command_palette.active
        || state.help_overlay.active
        || state.autocomplete.active
        || state.reverse_search.active
}

/// [`take_chat_focus_for_card`] in the local host: the composer is `state.input_buffer`.
pub fn take_chat_focus_for_local_card(state: &mut TuiState) {
    let mid_sentence = !state.input_buffer.is_empty();
    if !card_focus_blocked(state)
        && take_chat_focus_for_card(
            &mut state.focus,
            &mut state.card_focus_borrowed_from,
            true,
            mid_sentence,
        )
    {
        state.needs_redraw = true;
    }
}

/// The status-row cue while decisions wait.
#[must_use]
pub fn status_cue(waiting: usize) -> Option<String> {
    (waiting > 0).then(|| {
        format!(
            "Needs you ({waiting}) {}",
            leader_follow_up_compact(LeaderCmd::Decisions)
        )
    })
}

/// Host-neutral key classes the pane understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKey {
    Char(char),
    Enter,
    Esc,
    CtrlC,
    CtrlQ,
    /// Any other user input: consumed, never leaked to the surface below.
    Other,
}

/// What the host must do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneOutcome {
    /// Swallowed (inert window, unmapped key, deferral, a new item raised).
    Consumed,
    /// `Ctrl+C`/`Ctrl+Q`: run the host's S-QUIT guard.
    Quit,
    /// Run this existing effect exactly once.
    Effect(DecisionEffect),
}

fn inert(state: &TuiState) -> bool {
    let now = state.notifications.now();
    !state
        .decisions
        .painted_at
        .is_some_and(|at| now.saturating_duration_since(at) >= INERT_AFTER_PAINT)
}

impl PaneKey {
    fn key(self) -> Key {
        match self {
            PaneKey::Char(c) => Key::Char(c),
            PaneKey::Enter => Key::Enter,
            PaneKey::Esc => Key::Esc,
            PaneKey::CtrlC => Key::Ctrl('c'),
            PaneKey::CtrlQ => Key::Ctrl('q'),
            PaneKey::Other => Key::Other,
        }
    }
}

/// Apply one key to the focused pane. Without a decidable focused item it
/// consumes the key and heals the focus.
pub fn handle_key(state: &mut TuiState, key: PaneKey, facts: &LogFacts) -> PaneOutcome {
    execute(state, key.key(), facts)
}

/// Resolve the key in `DecisionPane` (letters fold case) and run its command
/// against the focused item's kind; a command the kind does not offer is swallowed.
fn execute(state: &mut TuiState, key: Key, facts: &LogFacts) -> PaneOutcome {
    state.needs_redraw = true;
    let command = match keymap::resolve(Context::DecisionPane, Host::Local, &[key]) {
        Resolved::Hit(row) => match row.command {
            CommandId::Overlay(command) => Some(command),
            _ => None,
        },
        _ => None,
    };
    if command == Some(OverlayCmd::PaneQuit) {
        return PaneOutcome::Quit;
    }
    let Some(item) = state.decisions.current(facts) else {
        state.decisions.unfocus();
        return PaneOutcome::Consumed;
    };
    if inert(state) {
        // Dropped, never replayed once the window passes.
        return PaneOutcome::Consumed;
    }
    let Some(command) = command else {
        return PaneOutcome::Consumed;
    };
    let (kind, conversation_id) = (item.kind, item.conversation_id.clone());
    let item_key = item.key();
    let resolve = |state: &mut TuiState, effect: DecisionEffect| {
        state.decisions.remove(&item_key);
        promote(state, facts);
        PaneOutcome::Effect(effect)
    };
    let defer = |state: &mut TuiState| {
        let now = state.notifications.now();
        if let Some(item) = state
            .decisions
            .items
            .iter_mut()
            .find(|item| item.key() == item_key)
        {
            item.deferred = true;
        }
        state.decisions.last_defer_at = Some(now);
        state.decisions.unfocus();
        PaneOutcome::Consumed
    };
    match (kind, command) {
        (DecisionKind::Recovery, OverlayCmd::PaneEnter | OverlayCmd::PaneEsc) => {
            resolve(state, DecisionEffect::RecoveryContinue)
        }
        (DecisionKind::Recovery, OverlayCmd::PaneNo) => {
            resolve(state, DecisionEffect::RecoveryNewSession)
        }
        (DecisionKind::Carryover, OverlayCmd::PaneYes) => {
            resolve(state, DecisionEffect::CarryoverAccept)
        }
        (DecisionKind::Carryover, OverlayCmd::PaneNo) => {
            resolve(state, DecisionEffect::CarryoverDecline)
        }
        (DecisionKind::Budget, OverlayCmd::PaneYes) => {
            resolve(state, DecisionEffect::BudgetContinue)
        }
        (DecisionKind::Budget, OverlayCmd::PaneSwitch) => {
            resolve(state, DecisionEffect::BudgetSwitchCheaper)
        }
        (DecisionKind::Budget, OverlayCmd::PanePause) => {
            resolve(state, DecisionEffect::BudgetPause)
        }
        (DecisionKind::ContextWarning, OverlayCmd::PaneCompact) => {
            resolve(state, DecisionEffect::ContextCompact)
        }
        (DecisionKind::ContextWarning, OverlayCmd::PaneNo) => {
            // "Start fresh" asks about the carried summary first (RU-21: never `c`).
            state.decisions.remove(&item_key);
            request_carryover(state, conversation_id);
            PaneOutcome::Consumed
        }
        (DecisionKind::LargeImage, OverlayCmd::PaneYes) => {
            resolve(state, DecisionEffect::AttachLargeImage)
        }
        (DecisionKind::LargeImage, OverlayCmd::PaneNo) => {
            resolve(state, DecisionEffect::CancelLargeImage)
        }
        (DecisionKind::DeleteConversations, OverlayCmd::PaneYes) => {
            resolve(state, DecisionEffect::ConfirmDelete)
        }
        (DecisionKind::DeleteConversations, OverlayCmd::PaneNo | OverlayCmd::PaneEsc) => {
            resolve(state, DecisionEffect::CancelDelete)
        }
        (kind, OverlayCmd::PaneEsc) if kind.deferrable() => defer(state),
        _ => PaneOutcome::Consumed,
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
    if !state.decisions.is_focused() {
        return None;
    }
    if state.decisions.current(facts).is_none() {
        // The focused item left (tab switch, condition cleared): heal and let
        // the key reach whoever owns it now.
        state.decisions.unfocus();
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

/// Paint the focused item bottom-anchored over `area` (the chat pane): `Clear`,
/// no layout change. Items that wait unfocused paint nothing here (the status
/// row carries the cue). Returns true only when the frame was actually painted:
/// the host starts the 500 ms inert clock from that, never from a no-op.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    pane: &DecisionPane,
    facts: &LogFacts,
    theme: &Theme,
) -> bool {
    let Some(item) = pane.current(facts) else {
        return false;
    };
    if area.width < 12 || area.height < 5 {
        return false;
    }
    let width = area.width.min(100);
    let inner_width = usize::from(width.saturating_sub(2));
    let normal = Style::default().fg(theme.colors.fg_primary);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default().fg(theme.colors.accent);

    let mut rail: Vec<Span<'static>> = Vec::new();
    for (key, label) in item.kind.rail() {
        rail.push(Span::styled(key, accent));
        rail.push(Span::styled(format!(" {label}  "), normal));
    }
    let rail_lines = footer_lines(rail, inner_width);
    let detail = wrapped(&item.detail, normal, inner_width);
    let others = pane.len().saturating_sub(1);
    let mut parts: Vec<ModalPart> = Vec::new();
    if others > 0 {
        let passive = pane
            .items()
            .filter(|other| other.key() != item.key() && !other.actionable(facts))
            .count();
        let text = if passive > 0 {
            format!("{others} more waiting — {passive} in another tab: open it to decide")
        } else {
            format!("{others} more waiting")
        };
        parts.push(ModalPart::Notice(vec![Line::from(Span::styled(
            text, muted,
        ))]));
    }
    let detail_len = detail.len();
    parts.push(ModalPart::Rows {
        lines: detail,
        selected: (0, 0),
    });
    let rail_len = rail_lines.len();
    parts.push(ModalPart::Footer(rail_lines));

    let wanted = u16::try_from(detail_len.min(8) + rail_len + usize::from(others > 0) + 2)
        .unwrap_or(u16::MAX);
    let height = wanted.max(5).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.bottom().saturating_sub(height),
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(accent)
        .title(format!(" System · {} ", item.kind.title()))
        .style(Style::default().bg(theme.colors.bg_surface));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let lines = fit_modal(parts, usize::from(inner.height), usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::notifications::NotificationStore;
    use crate::domain::clock::MockClock;
    use crate::domain::models::NotificationsConfig;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    const ALL: [DecisionKind; 6] = [
        DecisionKind::Recovery,
        DecisionKind::Carryover,
        DecisionKind::Budget,
        DecisionKind::ContextWarning,
        DecisionKind::LargeImage,
        DecisionKind::DeleteConversations,
    ];

    fn clocked() -> (TuiState, Arc<MockClock>) {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(80, 24);
        state.notifications =
            NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
        (state, clock)
    }

    fn facts(active: &str) -> LogFacts {
        LogFacts {
            active_conversation_id: Some(active.to_owned()),
            ..LogFacts::default()
        }
    }

    fn ready(state: &mut TuiState, clock: &MockClock) {
        note_painted(state);
        clock.advance(INERT_AFTER_PAINT);
    }

    fn draw(state: &TuiState, f: &LogFacts, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = state.theme.clone();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height.saturating_sub(4));
                render(frame, area, &state.decisions, f, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn story21_1_every_painted_rail_key_is_handled_and_nothing_else_is() {
        // The rail is the contract: each painted key executes (or defers/cancels),
        // and a key the rail does not paint resolves nothing.
        for kind in ALL {
            let mut painted: Vec<char> = Vec::new();
            for (key, _) in kind.rail() {
                let inner = key.trim_matches(|c| c == '[' || c == ']');
                match inner {
                    "Enter/Esc" => painted.extend(['\r', '\u{1b}']),
                    "Esc" => painted.push('\u{1b}'),
                    other => painted.extend(other.chars().take(1)),
                }
            }
            for painted_key in &painted {
                let (mut state, clock) = clocked();
                raise(
                    &mut state,
                    DecisionDraft::new(kind, "x"),
                    Arrival::Requested,
                );
                ready(&mut state, &clock);
                let key = match painted_key {
                    '\r' => PaneKey::Enter,
                    '\u{1b}' => PaneKey::Esc,
                    c => PaneKey::Char(*c),
                };
                let kinds_before: Vec<DecisionKind> =
                    state.decisions.items().map(|i| i.kind).collect();
                let outcome = handle_key(&mut state, key, &LogFacts::default());
                let kinds_after: Vec<DecisionKind> =
                    state.decisions.items().map(|i| i.kind).collect();
                let resolved = kinds_after != kinds_before
                    || !state.decisions.is_focused()
                    || matches!(outcome, PaneOutcome::Effect(_));
                assert!(
                    resolved,
                    "{kind:?}: painted key {painted_key:?} did nothing"
                );
            }
            let strays = [
                PaneKey::Char('z'),
                PaneKey::Char('q'),
                PaneKey::Char('x'),
                PaneKey::Char('c'),
                PaneKey::Char('Y'),
                PaneKey::Enter,
            ];
            for stray in strays {
                let painted_too = match stray {
                    PaneKey::Char(c) => painted.iter().any(|p| p.eq_ignore_ascii_case(&c)),
                    PaneKey::Enter => painted.contains(&'\r'),
                    _ => false,
                };
                if painted_too {
                    continue;
                }
                let (mut state, clock) = clocked();
                raise(
                    &mut state,
                    DecisionDraft::new(kind, "x"),
                    Arrival::Requested,
                );
                ready(&mut state, &clock);
                let outcome = handle_key(&mut state, stray, &LogFacts::default());
                assert_eq!(
                    outcome,
                    PaneOutcome::Consumed,
                    "{kind:?}: unpainted {stray:?}"
                );
                assert!(
                    state.decisions.is_focused(),
                    "{kind:?}: {stray:?} resolved it"
                );
            }
        }
    }

    #[test]
    fn story21_1_the_inert_clock_starts_only_from_a_frame_that_really_painted() {
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Requested,
        );
        let theme = state.theme.clone();
        let f = LogFacts::default();
        for (width, height) in [(10u16, 4u16), (11, 8), (40, 4)] {
            let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
            let mut painted = true;
            terminal
                .draw(|frame| {
                    painted = render(
                        frame,
                        Rect::new(0, 0, width, height),
                        &state.decisions,
                        &f,
                        &theme,
                    );
                })
                .unwrap();
            assert!(!painted, "{width}x{height} is below 12x5: nothing painted");
        }
        // The host only starts the clock when `render` said true, so it never ran.
        assert_eq!(state.decisions.painted_at, None);
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Consumed,
            "a key typed while nothing was visible never answers"
        );
        assert!(state.decisions.holds(DecisionKind::Budget));

        // Positive control: a roomy area paints, and the host then starts the clock.
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                painted = render(frame, Rect::new(0, 0, 60, 12), &state.decisions, &f, &theme);
            })
            .unwrap();
        assert!(painted);
        note_painted(&mut state);
        clock.advance(INERT_AFTER_PAINT);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Effect(DecisionEffect::BudgetContinue)
        );
    }

    #[test]
    fn story21_1_a_promoted_item_gets_its_own_inert_window_so_one_double_press_answers_once() {
        let (mut state, clock) = clocked();
        let f = LogFacts::default();
        for (kind, arrival) in [
            (DecisionKind::Budget, Arrival::Requested),
            (DecisionKind::LargeImage, Arrival::Automatic),
        ] {
            raise(&mut state, DecisionDraft::new(kind, "q"), arrival);
        }
        ready(&mut state, &clock);
        let y = PaneKey::Char('y');
        assert_eq!(
            handle_key(&mut state, y, &f),
            PaneOutcome::Effect(DecisionEffect::BudgetContinue)
        );
        // Promoted but unpainted: the immediate second `y` is dropped.
        assert_eq!(handle_key(&mut state, y, &f), PaneOutcome::Consumed);
        assert!(state.decisions.holds(DecisionKind::LargeImage));
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, y, &f),
            PaneOutcome::Effect(DecisionEffect::AttachLargeImage)
        );
    }

    #[test]
    fn story21_1_an_item_brought_to_focus_over_a_painted_one_gets_its_own_inert_window() {
        let f = LogFacts::default();
        // A Requested arrival while another decision is focused and past its window:
        // a key meant for the first one must not answer the new (destructive) one.
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Requested,
        );
        ready(&mut state, &clock);
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::DeleteConversations, "d"),
            Arrival::Requested,
        );
        assert_eq!(
            state.decisions.current(&f).map(|i| i.kind),
            Some(DecisionKind::DeleteConversations)
        );
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Consumed,
            "the new item has not painted: `y` is dropped"
        );
        assert!(state.decisions.holds(DecisionKind::DeleteConversations));
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Effect(DecisionEffect::ConfirmDelete)
        );

        // `Ctrl+X q` (`focus_oldest`) over a painted focused item.
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::LargeImage, "i"),
            Arrival::Requested,
        );
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Esc, &f),
            PaneOutcome::Consumed,
            "Later"
        );
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Requested,
        );
        ready(&mut state, &clock);
        assert!(focus_oldest(&mut state, &f));
        assert_eq!(
            state.decisions.current(&f).map(|i| i.kind),
            Some(DecisionKind::LargeImage)
        );
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Consumed,
            "the refocused image has not painted: `y` is dropped"
        );
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Effect(DecisionEffect::AttachLargeImage)
        );
    }

    #[test]
    fn story21_1_withdrawing_for_one_conversation_leaves_another_tabs_decision() {
        let (mut state, _clock) = clocked();
        for id in ["c1", "c2"] {
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::ContextWarning, "low").conversation(id),
                Arrival::Automatic,
            );
        }
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Carryover, "carry").conversation("c1"),
            Arrival::Automatic,
        );
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Carryover, "carry").conversation("c2"),
            Arrival::Automatic,
        );
        assert_eq!(state.decisions.len(), 4);
        // The context-warning clear for c1.
        withdraw_for(&mut state, DecisionKind::ContextWarning, "c1");
        let left: Vec<(DecisionKind, Option<String>)> = state
            .decisions
            .items()
            .map(|i| (i.kind, i.conversation_id.clone()))
            .collect();
        assert!(!left.contains(&(DecisionKind::ContextWarning, Some("c1".into()))));
        assert!(left.contains(&(DecisionKind::ContextWarning, Some("c2".into()))));
        assert_eq!(left.len(), 3);
        // `/new` on c1 withdraws its carryover and context warning, never c2's.
        withdraw_for(&mut state, DecisionKind::Carryover, "c1");
        withdraw_for(&mut state, DecisionKind::ContextWarning, "c1");
        let left: Vec<(DecisionKind, Option<String>)> = state
            .decisions
            .items()
            .map(|i| (i.kind, i.conversation_id.clone()))
            .collect();
        assert_eq!(
            left,
            vec![
                (DecisionKind::ContextWarning, Some("c2".to_owned())),
                (DecisionKind::Carryover, Some("c2".to_owned())),
            ]
        );
        // The daily budget is global: `withdraw` drops it whatever its key.
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Automatic,
        );
        withdraw(&mut state, DecisionKind::Budget);
        assert!(!state.decisions.holds(DecisionKind::Budget));
        assert_eq!(state.decisions.len(), 2);
    }

    #[test]
    fn story21_1_items_of_a_closed_conversation_are_pruned_and_stop_counting() {
        let (mut state, _clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::ContextWarning, "low").conversation("c1"),
            Arrival::Automatic,
        );
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Automatic,
        );
        assert_eq!(waiting(&state), 2);
        // No tab list to compare against: nothing is pruned.
        prune_closed(&mut state, &LogFacts::default());
        assert_eq!(waiting(&state), 2);
        // c1 is still a tab: kept.
        let with_c1 = LogFacts {
            titles: vec![("c1".into(), "one".into()), ("c2".into(), "two".into())],
            ..facts("c2")
        };
        prune_closed(&mut state, &with_c1);
        assert_eq!(waiting(&state), 2);
        // c1 was closed or deleted: its item can never be decided and goes; the
        // global budget item (no conversation) stays.
        let without_c1 = LogFacts {
            titles: vec![("c2".into(), "two".into())],
            ..facts("c2")
        };
        prune_closed(&mut state, &without_c1);
        assert_eq!(waiting(&state), 1);
        assert!(!state.decisions.holds(DecisionKind::ContextWarning));
        assert!(state.decisions.holds(DecisionKind::Budget));
        assert_eq!(
            status_cue(waiting(&state)).as_deref(),
            Some("Needs you (1) ^X q")
        );
        // The active conversation is never pruned even if the tab list lags.
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Carryover, "x").conversation("c3"),
            Arrival::Automatic,
        );
        let lagging = LogFacts {
            titles: vec![("c2".into(), "two".into())],
            ..facts("c3")
        };
        prune_closed(&mut state, &lagging);
        assert!(state.decisions.holds(DecisionKind::Carryover));
    }

    #[test]
    fn story21_1_each_kind_paints_its_rail_and_fits_sixty_by_sixteen() {
        let table = [
            (
                DecisionKind::Recovery,
                "[Enter/Esc] Continue  [n] New session",
            ),
            (
                DecisionKind::Carryover,
                "[y] Carry over a summary  [n] Start fresh without it  [Esc] Later",
            ),
            (
                DecisionKind::Budget,
                "[y] Continue  [s] Switch model  [p] Pause until tomorrow  [Esc] Later",
            ),
            (
                DecisionKind::ContextWarning,
                "[K] Compact  [n] Start fresh  [Esc] Later",
            ),
            (
                DecisionKind::LargeImage,
                "[y] Attach anyway  [n] Cancel  [Esc] Later",
            ),
            (
                DecisionKind::DeleteConversations,
                "[y] Delete  [n] Cancel  [Esc] Cancel",
            ),
        ];
        for (kind, rail) in table {
            for (width, height) in [(60u16, 16u16), (80, 24)] {
                let (mut state, _clock) = clocked();
                raise(
                    &mut state,
                    DecisionDraft::new(
                        kind,
                        "A long question that has to wrap around the narrow frame at sixty columns without losing the rail below it",
                    ),
                    Arrival::Requested,
                );
                let text = draw(&state, &LogFacts::default(), width, height);
                // The rail wraps by whole key/label pairs at narrow widths: every
                // pair is intact somewhere on screen.
                for pair in rail.split("  ") {
                    assert!(
                        text.contains(pair),
                        "{kind:?} at {width}x{height}: {pair:?} missing\n{text}"
                    );
                }
                assert!(text.contains(kind.title()), "{kind:?}: frame title");
                assert!(!text.contains("Ctrl+K") && !text.contains("[y/n]"));
            }
        }
    }

    #[test]
    fn story21_1_detail_is_sanitized_and_bounded() {
        let (mut state, _c) = clocked();
        let hostile = format!("Delete \"a\u{1b}[31mred\u{7}\"?\n{}", "x".repeat(10_000));
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::DeleteConversations, hostile),
            Arrival::Requested,
        );
        let item = state.decisions.items().next().unwrap();
        assert!(!item.detail.contains('\u{1b}') && !item.detail.contains('\u{7}'));
        assert!(item.detail.chars().count() <= MAX_DETAIL_CHARS);
        assert!(item.detail.contains('\n'), "newlines keep their rows");
    }

    #[test]
    fn story21_1_one_item_per_kind_and_conversation_upserts_in_place() {
        let (mut state, _c) = clocked();
        for text in ["first", "second"] {
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::ContextWarning, text).conversation("c1"),
                Arrival::Automatic,
            );
        }
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions.items().next().unwrap().detail, "second");
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::ContextWarning, "other tab").conversation("c2"),
            Arrival::Automatic,
        );
        assert_eq!(state.decisions.len(), 2, "(kind, conversation) is the key");
        // Only one waiting toast per NEW item, never one per upsert.
        let toasts = state
            .notifications
            .visible()
            .filter(|r| r.message == waiting_toast())
            .count();
        assert_eq!(
            toasts, 1,
            "the second arrival waited behind the focused first"
        );
    }

    #[test]
    fn story21_1_an_item_of_another_conversation_is_passive_and_a_stray_key_never_answers_it() {
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Carryover, "x").conversation("other"),
            Arrival::Requested,
        );
        // Focus was taken at raise time, but the item is not decidable here: the
        // very first key heals the focus and flows on to the surface below.
        ready(&mut state, &clock);
        let event = DomainInputEvent::KeyPress('y');
        assert_eq!(
            handle_domain_event(&mut state, &event, &facts("active")),
            None
        );
        assert!(!state.decisions.is_focused());
        assert!(
            state.decisions.holds(DecisionKind::Carryover),
            "the item still waits"
        );
        // Ctrl+X q says where it waits instead of focusing it.
        open_waiting(&mut state, &facts("active"));
        let last = state.notifications.records().next_back().unwrap();
        assert!(last.message.contains("another tab"), "{}", last.message);
        assert!(!state.decisions.is_focused());
        // Back on its own tab the same chord focuses it.
        open_waiting(&mut state, &facts("other"));
        assert!(state.decisions.is_focused());
    }

    #[test]
    fn story21_1_resolving_promotes_the_next_waiting_item_but_never_a_deferred_one() {
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Requested,
        );
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::LargeImage, "i"),
            Arrival::Automatic,
        );
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::DeleteConversations, "d"),
            Arrival::Automatic,
        );
        assert_eq!(state.decisions.len(), 3);
        // Defer the image, then answer the budget: the delete (not deferred) is next.
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &LogFacts::default()),
            PaneOutcome::Effect(DecisionEffect::BudgetContinue)
        );
        let current = state
            .decisions
            .current(&LogFacts::default())
            .map(|i| i.kind);
        assert_eq!(
            current,
            Some(DecisionKind::LargeImage),
            "FIFO: the image is oldest"
        );
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Esc, &LogFacts::default()),
            PaneOutcome::Consumed,
            "Esc defers the image"
        );
        assert!(!state.decisions.is_focused());
        assert!(focus_oldest(&mut state, &LogFacts::default()));
        assert_eq!(
            state
                .decisions
                .current(&LogFacts::default())
                .map(|i| i.kind),
            Some(DecisionKind::LargeImage),
            "Ctrl+X q focuses the oldest, deferred or not"
        );
    }

    #[test]
    fn story21_1_ctrl_c_and_ctrl_q_reach_the_quit_guard_even_inside_the_inert_window() {
        let (mut state, _clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Recovery, "r"),
            Arrival::Automatic,
        );
        // Never painted: every rail key is inert, but quitting is never trapped.
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('n'), &LogFacts::default()),
            PaneOutcome::Consumed
        );
        assert_eq!(
            handle_key(&mut state, PaneKey::CtrlC, &LogFacts::default()),
            PaneOutcome::Quit
        );
        assert_eq!(
            handle_key(&mut state, PaneKey::CtrlQ, &LogFacts::default()),
            PaneOutcome::Quit
        );
        assert!(
            state.decisions.holds(DecisionKind::Recovery),
            "the guard decides, not the pane"
        );
    }

    #[test]
    fn story21_1_context_warning_start_fresh_asks_the_carryover_question_first() {
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::ContextWarning, "low").conversation("c"),
            Arrival::Requested,
        );
        ready(&mut state, &clock);
        let f = facts("c");
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('n'), &f),
            PaneOutcome::Consumed
        );
        assert!(!state.decisions.holds(DecisionKind::ContextWarning));
        assert!(
            state.decisions.holds(DecisionKind::Carryover),
            "n raises, never acts"
        );
        assert_eq!(
            state.decisions.current(&f).map(|i| i.kind),
            Some(DecisionKind::Carryover)
        );
        // RU-21: `c` is never Compact.
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('c'), &f),
            PaneOutcome::Consumed
        );
        assert!(state.decisions.holds(DecisionKind::Carryover));
    }

    #[test]
    fn story21_1_the_status_cue_counts_every_waiting_item() {
        assert_eq!(status_cue(0), None);
        assert_eq!(status_cue(1).as_deref(), Some("Needs you (1) ^X q"));
        assert_eq!(status_cue(3).as_deref(), Some("Needs you (3) ^X q"));
    }

    /// FR-21-3-c: a card arriving in an empty Input hands it Chat focus; nothing else moves.
    #[test]
    fn story21_3_card_arrival_takes_chat_focus_only_from_an_empty_input() {
        use crate::domain::models::visual::OverlayType;
        let overlay = FocusState::Overlay(OverlayType::CommandPalette);
        // (focus, can answer, user mid-sentence) -> (focus after, moved)
        let cases = [
            (FocusState::Input, true, false, FocusState::Chat, true),
            (FocusState::Input, true, true, FocusState::Input, false),
            (FocusState::Chat, true, false, FocusState::Chat, false),
            (FocusState::Input, false, false, FocusState::Input, false),
            (overlay.clone(), true, false, overlay, false),
        ];
        for (mut focus, can_answer, typing, want, moved) in cases {
            let before = focus.clone();
            let mut borrowed = None;
            assert_eq!(
                take_chat_focus_for_card(&mut focus, &mut borrowed, can_answer, typing),
                moved,
                "{before:?} answer={can_answer} typing={typing}"
            );
            assert_eq!(
                focus, want,
                "{before:?} answer={can_answer} typing={typing}"
            );
            assert_eq!(borrowed, moved.then_some(FocusState::Input));
        }
        // The local host reads the composer from the state and asks for a redraw.
        let mut state = TuiState::new(80, 24);
        state.needs_redraw = false;
        take_chat_focus_for_local_card(&mut state);
        assert!(state.focus == FocusState::Chat && state.needs_redraw);
        let mut typing = TuiState::new(80, 24);
        typing.input_buffer = "draft".into();
        typing.needs_redraw = false;
        take_chat_focus_for_local_card(&mut typing);
        assert!(typing.focus == FocusState::Input && !typing.needs_redraw);
    }
}
