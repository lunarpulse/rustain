//! The sole owner of migrated frame/payload lifetimes and focus restoration.

use super::stack::Continuation;
use super::{Frame, Opened, Surface, is_open};
use crate::adapters::tui::decision_pane::DecisionKey;
use crate::adapters::tui::notification_log::NotificationLogState;
use crate::adapters::tui::quit_confirm::{QuitConfirmState, QuitFacts, QuitRequest};
use crate::adapters::tui::state::{LoginFrameIntent, TuiState};
use crate::domain::models::visual::ConfirmationType;
use crate::domain::models::{FocusState, OverlayType};

/// Every host, including a read-only attachment, starts at the shared composer.
pub fn initial_base() -> FocusState {
    FocusState::Input
}

/// Runtime effects may change an empty stack's base, never a retained caller.
/// Sidebar and legacy confirmations remain base slots until their owner stories.
pub fn set_base(state: &mut TuiState, focus: FocusState) {
    if may_set_base(state, &focus) && state.focus != focus {
        state.focus = focus;
        refresh_hint(state);
    }
}

/// Capture a recipient/card return slot without borrowing the focus field.
pub fn replace_base(state: &mut TuiState, focus: FocusState) -> FocusState {
    if may_set_base(state, &focus) {
        let previous = std::mem::replace(&mut state.focus, focus);
        refresh_hint(state);
        previous
    } else {
        state.focus.clone()
    }
}

pub fn update_sidebar_selection(state: &mut TuiState, selected: usize) {
    if state.modal_stack.is_empty()
        && let FocusState::Sidebar {
            selected: current, ..
        } = &mut state.focus
    {
        *current = selected;
    }
}

fn may_set_base(state: &TuiState, focus: &FocusState) -> bool {
    state.modal_stack.is_empty()
        && (matches!(focus, FocusState::Overlay(_)) || !current_confirmation_pending(state))
}

fn higher_owner(state: &TuiState) -> bool {
    !state.modal_stack.is_empty()
        || state.pending_leader.is_some()
        || state.autocomplete.is_open()
        || state.reverse_search.is_open()
}

fn confirmation_pending(state: &TuiState, kind: &ConfirmationType) -> bool {
    match kind {
        ConfirmationType::Permission => state.pending_permission.is_some(),
        ConfirmationType::PermissionFeedback => state.pending_feedback_input.is_some(),
        ConfirmationType::Question => state.ask_user_question.is_some(),
        ConfirmationType::Fork => state.pending_fork_index.is_some(),
        ConfirmationType::Rewind => state.pending_rewind_index.is_some(),
        ConfirmationType::ExportOverwrite(path) => state
            .pending_export
            .as_ref()
            .is_some_and(|(pending, _)| pending == path),
        ConfirmationType::SkillTrust | ConfirmationType::SkillTrustInspect => {
            state.pending_skill_trust.is_some() || state.pending_activation.is_some()
        }
        ConfirmationType::PlanApproval => state.pending_plan_approval.is_some(),
        ConfirmationType::ArtifactApply => state.pending_artifact_card.is_some(),
        ConfirmationType::PeerAdd => state.pending_peer_add.is_some(),
        ConfirmationType::TeamRetract => state.pending_team_retract.is_some(),
    }
}

fn current_confirmation_pending(state: &TuiState) -> bool {
    matches!(
        &state.focus,
        FocusState::Overlay(OverlayType::Confirmation(kind)) if confirmation_pending(state, kind)
    )
}

/// The real payload/queue remains pending beneath any higher keyboard owner.
pub fn raise_confirmation(state: &mut TuiState, confirmation: ConfirmationType) {
    if !confirmation_pending(state, &confirmation) {
        return;
    }
    if matches!(&confirmation, ConfirmationType::Permission)
        && another_confirmation_owns_focus(state)
    {
        return;
    }
    if higher_owner(state) {
        state.modal_stack.pending_confirmation = Some(confirmation);
    } else {
        set_base(
            state,
            FocusState::Overlay(OverlayType::Confirmation(confirmation)),
        );
    }
}

fn another_confirmation_owns_focus(state: &TuiState) -> bool {
    matches!(state.focus, FocusState::Overlay(_))
        && !matches!(
            state.focus,
            FocusState::Overlay(OverlayType::Confirmation(
                ConfirmationType::Permission | ConfirmationType::PermissionFeedback
            ))
        )
}

/// A generic permission must wait for all existing higher legacy owners too.
pub(crate) fn permission_modal_owns_focus(state: &TuiState) -> bool {
    higher_owner(state)
        || state.pending_plan_card.is_some()
        || state.pending_delegation_card.is_some()
        || state.pending_consolidation_card.is_some()
        || state.pending_forget_card.is_some()
        || state.task_panel_state.pending_deviation.is_some()
        || state.task_panel_state.cancel_plan_confirm.is_some()
        || state.task_panel_state.skip_cascade_pending.is_some()
        || state.pending_skill_trust.is_some()
        || state.pending_activation.is_some()
        || state.pending_plan_approval.is_some()
        || state.pending_spawn_gate.is_some()
        || state.pending_large_image.is_some()
        || state.pending_artifact_card.is_some()
        || state.pending_feedback_input.is_some()
        || state.tool_block_states.values().any(|tbs| tbs.peek_active)
        || another_confirmation_owns_focus(state)
}

fn pending_confirmation(state: &TuiState) -> Option<ConfirmationType> {
    if state.pending_feedback_input.is_some() {
        Some(ConfirmationType::PermissionFeedback)
    } else if state.pending_permission.is_some() {
        Some(ConfirmationType::Permission)
    } else if state.pending_skill_trust.is_some() || state.pending_activation.is_some() {
        Some(if state.skill_trust_inspect_mode {
            ConfirmationType::SkillTrustInspect
        } else {
            ConfirmationType::SkillTrust
        })
    } else if state.pending_artifact_card.is_some() {
        Some(ConfirmationType::ArtifactApply)
    } else if state.pending_plan_approval.is_some() {
        Some(ConfirmationType::PlanApproval)
    } else if state.pending_peer_add.is_some() {
        Some(ConfirmationType::PeerAdd)
    } else if state.pending_team_retract.is_some() {
        Some(ConfirmationType::TeamRetract)
    } else if state.ask_user_question.is_some() {
        Some(ConfirmationType::Question)
    } else if state.pending_fork_index.is_some() {
        Some(ConfirmationType::Fork)
    } else if state.pending_rewind_index.is_some() {
        Some(ConfirmationType::Rewind)
    } else {
        // A different deferred kind may replace the intent, never the live payload.
        // Legacy focus owns a separate target, so recovery then needs this copy.
        state
            .pending_export
            .as_ref()
            .map(|(path, _)| ConfirmationType::ExportOverwrite(path.clone()))
    }
}

fn settle_legacy_inner(state: &mut TuiState) -> bool {
    if state
        .modal_stack
        .pending_confirmation
        .as_ref()
        .is_some_and(|kind| !confirmation_pending(state, kind))
    {
        state.modal_stack.pending_confirmation = None;
    }
    if higher_owner(state) || current_confirmation_pending(state) {
        return false;
    }
    // Release the former request's exact-input cache before promoting any owner,
    // even when there is no next queued permission or Resolved has not arrived.
    let permission_closed = matches!(
        state.focus,
        FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::Permission | ConfirmationType::PermissionFeedback
        ))
    );
    if permission_closed {
        state.permission_reader = Default::default();
    }
    let next = state
        .modal_stack
        .pending_confirmation
        .take()
        .or_else(|| pending_confirmation(state));
    if let Some(kind) = next {
        state.focus = FocusState::Overlay(OverlayType::Confirmation(kind));
        return true;
    }
    let mut changed = false;
    if matches!(state.focus, FocusState::Overlay(_)) {
        state.focus = if state.recipient_surface.is_some() {
            FocusState::Chat
        } else {
            FocusState::Input
        };
        changed = true;
    }
    if !permission_modal_owns_focus(state)
        && let Some(next) = state.permission_queue.pop()
    {
        if !permission_closed {
            state.permission_reader = Default::default();
        }
        state.pending_permission = Some(next);
        state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));
        changed = true;
    }
    changed
}

/// Run after real keys, async resolutions and frame ownership transitions.
pub fn settle_legacy(state: &mut TuiState) {
    if settle_legacy_inner(state) {
        refresh_hint(state);
    }
}

pub fn restore_focus_after_modal(state: &mut TuiState) {
    let focus = if state.pending_feedback_input.is_some() {
        FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::PermissionFeedback,
        ))
    } else if state.pending_permission.is_some() {
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission))
    } else if state.recipient_surface.is_some() {
        FocusState::Chat
    } else {
        FocusState::Input
    };
    set_base(state, focus);
    settle_legacy(state);
}

pub fn advance_skill_trust_queue(state: &mut TuiState) {
    if let Some(next) = state.skill_trust_queue.pop_front() {
        state.pending_skill_trust = Some(next);
        state.skill_trust_inspect_mode = false;
        raise_confirmation(state, ConfirmationType::SkillTrust);
    } else {
        state.pending_skill_trust = None;
        state.skill_trust_inspect_mode = false;
        restore_focus_after_modal(state);
    }
    state.needs_redraw = true;
}

/// The shared composer fact is authoritative in both hosts.
pub fn take_chat_focus_for_card(state: &mut TuiState, can_answer: bool) -> bool {
    let moves = can_answer
        && state.input_buffer.is_empty()
        && state.focus == FocusState::Input
        && !crate::adapters::tui::decision_pane::card_focus_blocked(state);
    if moves {
        state
            .card_focus_borrowed_from
            .get_or_insert(FocusState::Input);
        set_base(state, FocusState::Chat);
    }
    moves
}

/// Explicit user navigation relinquishes a borrow; closing the final card returns it.
pub fn settle_borrowed_card_focus(state: &mut TuiState, card_pending: bool) {
    if state.card_focus_borrowed_from.is_none() {
        return;
    }
    if state.focus != FocusState::Chat
        || crate::adapters::tui::decision_pane::card_focus_blocked(state)
        || state.recipient_surface.is_some()
    {
        state.card_focus_borrowed_from = None;
    } else if !card_pending {
        let previous = state.card_focus_borrowed_from.take().unwrap();
        set_base(state, previous);
    }
}

/// Reopen truncates to the existing frame and retains its untouched payload.
pub fn open(state: &mut TuiState, surface: Surface) -> Opened {
    assert!(
        surface != Surface::DecisionPane,
        "pane opens with its queue identity"
    );
    open_frame(state, surface, None)
}

/// Consume the payload preparation owed by a fresh frame, including a palette
/// handoff whose effect is applied after key dispatch. Reopening never resets it.
pub fn initialize_surface(state: &mut TuiState, surface: Surface) -> bool {
    state
        .modal_stack
        .all_frames_mut()
        .find(|frame| frame.surface == surface)
        .is_some_and(|frame| std::mem::take(&mut frame.needs_initialization))
}

/// Bind a freshly initialized Stage owner to its actual conversation.
pub fn bind_stage(state: &mut TuiState, surface: Surface, conversation_id: &str) {
    if let Some(frame) = state
        .modal_stack
        .all_frames_mut()
        .find(|frame| frame.surface == surface)
    {
        frame.conversation_id = Some(conversation_id.to_owned());
    }
}

/// Applies trusted Login requests before deferred permission focus is settled.
pub fn settle_login(state: &mut TuiState) -> bool {
    let Some(intent) = state.login.frame_intent.take() else {
        return true;
    };
    match intent {
        LoginFrameIntent::Open => open(state, Surface::Login),
        LoginFrameIntent::FromPalette => handoff(state, Surface::CommandPalette, Surface::Login),
        LoginFrameIntent::FromModel => handoff(state, Surface::ModelSelector, Surface::Login),
        LoginFrameIntent::Refresh => handoff(state, Surface::Login, Surface::Login),
        LoginFrameIntent::Close => {
            let was_open = is_open(state, Surface::Login);
            close(state, Surface::Login);
            return was_open;
        }
        LoginFrameIntent::ReturnToModel => {
            state.login.frame_intent = Some(intent);
            return true;
        }
    };
    initialize_surface(state, Surface::Login);
    true
}

/// Replace a surface, not its caller. Async completion beneath another frame
/// preserves that higher frame (including a suspended source at the depth cap).
pub fn handoff(state: &mut TuiState, from: Surface, to: Surface) -> Opened {
    if from == to && is_open(state, from) {
        return Opened::Unchanged;
    }
    if !is_open(state, from) || state.modal_stack.top().is_some_and(|f| f.surface == from) {
        let removed = remove_surface(state, from);
        let opened = open(state, to);
        if removed && opened == Opened::Unchanged {
            transition(state);
        }
        return opened;
    }
    if is_open(state, to) {
        return reopen_for_handoff(state, from, to);
    }
    let continuation = take_continuation(state, from);
    release_payload(state, from, false);
    prepare_payload(state, to);
    let replacement = fresh_frame(state, to, None);
    replace_frame(state, from, replacement);
    if let Some(frame) = continuation {
        if to.keeps_continuation() {
            state
                .modal_stack
                .continuations
                .push(Continuation { owner: to, frame });
        } else {
            discard(state, frame.surface);
        }
    }
    transition(state);
    debug_assert!(super::consistent(state));
    Opened::Replaced
}

fn replace_frame(state: &mut TuiState, surface: Surface, replacement: Frame) {
    *state
        .modal_stack
        .all_frames_mut()
        .find(|frame| frame.surface == surface)
        .expect("owned surface") = replacement;
}

/// A continuation is below its owner, even when both occupy one live slot.
fn owned_position(state: &TuiState, mut surface: Surface) -> (usize, usize) {
    let mut depth = 0;
    loop {
        if let Some(index) = state
            .modal_stack
            .iter()
            .position(|frame| frame.surface == surface)
        {
            return (index, depth);
        }
        surface = state
            .modal_stack
            .continuations
            .iter()
            .find(|c| c.frame.surface == surface)
            .expect("owned surface")
            .owner;
        depth += 1;
    }
}

fn reopen_for_handoff(state: &mut TuiState, from: Surface, to: Surface) -> Opened {
    let (source_index, source_depth) = owned_position(state, from);
    let (target_index, target_depth) = owned_position(state, to);
    if target_index > source_index || (target_index == source_index && target_depth < source_depth)
    {
        close(state, from);
        return Opened::Unchanged;
    }
    // Promote the existing target without popping the source's higher owners.
    while let Some(index) = state
        .modal_stack
        .continuations
        .iter()
        .position(|c| c.frame.surface == to)
    {
        let Continuation { owner, frame } = state.modal_stack.continuations.remove(index);
        replace_frame(state, owner, frame);
        release_payload(state, owner, false);
        if owner == from {
            transition(state);
            debug_assert!(super::consistent(state));
            return Opened::Truncated;
        }
    }
    let boundary = state.modal_stack.frames[source_index + usize::from(source_depth == 0)].id;
    let owner = state
        .modal_stack
        .continuations
        .iter()
        .find(|c| c.frame.surface == from)
        .map(|c| c.owner);
    close(state, from);
    if let Some(frame) = owner.and_then(|owner| take_continuation(state, owner)) {
        discard(state, frame.surface);
    }
    let target_index = state
        .modal_stack
        .iter()
        .position(|frame| frame.surface == to)
        .unwrap();
    while state
        .modal_stack
        .frames
        .get(target_index + 1)
        .is_some_and(|frame| frame.id != boundary)
    {
        let frame = state.modal_stack.frames.remove(target_index + 1);
        discard(state, frame.surface);
    }
    transition(state);
    debug_assert!(super::consistent(state));
    Opened::Truncated
}

/// The queue requests focus; this reducer alone writes the selected identity.
pub(crate) fn focus_decision(state: &mut TuiState, key: DecisionKey) {
    state.decisions.prepare(&key);
    open_frame(state, Surface::DecisionPane, Some(key));
    debug_assert!(super::consistent(state));
}

fn open_frame(state: &mut TuiState, surface: Surface, decision: Option<DecisionKey>) -> Opened {
    // A surface displaced into a continuation is still owned. Restore it before
    // truncating, rather than creating a second payload/identity for that surface.
    while !state
        .modal_stack
        .iter()
        .any(|frame| frame.surface == surface)
        && state
            .modal_stack
            .continuations
            .iter()
            .any(|c| c.frame.surface == surface)
    {
        back(state);
    }
    if let Some(index) = state
        .modal_stack
        .iter()
        .position(|frame| frame.surface == surface)
    {
        let truncated = index + 1 < state.modal_stack.len();
        while index + 1 < state.modal_stack.len() {
            back(state);
        }
        let retargeted = decision.is_some();
        if let Some(key) = decision {
            state.modal_stack.frames[index].decision = Some(key);
        }
        if truncated || retargeted {
            transition(state);
        }
        debug_assert!(super::consistent(state));
        return if truncated {
            Opened::Truncated
        } else {
            Opened::Unchanged
        };
    }
    prepare_payload(state, surface);
    let opened = if state.modal_stack.len() == 4 {
        let displaced = state.modal_stack.frames.pop().unwrap();
        if surface.keeps_continuation() {
            debug_assert!(
                !state
                    .modal_stack
                    .continuations
                    .iter()
                    .any(|c| c.owner == surface)
            );
            state.modal_stack.continuations.push(Continuation {
                owner: surface,
                frame: displaced,
            });
            Opened::Continued
        } else {
            discard(state, displaced.surface);
            Opened::Replaced
        }
    } else {
        Opened::Pushed
    };
    let frame = fresh_frame(state, surface, decision);
    state.modal_stack.frames.push(frame);
    transition(state);
    debug_assert!(super::consistent(state));
    opened
}

fn fresh_frame(state: &mut TuiState, surface: Surface, decision: Option<DecisionKey>) -> Frame {
    let stack = &mut state.modal_stack;
    stack.last_id = stack
        .last_id
        .checked_add(1)
        .expect("modal frame id overflow");
    Frame {
        id: stack.last_id,
        surface,
        conversation_id: None,
        decision,
        needs_initialization: matches!(
            surface,
            Surface::ModelSelector
                | Surface::ProfileSwitcher
                | Surface::UsagePanel
                | Surface::Login
                | Surface::Search
                | Surface::BookmarkList
                | Surface::WaveOverlay
        ),
    }
}

fn prepare_payload(state: &mut TuiState, surface: Surface) {
    match surface {
        Surface::CommandPalette => state.command_palette.reset(),
        Surface::Help => state.help_overlay.reset(),
        Surface::Search => state.search_state = Default::default(),
        Surface::CrossSearch => state.cross_search = Default::default(),
        Surface::BookmarkList => {
            state.bookmark_list_selected = 0;
            state.bookmark_list_count = 0;
            state.bookmark_owning_tab = None;
        }
        Surface::WaveOverlay => {
            state.wave_overlay_selected = 0;
            state.wave_diverge_open = false;
            state.wave_drill_body = None;
            state.wave_chat_keys = false;
        }
        Surface::NotificationLog => {
            state.notification_log = Some(NotificationLogState {
                selected_id: state
                    .notifications
                    .records()
                    .next_back()
                    .map(|record| record.id),
                selected: 0,
                detail: false,
                detail_scroll: 0,
                caller_focus: state.focus.clone(),
                search: Default::default(),
            });
            state.notifications.log_presented();
        }
        Surface::QuitConfirm => assert!(
            state.quit_confirm.is_some(),
            "Quit must open through request_quit"
        ),
        Surface::DecisionPane
        | Surface::ModelSelector
        | Surface::ProfileSwitcher
        | Surface::UsagePanel
        | Surface::Login => {}
    }
}

/// Preserve the existing guard facts and direct idle exit; repeated requests
/// neither replace the saved facts nor disturb any surface beneath Quit.
pub fn request_quit(state: &mut TuiState, facts: QuitFacts) -> QuitRequest {
    if is_open(state, Surface::QuitConfirm) {
        debug_assert!(super::consistent(state));
        return QuitRequest::Confirming;
    }
    if facts.is_idle() {
        debug_assert!(super::consistent(state));
        return QuitRequest::ExitNow;
    }
    state.quit_confirm = Some(QuitConfirmState {
        facts,
        caller_focus: state.focus.clone(),
    });
    open(state, Surface::QuitConfirm);
    debug_assert!(super::consistent(state));
    QuitRequest::Confirming
}

/// Back removes exactly the top frame, never an interrupted frame beneath it.
/// Search's user Back routes to `handlers::search::handle_search_action(CloseSearch)` first;
/// state-only truncation/discard must never restore a Stage through this reducer.
pub fn back(state: &mut TuiState) -> Option<Surface> {
    let Some(surface) = state.modal_stack.top().map(|frame| frame.surface) else {
        debug_assert!(super::consistent(state));
        return None;
    };
    close(state, surface);
    debug_assert!(super::consistent(state));
    Some(surface)
}

/// Discard every replaced-Stage owner, including suspended continuations.
/// Global frames and their identities survive; an old Search snapshot is never
/// restored into the replacement conversation.
pub fn reconcile_stage(state: &mut TuiState) {
    // Undo remains available after an ordinary close, but never in another Stage.
    state.bookmark_undo_buffer = None;
    loop {
        let stage = state
            .modal_stack
            .all_frames()
            .find(|frame| frame.conversation_id.is_some())
            .map(|frame| frame.surface);
        let Some(surface) = stage else { break };
        close(state, surface);
    }
}

/// A replacement effect's base target yields to retained frames or a promoted approval.
pub fn settle_stage_base(state: &mut TuiState, focus: crate::domain::models::FocusState) {
    if state.pending_permission.is_none() {
        set_base(state, focus);
    }
}

/// Remove one surface and its payload. Closing a covered surface does not
/// change the focus of the still-open top frame.
pub fn close(state: &mut TuiState, surface: Surface) {
    if remove_surface(state, surface) {
        transition(state);
        debug_assert!(super::consistent(state));
    }
}

/// Handoffs remove and open atomically before settling legacy owners.
fn remove_surface(state: &mut TuiState, surface: Surface) -> bool {
    let live = state
        .modal_stack
        .frames
        .iter()
        .position(|frame| frame.surface == surface);
    let suspended = state
        .modal_stack
        .continuations
        .iter()
        .position(|c| c.frame.surface == surface);
    let was_top = live.is_some_and(|i| i + 1 == state.modal_stack.len());
    if let Some(index) = live {
        state.modal_stack.frames.remove(index);
        if let Some(restored) = take_continuation(state, surface) {
            state.modal_stack.frames.insert(index, restored);
        }
    } else if let Some(index) = suspended {
        let outer = state.modal_stack.continuations.remove(index).owner;
        if let Some(restored) = take_continuation(state, surface) {
            state.modal_stack.continuations.push(Continuation {
                owner: outer,
                frame: restored,
            });
        }
    } else {
        debug_assert!(super::consistent(state));
        return false;
    }
    release_payload(state, surface, was_top);
    true
}

fn take_continuation(state: &mut TuiState, owner: Surface) -> Option<Frame> {
    let index = state
        .modal_stack
        .continuations
        .iter()
        .position(|c| c.owner == owner)?;
    Some(state.modal_stack.continuations.remove(index).frame)
}

fn discard(state: &mut TuiState, surface: Surface) {
    if let Some(frame) = take_continuation(state, surface) {
        discard(state, frame.surface);
    }
    release_payload(state, surface, false);
}

fn release_payload(state: &mut TuiState, surface: Surface, was_top: bool) {
    let caller_focus = match surface {
        Surface::CommandPalette => {
            state.command_palette.clear();
            None
        }
        Surface::Help => {
            state.help_overlay.search = Default::default();
            state.help_overlay.filtered_scroll_offset = 0;
            None
        }
        Surface::DecisionPane => {
            state.decisions.clear_paint();
            None
        }
        Surface::ModelSelector => {
            state.model_selector.clear();
            None
        }
        Surface::ProfileSwitcher => {
            state.profile_switcher.clear();
            None
        }
        Surface::UsagePanel => {
            state.usage_panel.clear();
            None
        }
        Surface::Login => {
            state.login.clear();
            None
        }
        Surface::Search => {
            state.search_state = Default::default();
            None
        }
        Surface::CrossSearch => {
            state.cross_search = Default::default();
            None
        }
        Surface::BookmarkList => {
            state.bookmark_list_selected = 0;
            state.bookmark_list_count = 0;
            state.bookmark_owning_tab = None;
            None
        }
        Surface::WaveOverlay => {
            state.wave_overlay_selected = 0;
            state.wave_diverge_open = false;
            state.wave_drill_body = None;
            state.wave_chat_keys = false;
            None
        }
        Surface::NotificationLog => {
            let caller = state.notification_log.take().map(|log| log.caller_focus);
            state.notifications.log_closed();
            caller
        }
        Surface::QuitConfirm => state.quit_confirm.take().map(|quit| quit.caller_focus),
    };
    if was_top && let Some(focus) = caller_focus {
        state.focus = focus;
    }
}

fn transition(state: &mut TuiState) {
    settle_legacy_inner(state);
    refresh_hint(state);
}

fn refresh_hint(state: &mut TuiState) {
    state.needs_redraw = true;
    state.current_hint = crate::adapters::tui::hints::contextual_hint(
        &state.focus,
        state.session_count,
        state.theme.timing.status_hint_fade_sessions,
        state.is_vscode,
        &state.modal_stack,
        state.pending_leader,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::keymap::{Host, Key};
    use crate::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
    use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
    use crate::adapters::tui::presentation::consistent;
    use crate::adapters::tui::quit_confirm::{self, QuitHost, QuitKeyOutcome};
    use crate::domain::models::{FocusState, OverlayType};

    #[test]
    fn story21_4_pane_opener_enters_the_stack() {
        let mut state = TuiState::new(80, 24);
        crate::adapters::tui::decision_pane::raise(
            &mut state,
            crate::adapters::tui::decision_pane::DecisionDraft::new(
                crate::adapters::tui::decision_pane::DecisionKind::Budget,
                "automatic first arrival",
            ),
            crate::adapters::tui::decision_pane::Arrival::Automatic,
        );
        assert_eq!(state.modal_stack.len(), 1);
        assert!(super::super::consistent(&state));
    }

    #[test]
    fn story21_4_front_doors_preserve_identity_payload_and_legacy_base() {
        for (host, quit_host) in [
            (Host::Local, QuitHost::Local),
            (Host::Attached, QuitHost::Attached),
        ] {
            let mut state = TuiState::new(80, 24);
            crate::adapters::tui::app::handle_input(
                &mut state,
                &crate::domain::events::DomainInputEvent::SpecialKey(
                    crate::domain::events::DomainKey::CtrlX,
                ),
            );
            crate::adapters::tui::app::handle_input(
                &mut state,
                &crate::domain::events::DomainInputEvent::KeyPress('?'),
            );
            state.help_overlay.scroll_offset = 7;
            state.input_buffer = "keep draft".into();
            state.cursor_position = 3;
            let base = state.focus.clone();
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Info,
                "older record",
            );
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Warning,
                "newest record",
            );
            let newest = state.notifications.records().next_back().unwrap().id;
            notification_log::open(&mut state);
            let log_frame = state.modal_stack.top().unwrap().clone();
            assert_eq!(log_frame.surface, Surface::NotificationLog);
            assert_eq!(log_frame.conversation_id, None);
            assert_eq!(
                state.notification_log.as_ref().unwrap().selected_id,
                Some(newest)
            );
            let facts = LogFacts::default();
            for key in [Key::Char('j'), Key::Enter, Key::Char('j')] {
                assert_eq!(
                    notification_log::handle_key(&mut state, host, key, &facts),
                    LogOutcome::Consumed
                );
            }
            let log = state.notification_log.clone();
            assert!(log.as_ref().unwrap().detail);
            assert_eq!(log.as_ref().unwrap().detail_scroll, 1);
            state.needs_redraw = false;
            notification_log::open(&mut state);
            assert!(!state.needs_redraw, "duplicate open is a no-op");
            assert_eq!(state.modal_stack.len(), 2);
            assert_eq!(state.modal_stack.top(), Some(&log_frame));
            assert_eq!(state.notification_log, log);
            assert_eq!(state.focus, base);
            assert!(consistent(&state));

            // A18: Ctrl+Q asks the host to guard quit without closing Log.
            assert_eq!(
                notification_log::handle_key(&mut state, host, Key::Ctrl('q'), &facts),
                LogOutcome::Quit
            );
            assert_eq!(state.notification_log, log);
            let busy = QuitFacts {
                streams: 1,
                ..QuitFacts::idle(quit_host)
            };
            assert_eq!(
                quit_confirm::request(&mut state, busy),
                QuitRequest::Confirming
            );
            let quit_frame = state.modal_stack.top().unwrap().clone();
            let quit = state.quit_confirm.clone();
            assert_eq!(quit_frame.surface, Surface::QuitConfirm);
            assert!(quit_frame.id > log_frame.id);
            state.needs_redraw = false;
            assert_eq!(
                quit_confirm::request(&mut state, QuitFacts::idle(quit_host)),
                QuitRequest::Confirming
            );
            assert!(!state.needs_redraw);
            assert_eq!(state.modal_stack.len(), 3);
            assert_eq!(state.modal_stack.top(), Some(&quit_frame));
            assert_eq!(state.quit_confirm, quit);
            assert_eq!(state.notification_log, log);
            assert_eq!(state.focus, base);
            assert!(consistent(&state));

            assert_eq!(
                quit_confirm::execute(&mut state, host, Key::Esc),
                QuitKeyOutcome::Cancelled
            );
            assert_eq!(state.modal_stack.top(), Some(&log_frame));
            assert_eq!(state.notification_log, log);
            assert!(state.quit_confirm.is_none());
            assert!(consistent(&state));
            assert_eq!(
                notification_log::handle_key(&mut state, host, Key::Esc, &facts),
                LogOutcome::Closed
            );
            assert_eq!(state.modal_stack.len(), 1, "Help is still the caller");
            assert!(state.notification_log.is_none());
            assert!(consistent(&state));
            assert_eq!(state.focus, base);
            assert!(is_open(&state, Surface::Help));
            assert_eq!(state.help_overlay.scroll_offset, 7);
            assert_eq!(state.input_buffer, "keep draft");
            assert_eq!(state.cursor_position, 3);

            notification_log::open(&mut state);
            let reopened = state.modal_stack.top().unwrap();
            assert!(reopened.id > quit_frame.id, "ids never recycle after back");
            assert_eq!(reopened.surface, Surface::NotificationLog);
            assert!(consistent(&state));
        }
    }

    #[test]
    fn story21_4_handoff_preserves_existing_targets_and_higher_continuations() {
        // Reducer topology controls; K7 separately witnesses the real Login completion.
        for case in 0..3 {
            let mut state = TuiState::new(80, 24);
            let surfaces = if case == 0 {
                [
                    Surface::ModelSelector,
                    Surface::ProfileSwitcher,
                    Surface::UsagePanel,
                    Surface::Help,
                ]
            } else {
                [
                    Surface::ProfileSwitcher,
                    Surface::UsagePanel,
                    Surface::NotificationLog,
                    Surface::ModelSelector,
                ]
            };
            for surface in surfaces {
                open(&mut state, surface);
            }
            let model = state
                .modal_stack
                .iter()
                .find(|f| f.surface == Surface::ModelSelector)
                .unwrap()
                .clone();
            let callers = state.modal_stack.frames[..if case == 0 { 0 } else { 3 }].to_vec();
            if case != 0 {
                open(&mut state, Surface::Help);
            }
            if case == 2 {
                close(&mut state, Surface::UsagePanel);
                close(&mut state, Surface::NotificationLog);
            }
            open(&mut state, Surface::Login);
            quit_confirm::request(
                &mut state,
                QuitFacts {
                    composer_draft: true,
                    ..QuitFacts::idle(QuitHost::Local)
                },
            );
            let quit = state.modal_stack.top().cloned().unwrap();
            assert_eq!(
                handoff(&mut state, Surface::Login, Surface::ModelSelector),
                Opened::Truncated
            );
            assert_eq!(state.modal_stack.top(), Some(&quit));
            assert!(!is_open(&state, Surface::Help));
            assert!(!is_open(&state, Surface::Login));
            assert!(consistent(&state));
            back(&mut state);
            assert_eq!(state.modal_stack.top(), Some(&model));
            let kept = if case == 2 { 1 } else { callers.len() };
            assert_eq!(&state.modal_stack.frames[..kept], &callers[..kept]);
            assert_eq!(state.modal_stack.len(), kept + 1);
            assert_eq!(state.focus, FocusState::Input);
            assert!(consistent(&state));
        }
    }

    #[test]
    fn story21_4_idle_and_confirm_remove_no_underlying_frame() {
        let mut state = TuiState::new(80, 24);
        notification_log::open(&mut state);
        let frame = state.modal_stack.top().unwrap().clone();
        assert_eq!(
            quit_confirm::request(&mut state, QuitFacts::idle(QuitHost::Local)),
            QuitRequest::ExitNow
        );
        assert_eq!(state.modal_stack.top(), Some(&frame));
        assert!(state.quit_confirm.is_none());
        assert!(consistent(&state));
        quit_confirm::request(
            &mut state,
            QuitFacts {
                composer_draft: true,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
        assert_eq!(
            quit_confirm::execute(&mut state, Host::Local, Key::Char('y')),
            QuitKeyOutcome::Confirmed
        );
        assert_eq!(state.modal_stack.len(), 1);
        assert_eq!(state.modal_stack.top(), Some(&frame));
        assert!(state.notification_log.is_some());
        assert!(consistent(&state));
        assert_eq!(
            quit_confirm::execute(&mut state, Host::Local, Key::Char('y')),
            QuitKeyOutcome::Consumed
        );
        assert_eq!(state.modal_stack.top(), Some(&frame));
    }
}
