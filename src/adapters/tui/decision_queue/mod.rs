//! The session decision queue (Story 21.6 slice 6-A), grown from the 21-1 interim
//! decision pane (S2, AC9–AC11; RU-5/RU-6/RU-7; AD-2106).
//!
//! Every question the session asks the user is an item of ONE FIFO keyed by
//! `(kind, source, conversation)`, counted by the `Needs you (N)` status cue and
//! the quit guard. A decision is an L3 `Surface::Decision` frame on the 21-4
//! stack: it opens only through [`raise`]/[`open`]/[`open_list`] (producers
//! never write focus), pops back to exactly what it interrupted, and `Esc` sets
//! it aside without ever deciding it.
//!
//! RU-6 arrivals may interrupt a non-text L2, never typing or a higher layer.
//! Rail keys are inert for 500 ms from the frame's first paint; `Ctrl+C` backs
//! without deciding. This module is pure: no I/O and no clock of its own.

mod compose;
mod keys;
mod kinds;
mod list;
mod permission;

use std::collections::HashSet;
use std::time::{Duration, Instant};

pub use keys::{PaneKey, PaneOutcome, handle_domain_event, handle_key, settle_sent};
pub(crate) use keys::{clear_feedback, paste};
pub use kinds::{
    APPROVAL_NO_LONGER_PENDING, DecisionEffect, DecisionKind, DecisionSubject, EscPolicy,
    NO_LONGER_PENDING, Responder, StaleCopy, Validity,
};
pub(crate) use list::selected_index as list_selected_index;
pub use list::{ListOutcome, handle_list_key, open_list};
pub use permission::{
    ConsentPayload, PermissionInput, PermissionPayload, complete_input, drain_permissions,
    holds_request, raise_consent, raise_permission, withdraw_request,
};

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::render::leader_follow_up_compact;
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context};
use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::{self, Surface};
use crate::adapters::tui::state::TuiState;
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

/// State-only effects consumed by the live pane action arms, after the pane has
/// resolved its item and restored or promoted its caller.
/// Surviving caller frames keep their interrupted base focus. Only an empty
/// stack uses the pre-stack restoration (including permission/recipient handoff
/// for images and sidebar selection for delete cancellation). The host supplies
/// its existing image-focus policy without coupling the pane to its runtime.
pub(crate) fn apply_resolved_action(
    state: &mut TuiState,
    action: InputAction,
    restore_base_focus: fn(&mut TuiState),
) {
    match action {
        InputAction::Decision(DecisionEffect::RecoveryContinue) => {
            if state.modal_stack.is_empty() {
                presentation::reducer::set_base(state, FocusState::Input);
            }
        }
        InputAction::ImageConfirmAttach => {
            if let Some(DecisionSubject::LargeImage(attachment)) = state.decisions.take_resolved() {
                use crate::adapters::tui::image;
                state.pending_images.push(attachment);
                state.image_indicator = Some(image::format_image_indicator(
                    state.pending_images.len(),
                    state
                        .pending_images
                        .iter()
                        .fold(0usize, |acc, i| acc.saturating_add(i.data.len() / 1024)),
                ));
            }
            if state.modal_stack.is_empty() {
                restore_base_focus(state);
            }
        }
        InputAction::ImageConfirmCancel => {
            drop(state.decisions.take_resolved());
            notifications::info(state, NoticeSource::Image, "Image attachment cancelled");
            if state.modal_stack.is_empty() {
                restore_base_focus(state);
            }
        }
        InputAction::CancelDelete => {
            drop(state.decisions.take_resolved());
            if state.modal_stack.is_empty() {
                let focus = if state.sidebar_visible {
                    FocusState::Sidebar {
                        panel: crate::domain::models::visual::PanelType::History,
                        selected: state.sidebar_selected,
                    }
                } else {
                    FocusState::Input
                };
                presentation::reducer::set_base(state, focus);
            }
        }
        _ => unreachable!("only state-only resolved pane actions enter this seam"),
    }
    state.needs_redraw = true;
}

/// Stable identity of one decision: the kind, the source's own identity (empty
/// for the interim kinds, whose source is the session itself) and the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionKey {
    pub kind: DecisionKind,
    pub source: String,
    pub conversation_id: Option<String>,
}

impl DecisionKey {
    /// The stack frame's title (`Decision: <kind title>`).
    pub(crate) fn title(&self) -> &'static str {
        self.kind.frame_title()
    }
}

/// One pending decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionItem {
    pub kind: DecisionKind,
    /// The source's own identity; empty for the interim kinds.
    pub source: String,
    /// `None` = decidable from any tab; `Some(id)` only while `id` is active.
    pub conversation_id: Option<String>,
    /// What the source is, in a few words (breadcrumb and list row); the source
    /// identity itself when empty.
    pub summary: String,
    pub detail: String,
    /// The payload the kind's effect consumes (moved here from its old slot).
    pub subject: DecisionSubject,
    /// `Esc`/"Later" was pressed: never re-focused automatically.
    pub deferred: bool,
    /// A stale item answers nothing; its only key is Close.
    pub validity: Validity,
    pub arrived: Instant,
    pub responder: Responder,
}

impl DecisionItem {
    pub(crate) fn key(&self) -> DecisionKey {
        DecisionKey {
            kind: self.kind,
            source: self.source.clone(),
            conversation_id: self.conversation_id.clone(),
        }
    }

    fn matches_key(&self, key: &DecisionKey) -> bool {
        self.kind == key.kind
            && self.conversation_id == key.conversation_id
            && self.source == key.source
    }

    pub(crate) fn actionable(&self, facts: &LogFacts) -> bool {
        self.conversation_id.is_none()
            || self.conversation_id.as_deref() == facts.active_conversation_id.as_deref()
    }

    #[must_use]
    pub fn is_stale(&self) -> bool {
        matches!(self.validity, Validity::Stale(_))
    }
}

/// What a producer asks the queue to hold.
#[derive(Debug, Clone)]
pub struct DecisionDraft {
    pub kind: DecisionKind,
    pub source: String,
    pub conversation_id: Option<String>,
    pub summary: String,
    pub detail: String,
    pub subject: DecisionSubject,
    pub responder: Responder,
}

impl DecisionDraft {
    #[must_use]
    pub fn new(kind: DecisionKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            source: String::new(),
            conversation_id: None,
            summary: String::new(),
            detail: detail.into(),
            subject: DecisionSubject::Text,
            responder: Responder::Local,
        }
    }

    #[must_use]
    pub fn conversation(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }

    /// The source's own identity, for kinds with more than one source per conversation.
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// What the source is, in a few words (the tool name of a permission).
    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// The route that answers it (never re-targeted once queued).
    #[must_use]
    pub fn responder(mut self, responder: Responder) -> Self {
        self.responder = responder;
        self
    }

    #[must_use]
    pub fn subject(mut self, subject: DecisionSubject) -> Self {
        self.subject = subject;
        self
    }
}

/// Why a decision was raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// The user's own action raised it (delete, attach, S-LOG `K`/`n`): the pane
    /// takes focus immediately, except over the quit guard or a text field.
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
pub struct DecisionQueue {
    items: Vec<DecisionItem>,
    painted_at: Option<Instant>,
    /// The inert window's end was already announced to the redraw tick.
    inert_released: bool,
    last_defer_at: Option<Instant>,
    /// Surface-owned refusal; expires on its next key, never a hidden toast.
    feedback: Option<&'static str>,
    /// First visible body row of the focused frame (`PgUp`/`PgDn`).
    scroll: usize,
    pub(crate) permission_layout: std::cell::RefCell<
        Option<std::sync::Arc<crate::adapters::tui::widgets::decision_frame::PreparedPermission>>,
    >,
    /// The payload of the item the last rail key resolved, until its effect arm
    /// consumes it (the same tick); the queue is its only owner.
    resolved: Option<DecisionSubject>,
    /// An attached item whose effect the host is sending: the item stays queued
    /// until the send succeeds (the `AttachedApprovals::decide` rule, A8).
    sending: Option<(DecisionKey, DecisionEffect)>,
    /// The active tab's conversation, as the last draw saw it: an item for
    /// another conversation is never focused (it waits, counted).
    active_conversation: Option<String>,
    /// The S-QUEUE list's selected item (the oldest non-deferred when `None`).
    pub(crate) list_selected: Option<DecisionKey>,
    /// A read-only mirror's count of approvals the writer still owes (A14).
    mirror_waiting: usize,
    /// Request ids a mirror already saw retire (a repeated close counts once).
    mirror_retired: HashSet<String>,
}

impl DecisionQueue {
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> impl ExactSizeIterator<Item = &DecisionItem> {
        self.items.iter()
    }

    #[must_use]
    pub fn holds(&self, kind: DecisionKind) -> bool {
        self.items.iter().any(|item| item.kind == kind)
    }

    /// An item of `kind` the user has not set aside: the only kind of waiting
    /// that gates other prompts (a deferred large image never does).
    #[must_use]
    pub fn holds_undeferred(&self, kind: DecisionKind) -> bool {
        self.items
            .iter()
            .any(|item| item.kind == kind && !item.deferred)
    }

    /// How many items of `kind` wait.
    #[must_use]
    pub fn count(&self, kind: DecisionKind) -> usize {
        self.items.iter().filter(|item| item.kind == kind).count()
    }

    /// How many items of `kind` can still be decided (a stale one cannot).
    #[must_use]
    pub fn count_live(&self, kind: DecisionKind) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind == kind && !item.is_stale())
            .count()
    }

    /// How many items can still be decided: what quitting would leave unanswered.
    #[must_use]
    pub fn undecided(&self) -> usize {
        self.items.iter().filter(|item| !item.is_stale()).count()
    }

    pub(crate) fn contains_key(&self, key: &DecisionKey) -> bool {
        self.items.iter().any(|item| item.matches_key(key))
    }

    pub(crate) fn item(&self, key: &DecisionKey) -> Option<&DecisionItem> {
        self.items.iter().find(|item| item.matches_key(key))
    }

    fn item_mut(&mut self, key: &DecisionKey) -> Option<&mut DecisionItem> {
        self.items.iter_mut().find(|item| item.matches_key(key))
    }

    pub(crate) fn prepare(&mut self, key: &DecisionKey) {
        if let Some(item) = self.item_mut(key) {
            item.deferred = false;
            // A frame opens on the request itself, never inside a sub-context.
            if let DecisionSubject::Permission(payload) = &mut item.subject {
                payload.reviewing = false;
                payload.feedback = None;
            }
        }
        self.clear_paint();
        self.feedback = None;
        self.scroll = 0;
    }

    pub(crate) fn clear_paint(&mut self) {
        self.painted_at = None;
        self.inert_released = false;
    }

    pub(crate) fn last_defer_at(&self) -> Option<Instant> {
        self.last_defer_at
    }

    pub(crate) fn feedback(&self) -> Option<&'static str> {
        self.feedback
    }

    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    /// The payload of the item a rail key just resolved; taken once by its effect arm.
    pub(crate) fn take_resolved(&mut self) -> Option<DecisionSubject> {
        self.resolved.take()
    }

    /// The attached item the host must send now, with its payload: it stays queued
    /// until [`settle_sent`] retires it after a successful send.
    pub(crate) fn take_sending(
        &mut self,
    ) -> Option<(DecisionKey, DecisionEffect, DecisionSubject)> {
        let (key, effect) = self.sending.take()?;
        let subject = self.item(&key)?.subject.clone();
        Some((key, effect, subject))
    }

    /// A read-only mirror's handshake count: approvals the writer still owes.
    pub fn set_mirror_waiting(&mut self, waiting: usize) {
        self.mirror_waiting = waiting;
        self.mirror_retired.clear();
    }

    /// A mirror saw `request_id` retire (`ApprovalClosed`, or `ApprovalRefused` with
    /// `NoLongerPending`); a repeat of the same id counts once (A14).
    pub fn retire_mirror(&mut self, request_id: &str) {
        if self.mirror_retired.insert(request_id.to_owned()) {
            self.mirror_waiting = self.mirror_waiting.saturating_sub(1);
        }
    }

    /// Approvals a read-only mirror knows the writer still has to answer.
    #[must_use]
    pub fn mirror_waiting(&self) -> usize {
        self.mirror_waiting
    }

    /// True until 500 ms after the focused frame's first paint (RU-6).
    pub(crate) fn inert(&self, state: &TuiState) -> bool {
        let now = state.notifications.now();
        !self
            .painted_at
            .is_some_and(|at| now.saturating_duration_since(at) >= INERT_AFTER_PAINT)
    }

    /// The oldest decidable item, optionally skipping deferred ones.
    fn oldest(&self, facts: &LogFacts, skip_deferred: bool) -> Option<DecisionKey> {
        self.items
            .iter()
            .find(|item| item.actionable(facts) && !(skip_deferred && item.deferred))
            .map(DecisionItem::key)
    }

    /// The oldest decidable, not-deferred item of `kind`.
    fn next_of_kind(&self, kind: DecisionKind, facts: &LogFacts) -> Option<DecisionKey> {
        self.items
            .iter()
            .find(|item| item.kind == kind && item.actionable(facts) && !item.deferred)
            .map(DecisionItem::key)
    }

    fn remove(&mut self, key: &DecisionKey) -> Option<DecisionItem> {
        let at = self.items.iter().position(|item| item.matches_key(key))?;
        *self.permission_layout.get_mut() = None;
        Some(self.items.remove(at))
    }

    /// Whether a draft for `conversation_id` can be answered where the user is.
    /// Unknown (no draw yet) counts as here: the facts of the next draw correct it.
    fn here(&self, conversation_id: Option<&str>) -> bool {
        match (conversation_id, self.active_conversation.as_deref()) {
            (Some(wanted), Some(active)) => wanted == active,
            _ => true,
        }
    }
}

/// The selected queue item is derived from the frame, including a suspended
/// continuation. Only the top frame is routed; this query never grants keys. A
/// stale item is shown on any tab (its Close must stay reachable); a live one
/// only where it can be decided.
#[must_use]
pub fn current<'a>(state: &'a TuiState, facts: &LogFacts) -> Option<&'a DecisionItem> {
    let key = state
        .modal_stack
        .all_frames()
        .find(|frame| frame.surface == Surface::Decision)?
        .decision
        .as_ref()?;
    state
        .decisions
        .items
        .iter()
        .find(|item| item.matches_key(key) && (item.is_stale() || item.actionable(facts)))
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

/// Hold a decision. An existing item with the same key is updated in place
/// (a producer's update is never a new arrival). The first item (0→1) of an
/// automatic arrival takes focus only when nothing protects the user's current
/// work and no deferral happened in the last minute; otherwise one Info toast
/// says it is waiting. An item for another conversation never takes focus.
pub fn raise(state: &mut TuiState, draft: DecisionDraft, arrival: Arrival) -> Raised {
    let now = state.notifications.now();
    let detail = clean(&draft.detail);
    let key = DecisionKey {
        kind: draft.kind,
        source: draft.source.clone(),
        conversation_id: draft.conversation_id.clone(),
    };
    let may_preempt = presentation::arrival_may_preempt(state, now);
    let here = state.decisions.here(draft.conversation_id.as_deref());
    let is_new = match state.decisions.item_mut(&key) {
        Some(item) => {
            item.detail = detail;
            item.subject = draft.subject;
            item.validity = Validity::Live;
            false
        }
        None => {
            state.decisions.items.push(DecisionItem {
                kind: draft.kind,
                source: draft.source,
                conversation_id: draft.conversation_id,
                summary: draft.summary,
                detail,
                subject: draft.subject,
                deferred: false,
                validity: Validity::Live,
                arrived: now,
                responder: draft.responder,
            });
            true
        }
    };
    state.needs_redraw = true;
    let take_focus = here
        && match arrival {
            Arrival::Requested => {
                !presentation::is_open(state, Surface::QuitConfirm)
                    && !presentation::typing_top(state)
            }
            Arrival::Automatic => may_preempt,
        };
    if take_focus {
        presentation::reducer::focus_decision(state, key);
        return Raised::Focused;
    }
    if is_new {
        notifications::info(state, NoticeSource::System, waiting_toast());
    }
    Raised::Waiting
}

/// Drop every item of `kind` (its condition cleared) whatever conversation it
/// belongs to: only for global conditions (the daily budget). Nothing is promoted
/// here; an item whose frame is open turns stale instead (A7).
pub fn withdraw(state: &mut TuiState, kind: DecisionKind) {
    withdraw_where(state, NO_LONGER_PENDING, |item| item.kind == kind);
}

/// The one item `key` names settled elsewhere: it leaves the queue, or, when its
/// frame is open, turns stale with `copy` (the source's own words) and waits for
/// Close. False when it was not queued.
pub(crate) fn withdraw_key(state: &mut TuiState, key: &DecisionKey, copy: StaleCopy) -> bool {
    let found = state.decisions.contains_key(key);
    withdraw_where(state, copy, |item| item.matches_key(key));
    found
}

/// Drop the `kind` item keyed to `conversation_id` only: clearing one tab's
/// condition never silently drops another tab's pending decision.
pub fn withdraw_for(state: &mut TuiState, kind: DecisionKind, conversation_id: &str) {
    withdraw_where(state, NO_LONGER_PENDING, |item| {
        item.kind == kind && item.conversation_id.as_deref() == Some(conversation_id)
    });
}

/// Drop every item keyed to a conversation that no longer exists (its tab was
/// closed or the conversation deleted): such an item could never be decided, yet
/// would keep `Needs you (N)` up and make the quit guard ask forever. Also
/// records which conversation the user is in. Prunes nothing without a tab list
/// to compare against.
pub fn prune_closed(state: &mut TuiState, facts: &LogFacts) {
    state.decisions.active_conversation = facts.active_conversation_id.clone();
    if facts.titles.is_empty() {
        return;
    }
    withdraw_where(state, NO_LONGER_PENDING, |item| {
        item.conversation_id.as_deref().is_some_and(|id| {
            Some(id) != facts.active_conversation_id.as_deref()
                && !facts.titles.iter().any(|(known, _)| known == id)
        })
    });
}

/// A withdrawn item leaves the queue, unless its frame is open: then the user is
/// reading it, so it turns stale (its source copy, `[Esc] Close` only) and Close
/// removes it.
fn withdraw_where(state: &mut TuiState, copy: StaleCopy, matches: impl Fn(&DecisionItem) -> bool) {
    let shown = state
        .modal_stack
        .all_frames()
        .find(|frame| frame.surface == Surface::Decision)
        .and_then(|frame| frame.decision.clone());
    let before = state.decisions.items.len();
    let mut staled = false;
    state.decisions.items.retain_mut(|item| {
        if !matches(item) {
            return true;
        }
        if shown.as_ref().is_some_and(|key| item.matches_key(key)) {
            staled |= !item.is_stale();
            item.validity = Validity::Stale(copy);
            return true;
        }
        false
    });
    if staled || state.decisions.items.len() != before {
        *state.decisions.permission_layout.get_mut() = None;
        state.needs_redraw = true;
    }
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

/// Hold a pasted image above the size limit for the user's confirmation: the
/// image is the item's payload, the warning its question (Requested: the user
/// just pasted it).
pub fn raise_large_image(
    state: &mut TuiState,
    attachment: crate::domain::models::ImageAttachment,
    warning: impl Into<String>,
) -> Raised {
    raise(
        state,
        DecisionDraft::new(DecisionKind::LargeImage, warning)
            .subject(DecisionSubject::LargeImage(attachment)),
        Arrival::Requested,
    )
}

/// The registry context of the focused decision frame (its item's own context, the
/// field's while one is open): the context hints, `Ctrl+X` and paste resolve in.
#[must_use]
pub(crate) fn frame_context(state: &TuiState) -> Context {
    state
        .modal_stack
        .all_frames()
        .find(|frame| frame.surface == Surface::Decision)
        .and_then(|frame| frame.decision.as_ref())
        .and_then(|key| state.decisions.item(key))
        .map_or(Context::Decision, keys::context_of)
}

/// Open the queued item `key` as a frame over whatever is open: an explicit,
/// Requested open (A10). False when the key is not queued, not decidable in this
/// conversation, or the top is the quit guard or a text field.
pub fn open(state: &mut TuiState, key: &DecisionKey) -> bool {
    let Some(item) = state.decisions.item(key) else {
        return false;
    };
    if !state.decisions.here(item.conversation_id.as_deref())
        || presentation::is_open(state, Surface::QuitConfirm)
        || presentation::typing_top(state)
    {
        return false;
    }
    presentation::reducer::focus_decision(state, key.clone());
    true
}

/// Hygiene both hosts run after an owner closes: a decision frame whose item left
/// the queue closes (nothing below it is ever opened by surprise, AD-2106).
pub fn settle(state: &mut TuiState) {
    let orphan = state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::Decision)
        && !state
            .modal_stack
            .all_frames()
            .find(|frame| frame.surface == Surface::Decision)
            .and_then(|frame| frame.decision.as_ref())
            .is_some_and(|key| state.decisions.contains_key(key));
    if orphan {
        presentation::reducer::close(state, Surface::Decision);
        state.needs_redraw = true;
    }
}

/// `Ctrl+X q` as both hosts run it: open the Needs-you list, or say honestly that
/// nothing waits.
pub fn open_waiting(state: &mut TuiState, facts: &LogFacts) {
    if open_list(state, facts) {
        return;
    }
    let message = keymap::table::decision_waiting_remedy();
    if let Some(origin) = state
        .modal_stack
        .top()
        .map(|frame| presentation::frame_context(state, frame.surface))
    {
        crate::adapters::tui::app::refuse_leader(
            state,
            origin,
            Some(CommandId::Leader(LeaderCmd::Decisions)),
            NoticeSource::System,
            message.into(),
        );
        state.current_hint = crate::adapters::tui::hints::contextual_hint(
            &state.focus,
            state.session_count,
            state.theme.timing.status_hint_fade_sessions,
            state.is_vscode,
            &state.modal_stack,
            state.pending_leader,
            false,
        );
    } else {
        notifications::info(state, NoticeSource::System, message);
    }
}

/// First paint of the focused pane: starts the 500 ms inert window.
pub fn note_painted(state: &mut TuiState) {
    if state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::Decision)
        && state.decisions.painted_at.is_none()
    {
        state.decisions.painted_at = Some(state.notifications.now());
        state.decisions.inert_released = false;
    }
}

/// Both hosts' tick: the frame was painted with muted keys while inert, so the
/// moment the window ends asks for the one redraw that activates them. True when
/// it asked.
pub fn tick(state: &mut TuiState) -> bool {
    let queue = &state.decisions;
    if queue.inert_released
        || queue.painted_at.is_none()
        || queue.inert(state)
        || state
            .modal_stack
            .top()
            .is_none_or(|frame| frame.surface != Surface::Decision)
    {
        return false;
    }
    state.decisions.inert_released = true;
    state.needs_redraw = true;
    true
}

/// Decisions waiting, for the `Needs you (N) ^X q` status cue.
#[must_use]
pub fn waiting(state: &TuiState) -> usize {
    state.decisions.len()
}

/// Key-owning surfaces may keep Input focus; card arrivals must wait for dismissal.
pub fn card_focus_blocked(state: &TuiState) -> bool {
    matches!(state.focus, FocusState::Overlay(_))
        || !state.modal_stack.is_empty()
        || state.pending_leader.is_some()
        || state.autocomplete.is_open()
        || state.reverse_search.is_open()
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

/// A read-only mirror's cue: it can see that approvals wait, never answer them (A14).
#[must_use]
pub fn mirror_cue(waiting: usize) -> Option<String> {
    (waiting > 0).then(|| format!("{waiting} approval(s) waiting for the writer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oldest decidable item as a frame (what `Ctrl+X q` + `Enter` do).
    fn focus_oldest(state: &mut TuiState, facts: &LogFacts) -> bool {
        let Some(key) = state.decisions.oldest(facts, false) else {
            return false;
        };
        presentation::reducer::focus_decision(state, key);
        true
    }
    use crate::adapters::tui::notifications::NotificationStore;
    use crate::adapters::tui::theme::Theme;
    use crate::adapters::tui::widgets::decision_frame;
    use crate::domain::clock::MockClock;
    use crate::domain::events::DomainInputEvent;
    use crate::domain::models::NotificationsConfig;
    use ratatui::prelude::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;

    /// Paint the focused frame into `area`: `Some(area)` when it really painted.
    fn render(
        frame: &mut Frame,
        area: Rect,
        state: &TuiState,
        facts: &LogFacts,
        theme: &Theme,
    ) -> Option<Rect> {
        decision_frame::render(frame, area, state, facts, theme, "Decision: test", None)
            .then_some(area)
    }

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
                render(frame, area, state, f, &theme);
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
                    || !presentation::is_open(&state, Surface::Decision)
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
                    presentation::is_open(&state, Surface::Decision),
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
                    painted =
                        render(frame, Rect::new(0, 0, width, height), &state, &f, &theme).is_some();
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
                painted = render(frame, Rect::new(0, 0, 60, 12), &state, &f, &theme).is_some();
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
        // Only the same kind promotes (AD-2106): two carryover questions of two sources.
        for (source, arrival) in [("one", Arrival::Requested), ("two", Arrival::Automatic)] {
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::Carryover, "q").source(source),
                arrival,
            );
        }
        ready(&mut state, &clock);
        let y = PaneKey::Char('y');
        assert_eq!(
            handle_key(&mut state, y, &f),
            PaneOutcome::Effect(DecisionEffect::CarryoverAccept)
        );
        // Promoted but unpainted: the immediate second `y` is dropped.
        assert_eq!(handle_key(&mut state, y, &f), PaneOutcome::Consumed);
        assert_eq!(state.decisions.count(DecisionKind::Carryover), 1);
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, y, &f),
            PaneOutcome::Effect(DecisionEffect::CarryoverAccept)
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
            current(&state, &f).map(|i| i.kind),
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
            current(&state, &f).map(|i| i.kind),
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
        // The first arrival took the frame; the user sets it aside (an OPEN item
        // that is withdrawn turns stale instead of vanishing: see the stale tests).
        presentation::reducer::close(&mut state, Surface::Decision);
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
        presentation::reducer::close(&mut state, Surface::Decision);
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
        let (mut state, _clock) = clocked();
        prune_closed(&mut state, &facts("active"));
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Carryover, "x").conversation("other"),
            Arrival::Requested,
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        let event = DomainInputEvent::KeyPress('y');
        assert_eq!(
            handle_domain_event(&mut state, &event, &facts("active"), keymap::Host::Local),
            None
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert!(
            state.decisions.holds(DecisionKind::Carryover),
            "the item still waits"
        );
        // The list includes the passive item, but Enter cannot open it here.
        open_waiting(&mut state, &facts("active"));
        assert!(presentation::is_open(&state, Surface::DecisionQueue));
        list::handle_list_key(
            &mut state,
            keymap::Host::Local,
            keymap::Key::Enter,
            &facts("active"),
        );
        assert!(presentation::is_open(&state, Surface::DecisionQueue));
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert!(state.decisions.holds(DecisionKind::Carryover));
        // On its own tab Enter opens that same queued item.
        list::handle_list_key(
            &mut state,
            keymap::Host::Local,
            keymap::Key::Enter,
            &facts("other"),
        );
        assert!(presentation::is_open(&state, Surface::Decision));
    }

    #[test]
    fn story21_6_answering_never_promotes_another_kind_and_a_deferred_item_is_never_reopened() {
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
        // Answer the budget from the base: nothing of its kind waits, so the frame
        // closes and neither the image nor the delete is opened by surprise.
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &LogFacts::default()),
            PaneOutcome::Effect(DecisionEffect::BudgetContinue)
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(current(&state, &LogFacts::default()), None);
        assert_eq!(state.decisions.len(), 2);
        // `Ctrl+X q` focuses the oldest decidable item: FIFO, the image.
        assert!(focus_oldest(&mut state, &LogFacts::default()));
        assert_eq!(
            current(&state, &LogFacts::default()).map(|i| i.kind),
            Some(DecisionKind::LargeImage)
        );
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Esc, &LogFacts::default()),
            PaneOutcome::Consumed,
            "Esc defers the image"
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(current(&state, &LogFacts::default()), None);
        assert!(focus_oldest(&mut state, &LogFacts::default()));
        assert_eq!(
            current(&state, &LogFacts::default()).map(|i| i.kind),
            Some(DecisionKind::LargeImage),
            "Ctrl+X q focuses the oldest, deferred or not"
        );
    }

    #[test]
    fn story21_4_ctrl_c_backs_without_deciding_and_ctrl_q_still_guards() {
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
            PaneOutcome::Consumed
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert!(state.decisions.holds(DecisionKind::Recovery));
        assert!(focus_oldest(&mut state, &LogFacts::default()));
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
            current(&state, &f).map(|i| i.kind),
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
        // (focus, can answer, user mid-sentence) -> (focus after, moved)
        let cases = [
            (FocusState::Input, true, false, FocusState::Chat, true),
            (FocusState::Input, true, true, FocusState::Input, false),
            (FocusState::Chat, true, false, FocusState::Chat, false),
            (FocusState::Input, false, false, FocusState::Input, false),
        ];
        for (focus, can_answer, typing, want, moved) in cases {
            let before = focus.clone();
            let mut state = TuiState::new(80, 24);
            state.focus = focus;
            if typing {
                state.input_buffer = "draft".into();
            }
            assert_eq!(
                presentation::reducer::take_chat_focus_for_card(&mut state, can_answer),
                moved,
                "{before:?} answer={can_answer} typing={typing}"
            );
            assert_eq!(
                state.focus, want,
                "{before:?} answer={can_answer} typing={typing}"
            );
            assert_eq!(
                state.card_focus_borrowed_from,
                moved.then_some(FocusState::Input)
            );
        }
        // The local host reads the composer from the state and asks for a redraw.
        let mut state = TuiState::new(80, 24);
        state.needs_redraw = false;
        presentation::reducer::take_chat_focus_for_card(&mut state, true);
        assert!(state.focus == FocusState::Chat && state.needs_redraw);
        let mut typing = TuiState::new(80, 24);
        typing.input_buffer = "draft".into();
        typing.needs_redraw = false;
        presentation::reducer::take_chat_focus_for_card(&mut typing, true);
        assert!(typing.focus == FocusState::Input && !typing.needs_redraw);
        for surface in [Surface::ModelSelector, Surface::Search] {
            let mut framed = TuiState::new(80, 24);
            presentation::reducer::open(&mut framed, surface);
            let frame = framed.modal_stack.top().cloned();
            presentation::reducer::take_chat_focus_for_card(&mut framed, true);
            assert_eq!(framed.focus, FocusState::Input);
            assert_eq!(framed.modal_stack.top(), frame.as_ref());
        }
    }

    #[test]
    fn story21_4_pane_model_effect_keeps_its_interrupted_caller() {
        use crate::adapters::tui::app::handle_input;
        use crate::domain::events::DomainKey;
        // The migrated model destination is available from base and nested panes.
        for (kind, key, effect) in [(
            DecisionKind::Budget,
            's',
            DecisionEffect::BudgetSwitchCheaper,
        )] {
            for nested in [false, true] {
                let (mut state, clock) = clocked();
                state.focus = FocusState::Chat;
                state.input_buffer = "untouched draft".into();
                if nested {
                    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
                    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
                }
                raise(
                    &mut state,
                    DecisionDraft::new(kind, "pending"),
                    Arrival::Requested,
                );
                ready(&mut state, &clock);
                let caller: Vec<_> = state
                    .modal_stack
                    .iter()
                    .take(usize::from(nested))
                    .cloned()
                    .collect();
                let notices = state.notifications.records().len();
                let action = handle_input(&mut state, &DomainInputEvent::KeyPress(key));
                assert_eq!(action, effect.into_action());
                assert_eq!(
                    state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    caller
                );
                assert!(state.decisions.is_empty());
                assert_eq!(
                    state.notifications.records().len(),
                    notices,
                    "no refusal toast"
                );
                assert_eq!(state.input_buffer, "untouched draft");
                assert_eq!(state.focus, FocusState::Chat);
            }
        }
    }

    fn painted(state: &TuiState, f: &LogFacts) -> String {
        draw(state, f, 80, 24)
    }

    #[test]
    fn story21_6_a_withdrawn_item_whose_frame_is_open_turns_stale_and_close_removes_it() {
        let (mut state, clock) = clocked();
        let f = LogFacts::default();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "Daily budget ($10.00) reached."),
            Arrival::Automatic,
        );
        assert!(presentation::is_open(&state, Surface::Decision));
        ready(&mut state, &clock);
        withdraw(&mut state, DecisionKind::Budget);
        let item = state.decisions.items().next().expect("the open item stays");
        assert_eq!(item.validity, Validity::Stale(NO_LONGER_PENDING));
        assert_eq!(waiting(&state), 1, "a stale item still counts until closed");
        let text = painted(&state, &f);
        assert!(
            text.contains("This decision is no longer pending."),
            "{text}"
        );
        assert!(text.contains("[Esc] Close"), "{text}");
        assert!(
            !text.contains("Pause until tomorrow"),
            "no mutation key is offered: {text}"
        );
        for key in ['y', 's', 'p', 'n', 'k'] {
            assert_eq!(
                handle_key(&mut state, PaneKey::Char(key), &f),
                PaneOutcome::Consumed,
                "{key} acts on nothing"
            );
        }
        assert_eq!(
            handle_key(&mut state, PaneKey::Enter, &f),
            PaneOutcome::Consumed
        );
        assert_eq!(waiting(&state), 1);
        assert!(presentation::is_open(&state, Surface::Decision));
        // Close removes it; it was never deferred, so no cooldown starts.
        assert_eq!(
            handle_key(&mut state, PaneKey::Esc, &f),
            PaneOutcome::Consumed
        );
        assert!(state.decisions.is_empty());
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(state.decisions.last_defer_at(), None);
        assert!(presentation::consistent(&state));
    }

    #[test]
    fn story21_6_a_vanished_conversation_turns_an_open_item_stale_on_any_tab() {
        let (mut state, _clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::ContextWarning, "low").conversation("c1"),
            Arrival::Automatic,
        );
        assert!(presentation::is_open(&state, Surface::Decision));
        let gone = LogFacts {
            titles: vec![("c2".into(), "two".into())],
            ..facts("c2")
        };
        prune_closed(&mut state, &gone);
        assert!(current(&state, &gone).is_some_and(DecisionItem::is_stale));
        assert_eq!(
            handle_key(&mut state, PaneKey::CtrlC, &gone),
            PaneOutcome::Consumed
        );
        assert!(state.decisions.is_empty());
        assert!(!presentation::is_open(&state, Surface::Decision));
    }

    #[test]
    fn story21_6_an_item_for_another_conversation_waits_and_never_takes_focus() {
        let (mut state, _clock) = clocked();
        prune_closed(&mut state, &facts("a"));
        for arrival in [Arrival::Automatic, Arrival::Requested] {
            assert_eq!(
                raise(
                    &mut state,
                    DecisionDraft::new(DecisionKind::ContextWarning, "low").conversation("b"),
                    arrival,
                ),
                Raised::Waiting
            );
            assert!(!presentation::is_open(&state, Surface::Decision));
        }
        assert_eq!(state.decisions.len(), 1);
        let toasts = state
            .notifications
            .visible()
            .filter(|r| r.message == waiting_toast())
            .count();
        assert_eq!(toasts, 1, "one waiting toast per new item");
        // Positive control: the same arrival for the active conversation takes the frame.
        assert_eq!(
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::Carryover, "x").conversation("a"),
                Arrival::Requested,
            ),
            Raised::Focused
        );
        assert_eq!(
            current(&state, &facts("a")).map(|i| i.kind),
            Some(DecisionKind::Carryover)
        );
    }

    #[test]
    fn story21_6_a_requested_decision_queues_behind_a_text_field_but_opens_over_a_reader() {
        let (mut state, _clock) = clocked();
        presentation::reducer::open(&mut state, Surface::CommandPalette);
        assert_eq!(
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::DeleteConversations, "d"),
                Arrival::Requested,
            ),
            Raised::Waiting
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(waiting(&state), 1);
        let (mut state, _clock) = clocked();
        presentation::reducer::open(&mut state, Surface::Help);
        assert_eq!(
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::DeleteConversations, "d"),
                Arrival::Requested,
            ),
            Raised::Focused
        );
        assert_eq!(state.modal_stack.len(), 2);
    }

    #[test]
    fn story21_6_a_resolve_or_defer_restores_the_interrupted_frame_exactly() {
        for answer in [PaneKey::Char('y'), PaneKey::Esc] {
            let (mut state, clock) = clocked();
            presentation::reducer::open(&mut state, Surface::Help);
            state.help_overlay.scroll_offset = 7;
            let help = state.modal_stack.top().cloned();
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::Budget, "b"),
                Arrival::Requested,
            );
            assert_eq!(state.modal_stack.len(), 2);
            ready(&mut state, &clock);
            handle_key(&mut state, answer, &LogFacts::default());
            assert_eq!(state.modal_stack.top().cloned(), help, "{answer:?}");
            assert_eq!(state.help_overlay.scroll_offset, 7, "{answer:?}");
            assert!(presentation::consistent(&state));
        }
    }

    #[test]
    fn story21_6_only_an_answer_from_the_base_opens_the_next_item_of_the_same_kind() {
        let f = LogFacts::default();
        let two = |state: &mut TuiState| {
            for (source, arrival) in [("a", Arrival::Requested), ("b", Arrival::Automatic)] {
                raise(
                    state,
                    DecisionDraft::new(DecisionKind::Carryover, "q").source(source),
                    arrival,
                );
            }
        };
        // Nothing beneath: answering `a` opens `b` at once (the legacy sequence).
        let (mut state, clock) = clocked();
        two(&mut state);
        ready(&mut state, &clock);
        handle_key(&mut state, PaneKey::Char('y'), &f);
        assert_eq!(current(&state, &f).map(|i| i.source.as_str()), Some("b"));
        // Deferring `b` never opens anything.
        ready(&mut state, &clock);
        handle_key(&mut state, PaneKey::Esc, &f);
        assert_eq!(current(&state, &f), None);
        // A frame beneath: AD-2106 restore wins, `b` waits.
        let (mut state, clock) = clocked();
        presentation::reducer::open(&mut state, Surface::Help);
        two(&mut state);
        ready(&mut state, &clock);
        handle_key(&mut state, PaneKey::Char('y'), &f);
        assert_eq!(current(&state, &f), None);
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.surface),
            Some(Surface::Help)
        );
        assert_eq!(state.decisions.count(DecisionKind::Carryover), 1);
    }

    #[test]
    fn story21_6_the_tick_asks_for_one_redraw_when_the_inert_window_ends() {
        let (mut state, clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Requested,
        );
        assert!(!tick(&mut state), "unpainted: nothing to release");
        note_painted(&mut state);
        assert!(!tick(&mut state), "still inert");
        clock.advance(INERT_AFTER_PAINT);
        state.needs_redraw = false;
        assert!(tick(&mut state));
        assert!(state.needs_redraw);
        assert!(!tick(&mut state), "announced once per painted frame");
    }

    #[test]
    fn story21_6_page_keys_scroll_the_body_and_never_decide() {
        let (mut state, clock) = clocked();
        let f = LogFacts::default();
        let body: Vec<String> = (0..30).map(|n| format!("row-{n:02}")).collect();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::LargeImage, body.join("\n")),
            Arrival::Requested,
        );
        // Never painted, so keys are inert: scrolling is allowed all the same.
        assert!(painted(&state, &f).contains("row-00"));
        assert_eq!(
            handle_key(&mut state, PaneKey::PageDown, &f),
            PaneOutcome::Consumed
        );
        let text = painted(&state, &f);
        assert!(
            text.contains("row-05") && !text.contains("row-00"),
            "{text}"
        );
        for _ in 0..20 {
            handle_key(&mut state, PaneKey::PageDown, &f);
        }
        assert!(painted(&state, &f).contains("row-29"));
        for _ in 0..20 {
            handle_key(&mut state, PaneKey::PageUp, &f);
        }
        assert!(painted(&state, &f).contains("row-00"));
        assert!(state.decisions.holds(DecisionKind::LargeImage));
        ready(&mut state, &clock);
    }

    #[test]
    fn story21_6_a_large_image_travels_as_the_items_payload_and_is_attached_once() {
        use crate::domain::models::ImageAttachment;
        let (mut state, clock) = clocked();
        let f = LogFacts::default();
        let image = ImageAttachment {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        };
        raise_large_image(&mut state, image.clone(), "Image is large (6 MB).");
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Effect(DecisionEffect::AttachLargeImage)
        );
        assert!(
            state.pending_images.is_empty(),
            "the effect arm attaches, not the key"
        );
        apply_resolved_action(
            &mut state,
            InputAction::ImageConfirmAttach,
            presentation::reducer::restore_focus_after_modal,
        );
        assert_eq!(state.pending_images, vec![image]);
        assert!(state.image_indicator.is_some());
        assert!(state.decisions.is_empty());
        // Cancel drops the payload and attaches nothing.
        raise_large_image(
            &mut state,
            ImageAttachment {
                media_type: "image/png".into(),
                data: "BBBB".into(),
            },
            "Image is large (6 MB).",
        );
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('n'), &f),
            PaneOutcome::Effect(DecisionEffect::CancelLargeImage)
        );
        apply_resolved_action(
            &mut state,
            InputAction::ImageConfirmCancel,
            presentation::reducer::restore_focus_after_modal,
        );
        assert_eq!(state.pending_images.len(), 1);
        assert_eq!(state.decisions.take_resolved(), None);
    }

    #[test]
    fn story21_6_the_key_is_kind_source_and_conversation_and_an_update_never_re_arrives() {
        let (mut state, _clock) = clocked();
        for text in ["one", "two"] {
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::Budget, text),
                Arrival::Automatic,
            );
        }
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions.items().next().unwrap().detail, "two");
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "other source").source("s2"),
            Arrival::Automatic,
        );
        assert_eq!(state.decisions.len(), 2, "the source is part of the key");
        let toasts = state
            .notifications
            .visible()
            .filter(|r| r.message == waiting_toast())
            .count();
        assert_eq!(toasts, 1, "the update never announced itself again");
    }

    #[test]
    fn story21_6_open_by_key_opens_a_queued_item_unless_a_text_field_or_quit_owns_the_top() {
        let (mut state, _clock) = clocked();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "b"),
            Arrival::Automatic,
        );
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::LargeImage, "i"),
            Arrival::Automatic,
        );
        let image = state.decisions.items().nth(1).unwrap().key();
        assert!(open(&mut state, &image));
        assert_eq!(
            current(&state, &LogFacts::default()).map(|i| i.kind),
            Some(DecisionKind::LargeImage)
        );
        let unknown = DecisionKey {
            kind: DecisionKind::Recovery,
            source: String::new(),
            conversation_id: None,
        };
        assert!(!open(&mut state, &unknown));
        presentation::reducer::open(&mut state, Surface::CommandPalette);
        let budget = state.decisions.items().next().unwrap().key();
        assert!(
            !open(&mut state, &budget),
            "a text field keeps the keyboard"
        );
    }

    #[test]
    fn story21_6_esc_defers_inside_the_inert_window_but_every_deciding_key_stays_inert() {
        let f = LogFacts::default();
        for kind in [
            DecisionKind::Carryover,
            DecisionKind::Budget,
            DecisionKind::ContextWarning,
            DecisionKind::LargeImage,
        ] {
            let (mut state, clock) = clocked();
            raise(
                &mut state,
                DecisionDraft::new(kind, "q"),
                Arrival::Requested,
            );
            note_painted(&mut state);
            clock.advance(Duration::from_millis(499));
            // A deciding rail key is still dropped at 499 ms ...
            for key in ['y', 'n', 's', 'p', 'k'] {
                assert_eq!(
                    handle_key(&mut state, PaneKey::Char(key), &f),
                    PaneOutcome::Consumed
                );
                assert!(
                    state.decisions.holds(kind),
                    "{kind:?}: `{key}` inert at 499 ms"
                );
            }
            assert!(presentation::is_open(&state, Surface::Decision));
            // ... but Esc sets it aside at once: counted, deferred, frame gone.
            assert_eq!(
                handle_key(&mut state, PaneKey::Esc, &f),
                PaneOutcome::Consumed
            );
            assert!(
                !presentation::is_open(&state, Surface::Decision),
                "{kind:?}"
            );
            assert_eq!(waiting(&state), 1, "{kind:?}: still counted");
            assert!(state.decisions.items().next().unwrap().deferred, "{kind:?}");
        }
        // The resolving Esc policies wait for the window like any decision key.
        for kind in [DecisionKind::Recovery, DecisionKind::DeleteConversations] {
            let (mut state, clock) = clocked();
            raise(
                &mut state,
                DecisionDraft::new(kind, "q"),
                Arrival::Requested,
            );
            note_painted(&mut state);
            clock.advance(Duration::from_millis(499));
            assert_eq!(
                handle_key(&mut state, PaneKey::Esc, &f),
                PaneOutcome::Consumed
            );
            assert!(state.decisions.holds(kind), "{kind:?}: Esc inert at 499 ms");
            clock.advance(Duration::from_millis(2));
            assert!(matches!(
                handle_key(&mut state, PaneKey::Esc, &f),
                PaneOutcome::Effect(_)
            ));
        }
    }

    #[test]
    fn story21_6_a_scrolled_body_survives_a_width_change_in_both_directions() {
        let words: Vec<String> = (0..200).map(|n| format!("w{n:03}")).collect();
        let detail = words.join(" ");
        // Narrow (60 columns): scroll to the end, then widen to 130 where far fewer rows wrap.
        let (mut state, _clock) = clocked();
        state.terminal_width = 60;
        let f = LogFacts::default();
        raise(
            &mut state,
            DecisionDraft::new(DecisionKind::LargeImage, detail.clone()),
            Arrival::Requested,
        );
        for _ in 0..10 {
            handle_key(&mut state, PaneKey::PageDown, &f);
        }
        assert!(
            draw(&state, &f, 60, 24).contains("w199"),
            "narrow: the end shows"
        );
        state.terminal_width = 130;
        let wide = draw(&state, &f, 130, 30);
        assert!(
            wide.contains("w199"),
            "widened: the body is never skipped away\n{wide}"
        );
        assert!(wide.contains("[y] Attach anyway"), "the rail stays");
        // A later PgUp starts from the clamped offset, not the stale one, and reaches the top.
        for _ in 0..10 {
            handle_key(&mut state, PaneKey::PageUp, &f);
        }
        assert!(draw(&state, &f, 130, 30).contains("w000"));
        // Wide to narrow: an offset kept at 130 columns still shows rows at 60.
        for _ in 0..3 {
            handle_key(&mut state, PaneKey::PageDown, &f);
        }
        state.terminal_width = 60;
        let narrow = draw(&state, &f, 60, 24);
        assert!(
            narrow.contains("w1") && narrow.contains("[y] Attach anyway"),
            "{narrow}"
        );
    }
}
