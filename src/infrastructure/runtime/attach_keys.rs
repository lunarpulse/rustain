//! Attached host key front door (Story 21.2 Task 5: AC6 / AC8 / AC12 / AC13).
//!
//! [`handle_attached_key`] is the one place `run_attached` sends EVERY key
//! event. The shared presentation boundary owns S-QUIT, S-LOG and the minimum
//! terminal size before the legacy overlays, team-retract card, approvals,
//! proposals, the thread window, peer card Tab and inline decision cards.
//! The attached client also supports:
//!
//! * the untimed `Ctrl+X` leader with the complete A5 follow-up table,
//! * `Esc` / `Ctrl+D` / `Ctrl+C` / `q` with their local meanings (none of them
//!   detaches the client),
//! * multiline composition and the portable `Ctrl+X Enter` / `Ctrl+Enter` send,
//! * the shared command palette and slash popup, restricted to what the
//!   attached client really handles,
//! * the guarded S-QUIT decision, and the `/detach` `/ml` `/quit` commands.
//!
//! The shared state owns the draft, caret and attached card presentation.
//! Socket responders, requests and interruption remain runtime-owned.
//!
//! This is a child module of `attach_loop` (declared there with `#[path]`) so
//! it can reach that module's private team/approval/surface machinery without
//! widening its visibility.

use std::path::Path;

use crate::adapters::tui::app::refuse_leader;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use super::{
    AttachedTeamRequests, ComposerOutcome, SocketTurnDriver,
    attached_composer_key_with_team_requests, open_thread_for_node, preview_line, retractable_rows,
    system_message, thread_flash,
};
use crate::adapters::builtin_commands::{self, CommandHost};
use crate::adapters::command_registry::{CommandRegistry, CommandSource};
use crate::adapters::daemon::protocol::ClientFrame;
use crate::adapters::palette_registry::PaletteRegistry;
use crate::adapters::tui::app::{
    cancel_autocomplete_popup, convert_crossterm_event, handle_input, open_leader,
};
use crate::adapters::tui::keymap::render::{PendingPrefix, StatusTail};
use crate::adapters::tui::keymap::resolve::no_binding_text;
use crate::adapters::tui::keymap::table::{ATTACHED_SIGN_IN, GlobalCmd, LeaderCmd};
use crate::adapters::tui::keymap::table_chat::ChatCmd;
use crate::adapters::tui::keymap::table_input::InputCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{
    self, AvailCtx, CommandId, Context, Host, Key, LeaderStep, Resolved, leader_step,
};
use crate::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
use crate::adapters::tui::palette_gate::PaletteGate;
use crate::adapters::tui::presentation::{self, RouteEffect, Surface};
use crate::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
use crate::adapters::tui::state::{TabRenderState, TuiState};
use crate::adapters::tui::widgets::{autocomplete_popup, input_box};
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use crate::domain::models::{
    Conversation, FeedbackLevel, FocusState, MouseConfig, SessionManager, StreamingState,
};
use crate::infrastructure::runtime::attach_interrupt::{AttachedInterrupt, nothing_to_stop_notice};
use crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;
use crate::infrastructure::runtime::turn_driver::{TurnDriver, TurnViewState, UserSubmission};
use tokio_util::sync::CancellationToken;

/// What the host loop does after one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::infrastructure::runtime) enum KeyFlow {
    /// Stay attached.
    Continue,
    /// `Ctrl+X d` / `/detach`: leave this client; the daemon and its work keep
    /// running. The loop epilogue sends the one `ClientFrame::Detach`.
    Detach,
    /// A clean idle quit or an explicit S-QUIT `y`: leave this client only.
    Quit,
}

/// Reserved palette command name for the retract-an-auto-sent-message route.
/// Never a slash command: it is only produced by the attached palette.
const RETRACT_AUTO: &str = "retract-auto";

/// Attached presentation chrome that outlives one key: the palette populate
/// gate, the command catalogue and the palette registry.
pub(super) struct AttachedChrome {
    gate: PaletteGate,
    commands: CommandRegistry,
    palette: PaletteRegistry,
    last_slash_filter: Option<String>,
}

impl AttachedChrome {
    pub(super) fn new() -> Self {
        Self {
            gate: PaletteGate::new(),
            commands: CommandRegistry::new(),
            palette: PaletteRegistry::for_host(CommandHost::Attached),
            last_slash_filter: None,
        }
    }

    /// Start a palette session: a fresh registry carrying the attached-only
    /// entries (presentation commands the shared catalogue does not list, and
    /// the retract route over REAL retractable rows — never an invented id).
    fn begin_palette(&mut self, conversation: &Conversation, read_only: bool) {
        let mut palette = PaletteRegistry::for_host(CommandHost::Attached);
        for name in builtin_commands::offered_names(CommandHost::Attached) {
            if self.commands.find(name).is_some() {
                continue;
            }
            palette.register(PaletteEntry {
                name: format!("/{name}"),
                description: attached_description(&self.commands, name),
                shortcut: crate::adapters::tui::keymap::render::shortcut_for_slash(name),
                scope: PaletteScope::SlashCommand,
                action: PaletteAction::ExecuteCommand(name.to_owned(), None),
            });
        }
        let mut targets = 0usize;
        for row in retractable_rows(conversation).take(5) {
            targets += 1;
            palette.register(PaletteEntry {
                name: format!("retract: {}", preview_line(&row.content, 32)),
                description: if read_only {
                    "unavailable — read-only, can't retract here".to_owned()
                } else {
                    "Retract this auto-sent message (asks to confirm first)".to_owned()
                },
                shortcut: None,
                scope: PaletteScope::All,
                action: PaletteAction::ExecuteCommand(
                    RETRACT_AUTO.to_owned(),
                    Some(row.id.clone()),
                ),
            });
        }
        if targets == 0 {
            palette.register(PaletteEntry {
                name: "retract: no auto-sent message".to_owned(),
                description: "unavailable — nothing to retract in this session".to_owned(),
                shortcut: None,
                scope: PaletteScope::All,
                action: PaletteAction::ExecuteCommand(RETRACT_AUTO.to_owned(), None),
            });
        }
        self.palette = palette;
        self.gate.invalidate();
    }

    /// Bring the open palette's entries up to date (the shared gate decides
    /// whether a recompute is needed) and narrow them to attached truth.
    async fn sync_palette(&mut self, state: &mut TuiState, workspace: &Path) {
        let recomputed = self
            .gate
            .sync(state, &mut self.commands, &mut self.palette, workspace)
            .await;
        if recomputed {
            let commands = &self.commands;
            state
                .command_palette
                .filtered_entries
                .retain(|entry| attached_palette_entry(entry, commands));
            let max = state
                .command_palette
                .filtered_entries
                .len()
                .saturating_sub(1);
            state.command_palette.selected_index = state.command_palette.selected_index.min(max);
            state.command_palette.scroll_offset = state.command_palette.scroll_offset.min(max);
        }
    }

    /// The real bracketed-paste effect settles ownership and populates before paint.
    pub(super) async fn paste(
        &mut self,
        state: &mut TuiState,
        text: &str,
        card_pending: bool,
        workspace: &Path,
    ) {
        if handle_attached_paste(state, text, card_pending) {
            self.sync_palette(state, workspace).await;
        }
    }

    /// Fill the slash popup from the attached catalogue. Re-scans custom
    /// command files only when the popup opens (empty filter), like the local
    /// host; filtering as you type uses the cached registry.
    fn sync_slash(&mut self, state: &mut TuiState, workspace: &Path) {
        if state.autocomplete.kind != Some(AutocompleteKind::SlashCommand) {
            self.last_slash_filter = None;
            return;
        }
        let unchanged = self.last_slash_filter.as_deref() == Some(&*state.autocomplete.filter_text);
        if unchanged && !state.autocomplete.suggestions.is_empty() {
            return;
        }
        if state.autocomplete.filter_text.is_empty() {
            self.commands.refresh(workspace);
            crate::adapters::command_registry::announce_shadows(state, &self.commands);
        }
        self.last_slash_filter = Some(state.autocomplete.filter_text.clone());
        state.autocomplete.suggestions =
            attached_slash_suggestions(&self.commands, &state.autocomplete.filter_text);
        if state.autocomplete.selected_index >= state.autocomplete.suggestions.len() {
            state.autocomplete.selected_index = 0;
            state.autocomplete.scroll_offset = 0;
        }
    }
}

fn attached_description(commands: &CommandRegistry, name: &str) -> String {
    commands
        .find(name)
        .map(|command| command.description.clone())
        .unwrap_or_else(|| {
            match name {
                "ml" => "Toggle multi-line input",
                "quit" => "Leave this client (asks first when work is in flight)",
                "notifications" => "Open the notification log",
                "detach" => "Detach this client; the daemon and its work keep running",
                _ => "",
            }
            .to_owned()
        })
}

/// An entry survives only when the attached client really handles it.
fn attached_palette_entry(entry: &PaletteEntry, commands: &CommandRegistry) -> bool {
    match &entry.action {
        PaletteAction::ExecuteCommand(name, _) if name == RETRACT_AUTO => true,
        PaletteAction::ExecuteCommand(name, None) => {
            entry.scope == PaletteScope::SlashCommand
                && entry.name.strip_prefix('/') == Some(name.as_str())
                && (builtin_commands::offered_in(CommandHost::Attached, name)
                    || commands
                        .find(name)
                        .is_some_and(|c| matches!(c.source, CommandSource::UserDefined { .. })))
        }
        _ => false,
    }
}

fn attached_slash_suggestions(
    commands: &CommandRegistry,
    filter: &str,
) -> Vec<AutocompleteSuggestion> {
    let lower = filter.to_lowercase();
    let mut suggestions = Vec::new();
    for name in builtin_commands::offered_names(CommandHost::Attached) {
        if lower.is_empty() || name.contains(&lower) {
            suggestions.push(AutocompleteSuggestion::SlashCommand {
                name: name.to_owned(),
                description: attached_description(commands, name),
            });
        }
    }
    for command in commands.filter(filter) {
        if matches!(command.source, CommandSource::UserDefined { .. }) {
            suggestions.push(AutocompleteSuggestion::SlashCommand {
                name: command.name.clone(),
                description: command.description.clone(),
            });
        }
    }
    suggestions
}

/// Everything one key may touch. Borrowed from `run_attached`'s locals for the
/// duration of one key event.
pub(super) struct KeyCtx<'a> {
    pub(super) workspace: &'a Path,
    pub(super) read_only: bool,
    pub(super) state: &'a mut TuiState,
    pub(super) conversation: &'a mut Conversation,
    pub(super) streaming: &'a mut StreamingState,
    pub(super) frame_tx: &'a mpsc::UnboundedSender<ClientFrame>,
    pub(super) team_requests: &'a mut AttachedTeamRequests,
    pub(super) tab: &'a mut TabRenderState,
    pub(super) view: &'a mut crate::domain::models::view_state::ViewState,
    pub(super) interrupt: &'a AttachedInterrupt,
    pub(super) driver: &'a SocketTurnDriver,
    pub(super) lifecycle: &'a mut TuiTurnLifecycle,
    pub(super) session_manager: &'a mut SessionManager,
    pub(super) log_service: &'a crate::infrastructure::transparency::TransparencyService,
    pub(super) chrome: &'a mut AttachedChrome,
}

impl KeyCtx<'_> {
    /// An Info notice about the attached client itself (a refused or inert
    /// action): a store record, never a status flash.
    fn info(&mut self, message: impl Into<String>) {
        notifications::info(self.state, NoticeSource::Attached, message);
    }

    /// A peer reply composition owns the composer text.
    fn composer_locked(&self) -> bool {
        crate::adapters::tui::handlers::recipient_actions::pending_composition(self.state).is_some()
    }
}

/// The status-row hint for the attached client, generated from the registry
/// (Story 21.3 A10): the Detach leader row, `i` to type while Chat has focus
/// and `[^X Enter] Send` while multi-line. The `^X menu` tail is painted by the
/// status row itself, so it is not repeated here.
pub(super) fn status_hint(state: &TuiState) -> Option<String> {
    if !state.modal_stack.is_empty() || state.pending_leader.is_some() {
        return None;
    }
    crate::adapters::tui::keymap::render_tips::attached_status_tip(
        state.focus == FocusState::Chat,
        state.multiline_mode,
    )
}

/// The S-LOG facts of the attached client: the one conversation it shows, no
/// running-turn fact and NO retry path (`r` is never painted or acted on).
pub(super) fn log_facts(conversation_id: &str) -> LogFacts {
    LogFacts {
        active_conversation_id: Some(conversation_id.to_owned()),
        active_messages: 0,
        turn_running: false,
        retry_supported: false,
        decisions_supported: false,
        context_warning: false,
        titles: Vec::new(),
    }
}

/// Key and wheel ingress share the same owner boundary; paste stays borrowed.
pub(super) fn route_event(state: &mut TuiState, event: &Event) -> Option<RouteEffect> {
    match convert_crossterm_event(event, &MouseConfig::default()) {
        Some(event) => presentation::route(state, &event, Host::Attached, &LogFacts::default()),
        None => presentation::route::blocks_paste(state).then_some(RouteEffect::Consumed),
    }
}

/// Apply a wheel event only after the shared presentation boundary releases it.
pub(super) fn handle_attached_mouse(
    state: &mut TuiState,
    view: &mut crate::domain::models::view_state::ViewState,
    event: &Event,
) -> Option<RouteEffect> {
    let routed = route_event(state, event);
    if routed.is_none()
        && let Some(crate::domain::events::DomainInputEvent::MouseScroll(delta)) =
            convert_crossterm_event(event, &MouseConfig::default())
    {
        use crate::domain::models::view_state::ScrollDelta;
        let delta = match delta {
            ScrollDelta::WheelUp(rows) => ScrollDelta::UnboundedRows(rows.max(1) as isize),
            ScrollDelta::WheelDown(rows) => ScrollDelta::UnboundedRows(-(rows.max(1) as isize)),
            other => other,
        };
        crate::adapters::tui::handlers::scroll::dispatch_scroll(view, state, delta);
    }
    routed
}

/// Borrowed paste edits an offered field before considering the base composer.
pub(super) fn handle_attached_paste(state: &mut TuiState, text: &str, card_pending: bool) -> bool {
    if presentation::route::paste(state, text, Host::Attached).is_some() {
        return !presentation::too_small(state)
            && state.pending_leader.is_none()
            && state
                .modal_stack
                .top()
                .is_some_and(|frame| frame.surface == Surface::CommandPalette);
    }
    if presentation::route::blocks_paste(state) {
        return false;
    }
    if !card_pending
        && state.focus == FocusState::Input
        && crate::adapters::tui::handlers::recipient_actions::pending_composition(state).is_none()
        && !crate::adapters::tui::decision_queue::team_retract_pending(state)
    {
        let cursor = crate::adapters::tui::text_geometry::snap_boundary(
            &state.input_buffer,
            state.cursor_position,
        );
        let byte = crate::adapters::tui::text_geometry::byte_of_scalar(&state.input_buffer, cursor);
        state.input_buffer.insert_str(byte, text);
        state.cursor_position = cursor + text.chars().count();
        state.autocomplete.slash_rollback = None;
        state.autocomplete.dismiss();
        state.needs_redraw = true;
    }
    false
}

/// Check the raw event before conversion loses its kind and modifier details.
fn invalid_decision_key(state: &TuiState, key: KeyEvent) -> bool {
    use crate::adapters::tui::decision_queue::{DecisionKind, DecisionSubject};
    use crossterm::event::KeyEventKind;

    let Some(item) = state
        .modal_stack
        .top()
        .filter(|frame| frame.surface == Surface::Decision)
        .and_then(|frame| frame.decision.as_ref())
        .and_then(|id| state.decisions.item(id))
    else {
        return false;
    };
    let composing = matches!(&item.subject, DecisionSubject::Permission(payload) if payload.feedback.is_some())
        || matches!(&item.subject, DecisionSubject::RecipientDraft(payload) if payload.compose.is_some())
        || matches!(&item.subject, DecisionSubject::Ticket(payload) if payload.editing);
    let answer = if composing {
        key.code == KeyCode::Enter
    } else {
        matches!(item.kind, DecisionKind::Permission | DecisionKind::Consent)
            && matches!(
                key.code,
                KeyCode::Char(
                    'y' | 'Y' | 's' | 'S' | 'a' | 'A' | 'n' | 'N' | 'f' | 'F' | 'r' | 'R'
                )
            )
            || matches!(
                item.kind,
                DecisionKind::Consolidation | DecisionKind::TeamRetract
            ) && matches!(key.code, KeyCode::Char('y' | 'Y' | 'n' | 'N'))
            || item.kind == DecisionKind::ReplyArrived
                && matches!(key.code, KeyCode::Enter | KeyCode::Char('x' | 'X'))
            || item.kind == DecisionKind::TeamProposal
                && matches!(key.code, KeyCode::Char('y' | 'Y' | 'n' | 'N' | 'r' | 'R'))
            || item.kind == DecisionKind::RecipientDraftApproval
                && matches!(
                    key.code,
                    KeyCode::Char('y' | 'Y' | 'n' | 'N' | 'e' | 'E' | 'w' | 'W' | 'v' | 'V')
                )
            || item.kind == DecisionKind::Ticket
                && matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('d' | 'D' | 'x' | 'X' | 'e' | 'E')
                )
    };
    if answer && key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::CONTROL {
        return key.kind != KeyEventKind::Press;
    }
    answer && (key.kind != KeyEventKind::Press || !key.modifiers.is_empty())
}

/// The attached key front door. Called by `run_attached` for every key event.
pub(super) async fn handle_attached_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    if invalid_decision_key(ctx.state, key)
        || crate::adapters::tui::thread_inspector::ignores_raw_key(ctx.state, &key)
        || crate::adapters::tui::mission_control::keys::ignores_raw_key(ctx.state, &key)
    {
        return KeyFlow::Continue;
    }
    let owner_before = (
        ctx.state.modal_stack.top().map(|frame| frame.id),
        ctx.state.pending_leader,
        crate::adapters::tui::app::focus_context(ctx.state),
    );
    let routed =
        convert_crossterm_event(&Event::Key(key), &MouseConfig::default()).and_then(|event| {
            presentation::route(
                ctx.state,
                &event,
                Host::Attached,
                &log_facts(&ctx.conversation.id),
            )
        });
    let flow = match routed {
        Some(RouteEffect::Consumed) => KeyFlow::Continue,
        Some(RouteEffect::QuitRequest) => request_quit(ctx),
        Some(RouteEffect::QuitConfirmed) => KeyFlow::Quit,
        Some(RouteEffect::Log(outcome)) => apply_log_outcome(ctx, outcome),
        Some(RouteEffect::OpenPalette) => {
            open_palette(ctx).await;
            KeyFlow::Continue
        }
        Some(RouteEffect::Frame(Context::CommandPalette)) => palette_key(ctx, key).await,
        Some(RouteEffect::Frame(Context::Help)) => {
            popup_delegate(ctx, key);
            KeyFlow::Continue
        }
        Some(RouteEffect::Leader) => leader_key(ctx, key).await,
        Some(RouteEffect::Frame(Context::ThreadInspector | Context::BodyViewport)) => {
            thread_key(ctx, key).await
        }
        Some(RouteEffect::Frame(Context::ThreadReply)) => thread_reply_key(ctx, key).await,
        Some(RouteEffect::Frame(context)) => unreachable!("attached frame context: {context:?}"),
        Some(RouteEffect::Action(_)) => unreachable!("local-only frame action in attached host"),
        Some(RouteEffect::Decision(
            crate::adapters::tui::decision_queue::DecisionEffect::ReplyRead,
        )) => {
            if let Some(crate::adapters::tui::decision_queue::DecisionSubject::ReplyArrived(
                payload,
            )) = ctx.state.decisions.take_resolved()
            {
                let request = ctx.team_requests.mint();
                let frame = ClientFrame::TeamThread {
                    request,
                    peer: payload.alias,
                    item_id: payload.item,
                };
                super::enqueue_attached_team_frame(
                    ctx.state,
                    ctx.team_requests,
                    frame,
                    ctx.frame_tx,
                );
            }
            KeyFlow::Continue
        }
        Some(RouteEffect::Decision(
            crate::adapters::tui::decision_queue::DecisionEffect::ReplyDismiss,
        )) => {
            drop(ctx.state.decisions.take_resolved());
            KeyFlow::Continue
        }
        Some(RouteEffect::Decision(
            crate::adapters::tui::decision_queue::DecisionEffect::RecipientView,
        )) => {
            let node = ctx
                .state
                .modal_stack
                .top()
                .and_then(|frame| frame.decision.as_ref())
                .and_then(|id| ctx.state.decisions.item(id))
                .and_then(|item| match &item.subject {
                    crate::adapters::tui::decision_queue::DecisionSubject::RecipientDraft(
                        payload,
                    ) => Some(payload.node.clone()),
                    _ => None,
                });
            if let Some(node) = node {
                crate::adapters::tui::decision_queue::defer_focused(ctx.state);
                if let Err(error) = open_thread_for_node(
                    ctx.workspace,
                    ctx.conversation,
                    &node,
                    ctx.state,
                    ctx.read_only,
                    false,
                )
                .await
                {
                    thread_flash(ctx.state, error.to_string());
                }
            }
            KeyFlow::Continue
        }
        Some(RouteEffect::Decision(_)) => {
            if let Some((key, effect, subject)) = ctx.state.decisions.take_sending() {
                use crate::adapters::tui::decision_queue::{
                    self, DecisionEffect as Effect, DecisionSubject, RetractTarget,
                };
                let mut settle = false;
                // These answers leave a transcript view block (retract receipt, send
                // rows, dismissal row) below the newest row; a transcript scrolled up
                // by the user would hide the result of the decision just made.
                let shows_view = matches!(
                    subject,
                    DecisionSubject::TeamProposal(_) | DecisionSubject::TeamRetract(_)
                );
                let frame = match (subject, effect) {
                    (
                        DecisionSubject::Consolidation(_)
                        | DecisionSubject::TeamRetract(_)
                        | DecisionSubject::TeamProposal(_)
                        | DecisionSubject::Ticket(_),
                        _,
                    ) if ctx.read_only => {
                        decision_queue::withdraw_key(
                            ctx.state,
                            &key,
                            decision_queue::NO_LONGER_PENDING,
                        );
                        None
                    }
                    (
                        DecisionSubject::Ticket(payload),
                        effect @ (Effect::TicketAccept
                        | Effect::TicketDecline
                        | Effect::TicketCancel),
                    ) => {
                        match crate::infrastructure::runtime::ticket_bridge::response(
                            &payload, effect,
                        ) {
                            Ok(responses) => Some(ClientFrame::InputResponse {
                                node: payload.node.as_str().to_owned(),
                                responses,
                            }),
                            Err(error) => {
                                decision_queue::set_ticket_error(ctx.state, &key, error);
                                None
                            }
                        }
                    }
                    (DecisionSubject::RecipientDraft(payload), effect) => {
                        use crate::adapters::daemon::protocol::PeerDraftAction;
                        use crate::adapters::tui::decision_queue::RecipientComposeMode;
                        let still_live = ctx.conversation.messages.iter().any(|message| {
                            let Some(control) = ctx.conversation.peer_row_controls.get(&message.id) else { return false };
                            if control.node_id != payload.node || !control.node_resolvable || control.item_address != payload.item_address {
                                return false;
                            }
                            match (&payload.stage, control.stage) {
                                (decision_queue::RecipientDraftStage::AwaitingOperator, crate::domain::models::PeerRowStage::AwaitingOperator) => true,
                                (decision_queue::RecipientDraftStage::Ready(draft), crate::domain::models::PeerRowStage::Ready) =>
                                    message.content.strip_prefix(crate::adapters::daemon::response_modes::DRAFT_APPROVAL_PREFIX)
                                        .unwrap_or(&message.content) == draft,
                                _ => false,
                            }
                        });
                        let action = match effect {
                            Effect::RecipientApprove => Some(PeerDraftAction::Approve),
                            Effect::RecipientReject => Some(PeerDraftAction::Reject),
                            Effect::RecipientSend => {
                                payload.compose.map(|(mode, field)| match mode {
                                    RecipientComposeMode::Edit => PeerDraftAction::Edit {
                                        content: field.as_str().to_owned(),
                                    },
                                    RecipientComposeMode::WriteOwn => PeerDraftAction::WriteOwn {
                                        content: field.as_str().to_owned(),
                                    },
                                })
                            }
                            _ => None,
                        };
                        if !ctx.read_only
                            && still_live
                            && let Some(action) = action
                        {
                            settle = super::queue_recipient_action(
                                ctx.state,
                                ClientFrame::ResolvePeerDraft {
                                    node: payload.node,
                                    action,
                                },
                                ctx.frame_tx,
                                false,
                            );
                        }
                        None
                    }
                    (DecisionSubject::Permission(payload), effect) => {
                        payload
                            .outcome(effect)
                            .map(|outcome| ClientFrame::ApprovalResponse {
                                request_id: payload.request_id,
                                outcome,
                            })
                    }
                    (DecisionSubject::Consent(payload), effect) => {
                        payload
                            .outcome(effect)
                            .map(|outcome| ClientFrame::ApprovalResponse {
                                request_id: payload.request_id,
                                outcome,
                            })
                    }
                    (
                        DecisionSubject::TeamProposal(payload),
                        effect @ (Effect::TeamProposalSend | Effect::TeamProposalDismiss),
                    ) => {
                        let proposal = &payload.proposal;
                        if proposal.conversation_id == ctx.conversation.id
                            && !decision_queue::team_proposal_draft_blocked(
                                ctx.state,
                                decision_queue::Responder::Attached,
                            )
                        {
                            let send = matches!(effect, Effect::TeamProposalSend);
                            let frame = ClientFrame::TeamProposalDecision {
                                proposal: proposal.id.clone(),
                                decision: if send {
                                    crate::domain::models::TeamProposalDecision::Send
                                } else {
                                    crate::domain::models::TeamProposalDecision::Dismiss
                                },
                            };
                            if ctx.frame_tx.send(frame).is_ok() {
                                if send {
                                    let pending = super::begin_attached_team_send(
                                        ctx.state,
                                        &proposal.recipients,
                                        proposal.message_type,
                                    );
                                    ctx.team_requests
                                        .pending
                                        .insert(proposal.id.clone(), pending);
                                } else {
                                    crate::adapters::tui::handlers::team_command::show_team_send(
                                        ctx.state,
                                        "team-proposal-status",
                                        "Proposal dismissed. Nothing was sent.".into(),
                                    );
                                }
                                settle = true;
                            } else {
                                crate::adapters::tui::handlers::team_command::show_team_send(
                                    ctx.state,
                                    "team-proposal-status",
                                    "Attach socket closed — the proposal decision was not sent."
                                        .into(),
                                );
                            }
                        }
                        None
                    }
                    (
                        DecisionSubject::Consolidation(payload),
                        effect @ (Effect::ConsolidationAccept | Effect::ConsolidationDecline),
                    ) => {
                        let frame = decision_queue::consolidation_frame(
                            &key.source,
                            &payload,
                            matches!(effect, Effect::ConsolidationAccept),
                        );
                        if frame.is_none() {
                            decision_queue::withdraw_key(
                                ctx.state,
                                &key,
                                decision_queue::NO_LONGER_PENDING,
                            );
                        }
                        frame
                    }
                    (
                        DecisionSubject::TeamRetract(payload),
                        effect @ (Effect::TeamRetractConfirm | Effect::TeamRetractDecline),
                    ) => {
                        let confirm = matches!(effect, Effect::TeamRetractConfirm);
                        if let Some(request) = payload.request {
                            ctx.team_requests.open_retract.take();
                            if confirm {
                                ctx.team_requests.pending.insert(
                                    request.clone(),
                                    super::AttachedTeamRequest::Retract {
                                        next_sequence: 0,
                                        view: String::new(),
                                        view_done: false,
                                    },
                                );
                                crate::adapters::tui::handlers::team_command::show_team_retract(ctx.state, crate::adapters::tui::handlers::team_command::TEAM_RETRACT_SENDING.to_owned());
                            } else {
                                ctx.team_requests.pending.remove(&request);
                            }
                            let frame = ClientFrame::TeamRetractDecision {
                                request: request.clone(),
                                confirm,
                            };
                            if ctx.frame_tx.send(frame).is_ok() {
                                settle = true;
                            } else {
                                let pending = ctx
                                    .team_requests
                                    .pending
                                    .remove(&request)
                                    .unwrap_or(super::AttachedTeamRequest::RetractPreview);
                                super::paint_team_request_failure(
                                    ctx.state,
                                    pending,
                                    "could not enqueue command",
                                );
                            }
                            None
                        } else if let RetractTarget::AutoResponse { message_id } = payload.target {
                            if !confirm {
                                settle = true;
                                None
                            } else if retractable_rows(ctx.conversation)
                                .any(|row| row.id == message_id)
                            {
                                let sent = ctx
                                    .frame_tx
                                    .send(ClientFrame::RetractAutoResponse {
                                        message_id: message_id.clone(),
                                        target_seq: None,
                                    })
                                    .is_ok();
                                if sent {
                                    retract_landed(ctx, &message_id);
                                    settle = true;
                                }
                                None
                            } else {
                                ctx.info("that message can no longer be retracted — nothing sent");
                                settle = true;
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if frame.is_some_and(|frame| ctx.frame_tx.send(frame).is_ok()) {
                    settle = true;
                }
                if settle {
                    decision_queue::settle_sent(ctx.state, &log_facts(&ctx.conversation.id), &key);
                    if shows_view {
                        crate::adapters::tui::handlers::scroll::dispatch_scroll(
                            ctx.view,
                            ctx.state,
                            crate::domain::models::view_state::ScrollDelta::ResumeFollowing,
                        );
                    }
                }
            }
            KeyFlow::Continue
        }
        // Guarded quit is universal, including cards that consume every key.
        None if command_in(Context::Global, overlay_key(&key))
            == Some(CommandId::Global(GlobalCmd::QuitRequest)) =>
        {
            request_quit(ctx)
        }
        None => handle_attached_key_inner(ctx, key).await,
    };
    let owner_after = (
        ctx.state.modal_stack.top().map(|frame| frame.id),
        ctx.state.pending_leader,
        crate::adapters::tui::app::focus_context(ctx.state),
    );
    if owner_before != owner_after {
        ctx.state.current_hint = crate::adapters::tui::hints::contextual_hint(
            &ctx.state.focus,
            ctx.state.session_count,
            ctx.state.hint_fade_sessions(),
            ctx.state.is_vscode,
            &ctx.state.modal_stack,
            ctx.state.pending_leader,
            false,
        );
    }
    flow
}

/// A key of the thread frame, the registry having resolved its context: the shared executor
/// applies the state-only part (scrolling, closing, composer prefill, overlay toggles); this
/// host loads pages and queues the writer frames.
async fn thread_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crate::adapters::tui::thread_inspector::{self, RetractPlan, ThreadOutcome};
    match thread_inspector::on_key(
        ctx.state,
        Key::from_crossterm(&key),
        Host::Attached,
        Some(&*ctx.conversation),
    ) {
        ThreadOutcome::Consumed => {}
        ThreadOutcome::Page { earlier } => {
            if let Err(error) = crate::infrastructure::runtime::thread_open::navigate(
                ctx.workspace,
                Some(&*ctx.conversation),
                ctx.state,
                earlier,
            )
            .await
            {
                thread_flash(ctx.state, error.to_string());
            }
        }
        ThreadOutcome::Toggle => {
            let toggled = ctx.state.thread_inspector.as_ref().and_then(|inspector| {
                let node = match &inspector.target {
                    crate::adapters::tui::state::ThreadTarget::Item(_) => inspector.node_id.clone(),
                    crate::adapters::tui::state::ThreadTarget::Node(node) => Some(node.clone()),
                    crate::adapters::tui::state::ThreadTarget::Sent { .. } => None,
                }?;
                Some((
                    node,
                    matches!(
                        inspector.target,
                        crate::adapters::tui::state::ThreadTarget::Item(_)
                    ),
                ))
            });
            if let Some((node, want_node)) = toggled
                && let Err(error) = open_thread_for_node(
                    ctx.workspace,
                    ctx.conversation,
                    &node,
                    ctx.state,
                    ctx.read_only,
                    want_node,
                )
                .await
            {
                thread_flash(ctx.state, error.to_string());
            }
        }
        // `x`: the retract confirmation is 21-6's decision on L3; nothing is retracted here.
        // The press re-checks the writer boundary (an observer never retracts).
        ThreadOutcome::Retract(plan) => {
            if ctx.read_only {
                return KeyFlow::Continue;
            }
            match plan {
                RetractPlan::Reply { message_id, .. } => begin_retract(ctx, Some(&message_id)),
                RetractPlan::Item { peer, item } => {
                    let request = ctx.team_requests.mint();
                    super::enqueue_attached_team_frame(
                        ctx.state,
                        ctx.team_requests,
                        ClientFrame::TeamRetractPreview {
                            request,
                            peer,
                            item_id: item,
                        },
                        ctx.frame_tx,
                    );
                }
            }
        }
        ThreadOutcome::Send { frame, joined } => {
            if ctx.read_only {
                return KeyFlow::Continue;
            }
            // The Busy fast path reuses the durable principal/task join, not the
            // presentation sidecar as an item lookup authority.
            #[cfg(feature = "a2a")]
            if let Some((node, address)) = joined {
                let found = match super::recipient_projection(ctx.workspace).await {
                    Ok(projection) => crate::domain::models::AgentId::from_peer_path(&node)
                        .ok()
                        .and_then(|node| {
                            crate::adapters::a2a::exec::recipient_item_for_node(&projection, &node)
                        }),
                    Err(error) => {
                        thread_flash(ctx.state, error.to_string());
                        return KeyFlow::Continue;
                    }
                };
                if found.is_none_or(|item| item.address != address) {
                    thread_flash(
                        ctx.state,
                        format!("recipient item '{}' not found", address.item().as_str()),
                    );
                    return KeyFlow::Continue;
                }
            }
            #[cfg(not(feature = "a2a"))]
            let _ = joined;
            super::queue_recipient_action(ctx.state, frame, ctx.frame_tx, false);
        }
    }
    KeyFlow::Continue
}

/// A key of the thread's reply field that is not text editing (the field edits itself in the
/// route): `Enter` runs the reply through the `/team reply` parse and dispatch the composer
/// uses, never through `state.input_buffer`, and closes the thread on an accepted dispatch;
/// `Esc` clears and leaves the field.
async fn thread_reply_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crate::adapters::tui::thread_inspector::{self, ReplyOutcome};
    let ReplyOutcome::Submit {
        item_id,
        text,
        discuss,
    } = thread_inspector::on_reply_key(ctx.state, Key::from_crossterm(&key))
    else {
        return KeyFlow::Continue;
    };
    if ctx.read_only {
        return KeyFlow::Continue;
    }
    let line = if discuss {
        format!("/team reply --discuss {item_id} {text}")
    } else {
        format!("/team reply {item_id} {text}")
    };
    let request = ctx.team_requests.mint();
    match super::attached_team_line_for_request(&line, request) {
        super::AttachedTeamLine::Frame(frame @ ClientFrame::ReplyToRecipientItem { .. }) => {
            if super::queue_recipient_action(ctx.state, frame, ctx.frame_tx, false) {
                thread_inspector::reply_sent(ctx.state);
            }
        }
        super::AttachedTeamLine::Refused(error) => thread_flash(ctx.state, error),
        _ => thread_flash(
            ctx.state,
            "that reply is not a /team reply — nothing sent".to_owned(),
        ),
    }
    KeyFlow::Continue
}

async fn handle_attached_key_inner(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    if ctx.state.autocomplete.is_open()
        && ctx.state.focus == FocusState::Input
        && let Some(flow) = popup_key(ctx, key).await
    {
        return flow;
    }

    let pressed = Key::from_crossterm(&key);
    let context = if ctx.state.focus == FocusState::Input {
        Context::Input
    } else {
        Context::Chat
    };
    match command_in(context, pressed) {
        Some(command) => execute_attached(ctx, command, key).await,
        None => {
            // No row: a printable key types into the Input composer; every other
            // key (a Ctrl/Alt letter included) is a consumed no-op (Story 21.3 AC12).
            if let Key::Char(c) = pressed
                && ctx.state.focus == FocusState::Input
            {
                type_char(ctx, c);
            }
            KeyFlow::Continue
        }
    }
}

/// The registry key behind a press as an overlay reads it: a navigation key held
/// with any modifier (Shift+Up, Alt+Enter) is unbound there.
fn overlay_key(key: &KeyEvent) -> Key {
    if matches!(key.code, KeyCode::Char(_)) || key.modifiers.is_empty() {
        Key::from_crossterm(key)
    } else {
        Key::Other
    }
}

/// The command `key` is bound to in `context` for the attached host.
fn command_in(context: Context, key: Key) -> Option<CommandId> {
    match keymap::resolve(context, Host::Attached, &[key]) {
        Resolved::Hit(row) => Some(row.command),
        _ => None,
    }
}

/// Type one printable character into the composer (Story 19.16g: read-only
/// composers edit too; only submission is refused).
fn type_char(ctx: &mut KeyCtx<'_>, c: char) {
    if !ctx.composer_locked() {
        let _ = crate::adapters::tui::app::type_char(ctx.state, c, None);
    }
}

/// Run one command the registry resolved for this host. Each arm is the effect
/// the key's hand-written arm ran before; the overlays' own edits (palette
/// filter, popup filter) stay with the shared local handlers they delegate to.
async fn execute_attached(ctx: &mut KeyCtx<'_>, command: CommandId, key: KeyEvent) -> KeyFlow {
    match command {
        // `i`, `q` and `?` are commands only in Chat focus.
        CommandId::Chat(ChatCmd::FocusInput | ChatCmd::Quit | ChatCmd::OpenHelp)
            if ctx.state.focus != FocusState::Chat => {}
        // Ctrl+X opens the untimed leader (RU-3); it is never a retract.
        CommandId::Global(GlobalCmd::OpenLeader) => {
            let _ = open_leader(ctx.state);
        }
        CommandId::Global(GlobalCmd::QuitRequest) | CommandId::Overlay(OverlayCmd::QuitRequest) => {
            return request_quit(ctx);
        }
        CommandId::Input(InputCmd::OpenPalette) | CommandId::Chat(ChatCmd::OpenPalette) => {
            open_palette(ctx).await;
        }
        CommandId::Input(InputCmd::CancelOrQuit) | CommandId::Chat(ChatCmd::CancelOrQuit) => {
            return ctrl_c(ctx);
        }
        CommandId::Chat(ChatCmd::Quit) => return request_quit(ctx),
        CommandId::Chat(ChatCmd::FocusInput) => {
            presentation::reducer::set_base(ctx.state, FocusState::Input);
            ctx.state.needs_redraw = true;
        }
        CommandId::Chat(ChatCmd::OpenHelp) => {
            presentation::reducer::open(ctx.state, Surface::Help);
        }
        // Esc never detaches, quits or submits: with no popup/overlay open it
        // moves focus between Input and Chat and preserves the unsent text.
        CommandId::Input(InputCmd::ReturnToChat) | CommandId::Chat(ChatCmd::FocusToggle) => {
            match ctx.state.focus {
                FocusState::Input => presentation::reducer::set_base(ctx.state, FocusState::Chat),
                FocusState::Chat => presentation::reducer::set_base(ctx.state, FocusState::Input),
            }
            ctx.state.needs_redraw = true;
        }
        CommandId::Input(InputCmd::ToggleMultiline) => {
            ctx.state.multiline_mode = !ctx.state.multiline_mode;
            ctx.state.needs_redraw = true;
        }
        CommandId::Input(
            cmd @ (InputCmd::SubmitOrNewline | InputCmd::InsertNewline | InputCmd::Send),
        ) => return enter_key(ctx, cmd).await,
        CommandId::Input(
            cmd @ (InputCmd::DeleteBack
            | InputCmd::DeleteForward
            | InputCmd::CursorLeft
            | InputCmd::CursorRight
            | InputCmd::CursorHome
            | InputCmd::CursorEnd
            | InputCmd::LineStart
            | InputCmd::LineEnd
            | InputCmd::DeleteWordBack
            | InputCmd::KillToLineStart
            | InputCmd::KillToLineEnd),
        ) => {
            if !ctx.composer_locked() {
                let _ = crate::adapters::tui::app::exec_input(
                    ctx.state,
                    cmd,
                    Key::from_crossterm(&key),
                );
            }
        }
        CommandId::Input(InputCmd::OpenSlashPopup) => {
            if ctx.state.input_buffer.is_empty() && !ctx.composer_locked() {
                // The shared composer owns the slash trigger: it saves the pre-trigger
                // composer so Esc can restore it exactly.
                let event = crate::domain::events::DomainInputEvent::KeyPress('/');
                let _ = handle_input(ctx.state, &event);
                ctx.chrome.sync_slash(ctx.state, ctx.workspace);
            } else {
                type_char(ctx, '/');
            }
        }
        // Ctrl+D / Ctrl+U scroll half a page (Chat only, never a detach); PgUp/PgDn a page.
        CommandId::Input(InputCmd::TranscriptPageUp | InputCmd::TranscriptPageDown)
        | CommandId::Chat(
            ChatCmd::ScrollPageUp
            | ChatCmd::ScrollPageDown
            | ChatCmd::ScrollHalfPageUp
            | ChatCmd::ScrollHalfPageDown,
        ) => {
            let half = matches!(
                command,
                CommandId::Chat(ChatCmd::ScrollHalfPageUp | ChatCmd::ScrollHalfPageDown)
            );
            if half && ctx.state.focus != FocusState::Chat {
                return KeyFlow::Continue;
            }
            let earlier = matches!(
                command,
                CommandId::Input(InputCmd::TranscriptPageUp)
                    | CommandId::Chat(ChatCmd::ScrollPageUp | ChatCmd::ScrollHalfPageUp)
            );
            use crate::domain::models::view_state::ScrollDelta;
            // Preserve Input/Chat's legacy stride, including streaming's reserved row.
            let rows = ctx.state.viewport_height / if half { 2 } else { 1 };
            let page = rows
                .saturating_sub(u16::from(ctx.streaming.is_streaming))
                .clamp(1, 5);
            let delta = if earlier {
                ScrollDelta::UnboundedRows(page as isize)
            } else {
                ScrollDelta::UnboundedRows(-(page as isize))
            };
            crate::adapters::tui::handlers::scroll::dispatch_scroll(ctx.view, ctx.state, delta);
        }
        CommandId::Overlay(OverlayCmd::PaletteClose) => {
            presentation::reducer::close(ctx.state, Surface::CommandPalette);
        }
        CommandId::Overlay(
            OverlayCmd::PaletteUp | OverlayCmd::PaletteDown | OverlayCmd::PaletteRun,
        )
        | CommandId::Field(_) => {
            return palette_delegate(ctx, key).await;
        }
        CommandId::Overlay(OverlayCmd::AutoCancel) => {
            cancel_autocomplete_popup(ctx.state);
            ctx.chrome.sync_slash(ctx.state, ctx.workspace);
            ctx.state.needs_redraw = true;
        }
        CommandId::Overlay(
            OverlayCmd::AutoPrev
            | OverlayCmd::AutoNext
            | OverlayCmd::AutoAccept
            | OverlayCmd::AutoBackspace,
        ) => popup_delegate(ctx, key),
        // Local-only commands never resolve in this host.
        CommandId::Viewer(_) | CommandId::Tool(_) | CommandId::Chat(ChatCmd::Inspect) => {}
        _ => {}
    }
    KeyFlow::Continue
}

/// Enter and its modified forms. Plain Enter is never mistaken for Ctrl+Enter.
async fn enter_key(ctx: &mut KeyCtx<'_>, command: InputCmd) -> KeyFlow {
    match command {
        InputCmd::InsertNewline => {
            if !ctx.composer_locked() {
                let _ = handle_input(
                    ctx.state,
                    &crate::domain::events::DomainInputEvent::SpecialKey(
                        crate::domain::events::DomainKey::ShiftEnter,
                    ),
                );
                ctx.state.needs_redraw = true;
            }
            KeyFlow::Continue
        }
        // Ctrl+Enter (where the terminal distinguishes it) sends.
        InputCmd::Send if ctx.state.input_buffer.is_empty() => KeyFlow::Continue,
        InputCmd::Send => submit_enter(ctx).await,
        _ if ctx.state.multiline_mode => {
            if !ctx.state.input_buffer.is_empty() && !ctx.composer_locked() {
                let _ = handle_input(
                    ctx.state,
                    &crate::domain::events::DomainInputEvent::SpecialKey(
                        crate::domain::events::DomainKey::ShiftEnter,
                    ),
                );
                ctx.state.needs_redraw = true;
            }
            KeyFlow::Continue
        }
        _ => submit_enter(ctx).await,
    }
}

/// `Ctrl+C`, after every modal/popup/overlay/card had its chance to own it.
fn ctrl_c(ctx: &mut KeyCtx<'_>) -> KeyFlow {
    if let Some(frame) = ctx.interrupt.interrupt_frame() {
        if ctx.frame_tx.send(frame).is_err() {
            crate::infrastructure::runtime::error_notices::attach_stop_not_sent(ctx.state);
        }
        return KeyFlow::Continue;
    }
    if ctx.read_only && ctx.streaming.is_streaming {
        let notice = nothing_to_stop_notice(true);
        notifications::notify(
            ctx.state,
            NoticeSource::Attached,
            NoticeSeverity::from_level(notice.level),
            notice.text,
        );
        return KeyFlow::Continue;
    }
    request_quit(ctx)
}

/// Ask to quit this client through the shared S-QUIT decision. Exits only on a
/// clean idle host or an explicit `y`; the daemon and its work are untouched.
fn request_quit(ctx: &mut KeyCtx<'_>) -> KeyFlow {
    let large_images = ctx
        .state
        .decisions
        .count_live(crate::adapters::tui::decision_queue::DecisionKind::LargeImage);
    let facts = QuitFacts {
        host: QuitHost::Attached,
        streams: usize::from(ctx.streaming.is_streaming),
        live_tools: ctx.streaming.active_tool_calls.len(),
        agents: 0,
        queued_turns: 0,
        composer_draft: !crate::adapters::tui::handlers::recipient_actions::pending_composition(
            ctx.state,
        )
        .unwrap_or(&ctx.state.input_buffer)
        .is_empty(),
        pending_images: ctx.state.pending_images.len() + large_images,
        peer_compose: crate::adapters::tui::handlers::recipient_actions::pending_composition(
            ctx.state,
        )
        .is_some()
            || crate::adapters::tui::handlers::recipient_actions::pending_field_reply(ctx.state)
            || crate::adapters::tui::thread_inspector::reply_field_has_text(ctx.state),
        pending_decisions: ctx.state.decisions.undecided() - large_images,
    };
    match presentation::reducer::request_quit(ctx.state, facts) {
        QuitRequest::ExitNow => KeyFlow::Quit,
        QuitRequest::Confirming => KeyFlow::Continue,
    }
}

// ── Ctrl+X leader ────────────────────────────────────────────────────────────

/// A follow-up to the open leader, resolved through the keymap registry (AC2):
/// `Esc`/`Ctrl+C` cancel silently, a key without a row posts one info notice,
/// a row this host cannot run answers with its remedy. The origin context is
/// retained without changing the underlying base focus.
async fn leader_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    let origin = ctx.state.pending_leader.take().unwrap_or(Context::Input);
    ctx.state.leader_feedback = None;
    ctx.state.needs_redraw = true;
    let key = Key::from_crossterm(&key);
    let origin_name = origin.name();
    let mut avail = attached_avail_ctx(ctx.state, ctx.read_only);
    avail.frame_origin = !ctx.state.modal_stack.is_empty();
    let row = match leader_step(Host::Attached, key, &avail, origin_name) {
        LeaderStep::Cancel => return KeyFlow::Continue,
        LeaderStep::NoBinding(text) => {
            refuse_leader(ctx.state, origin, None, NoticeSource::Chord, text.into());
            return KeyFlow::Continue;
        }
        LeaderStep::Unavailable(row, remedy) => {
            let source = if remedy == ATTACHED_SIGN_IN {
                NoticeSource::Login
            } else {
                NoticeSource::Chord
            };
            refuse_leader(ctx.state, origin, Some(row.command), source, remedy.into());
            return KeyFlow::Continue;
        }
        LeaderStep::Run(row) => row,
    };
    let CommandId::Leader(command) = row.command else {
        return KeyFlow::Continue;
    };
    match command {
        LeaderCmd::Help => {
            presentation::reducer::open(ctx.state, Surface::Help);
        }
        LeaderCmd::A2aLog => {
            // The existing attached `/team log` local read.
            let args = crate::adapters::tui::handlers::team_command::TeamLogArgs::default();
            open_team_log(ctx, &args).await;
        }
        LeaderCmd::Notifications => {
            // S-LOG pushes above the exact frame that started this leader.
            notification_log::open(ctx.state);
        }
        LeaderCmd::ClearToasts => {
            let _ = ctx.state.notifications.clear_toasts();
            ctx.state.needs_redraw = true;
        }
        LeaderCmd::Decisions => {
            crate::adapters::tui::decision_queue::open_waiting(
                ctx.state,
                &log_facts(&ctx.conversation.id),
            );
        }
        LeaderCmd::Detach => return KeyFlow::Detach,
        // The shared boundary intercepts Ctrl+Q before dismissing a legacy menu.
        LeaderCmd::QuitRequest => return request_quit(ctx),
        // Portable multiline send: the same path as Enter, exactly once.
        LeaderCmd::Send if origin == Context::Input => {
            if !ctx.state.input_buffer.is_empty() {
                return submit_enter(ctx).await;
            }
        }
        LeaderCmd::Send => {
            let text = no_binding_text(key, origin_name);
            notifications::info(ctx.state, NoticeSource::Chord, text);
        }
        // Every other row is unavailable in this host (answered above).
        _ => {}
    }
    KeyFlow::Continue
}

/// Facts the registry's availability rows read in the attached host.
pub(super) fn attached_avail_ctx(state: &TuiState, read_only: bool) -> AvailCtx {
    let mut avail = AvailCtx::attached(state.terminal_width, read_only);
    avail.frame_origin = !state.modal_stack.is_empty();
    avail
}

/// The attached status row's rightmost tail (A10).
pub(super) fn status_tail(state: &TuiState) -> StatusTail {
    if !state.modal_stack.is_empty() {
        return StatusTail::Hidden;
    }
    if state.pending_leader.is_some() {
        StatusTail::Pending(PendingPrefix::Leader)
    } else {
        StatusTail::Menu
    }
}

/// Apply the shared S-LOG boundary's host effects. The attached client has no
/// retry path, so `Retry` is never offered or acted on.
fn apply_log_outcome(ctx: &mut KeyCtx<'_>, outcome: LogOutcome) -> KeyFlow {
    match outcome {
        LogOutcome::Copy(text) => {
            crate::adapters::tui::handlers::clipboard::notify_copy(
                ctx.state,
                &text,
                &crate::adapters::tui::clipboard::copy_to_clipboard,
            );
        }
        LogOutcome::Closed | LogOutcome::Consumed | LogOutcome::Retry { .. } => {}
        LogOutcome::Quit => return request_quit(ctx),
    }
    ctx.state.needs_redraw = true;
    KeyFlow::Continue
}

async fn open_team_log(
    ctx: &mut KeyCtx<'_>,
    args: &crate::adapters::tui::handlers::team_command::TeamLogArgs,
) {
    for notice in crate::infrastructure::runtime::transparency_bridge::attached_team_log(
        ctx.log_service,
        ctx.state,
        args,
    )
    .await
    {
        ctx.conversation.messages.push(system_message(notice));
    }
    crate::adapters::tui::handlers::scroll::dispatch_scroll(
        ctx.view,
        ctx.state,
        crate::domain::models::view_state::ScrollDelta::ResumeFollowing,
    );
}

// ── slash popup ──────────────────────────────────────────────────────────────

/// A key while the slash popup is open. `Some` = consumed; `None` = the popup
/// was dismissed by an unrelated key which then continues as a normal key.
async fn popup_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> Option<KeyFlow> {
    let k = overlay_key(&key);
    match command_in(Context::Autocomplete, k) {
        Some(command) => Some(execute_attached(ctx, command, key).await),
        // A printable key edits the popup's filter.
        None if matches!(k, Key::Char(_)) => {
            popup_delegate(ctx, key);
            Some(KeyFlow::Continue)
        }
        None => {
            ctx.state.autocomplete.dismiss();
            ctx.state.needs_redraw = true;
            None
        }
    }
}

/// Shared Help/popup dispatch using the authoritative composer geometry.
fn popup_delegate(ctx: &mut KeyCtx<'_>, key: KeyEvent) {
    let help = ctx
        .state
        .modal_stack
        .top()
        .is_some_and(|f| f.surface == Surface::Help);
    if let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default()) {
        let _ = if help {
            crate::adapters::tui::app::dispatch_frame_key(
                ctx.state,
                Context::Help,
                overlay_key(&key),
                Host::Attached,
            )
        } else {
            handle_input(ctx.state, &event)
        };
    }
    if !help {
        ctx.chrome.sync_slash(ctx.state, ctx.workspace);
    }
}

// ── command palette ──────────────────────────────────────────────────────────

async fn open_palette(ctx: &mut KeyCtx<'_>) {
    ctx.chrome.begin_palette(ctx.conversation, ctx.read_only);
    presentation::reducer::open(ctx.state, Surface::CommandPalette);
    ctx.chrome.sync_palette(ctx.state, ctx.workspace).await;
}

async fn palette_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    let k = overlay_key(&key);
    match command_in(Context::CommandPalette, k) {
        Some(command) => execute_attached(ctx, command, key).await,
        // A printable key types into the filter; every other rowless key is inert.
        None if matches!(k, Key::Char(_)) => palette_delegate(ctx, key).await,
        None => KeyFlow::Continue,
    }
}

/// Hand a palette key to the shared palette handler and run its selected entry.
async fn palette_delegate(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crate::adapters::tui::app::InputAction;
    let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default()) else {
        return KeyFlow::Continue;
    };
    let action = crate::adapters::tui::app::with_card_focus_reconcile(ctx.state, |state| {
        crate::adapters::tui::app::dispatch_frame_key(
            state,
            Context::CommandPalette,
            Key::from_domain(&event).unwrap_or(Key::Other),
            Host::Attached,
        )
    });
    ctx.chrome.sync_palette(ctx.state, ctx.workspace).await;
    match action {
        InputAction::ExecuteCommand { name, args } => run_palette_command(ctx, name, args).await,
        _ => KeyFlow::Continue,
    }
}

/// Run the entry the palette just selected through the same attached command
/// paths a typed line uses. The palette never touches the composer draft.
async fn run_palette_command(ctx: &mut KeyCtx<'_>, name: String, args: Option<String>) -> KeyFlow {
    if name == RETRACT_AUTO {
        begin_retract(ctx, args.as_deref());
        return KeyFlow::Continue;
    }
    if name == "quit" && args.is_none() {
        // Ask with the real composer so its unsent draft is counted without being cleared.
        return request_quit(ctx);
    }
    let text = match args {
        Some(args) => format!("/{name} {args}"),
        None => format!("/{name}"),
    };
    let cursor = ctx.state.cursor_position;
    let scroll = ctx.state.input_scroll_offset;
    let draft = std::mem::replace(&mut ctx.state.input_buffer, text);
    ctx.state.cursor_position = ctx.state.input_buffer.chars().count();
    let flow = submit_enter(ctx).await;
    ctx.state.input_buffer = draft;
    ctx.state.cursor_position = cursor;
    ctx.state.input_scroll_offset = scroll;
    flow
}

/// Open the existing retract confirmation for one REAL retractable row.
fn begin_retract(ctx: &mut KeyCtx<'_>, message_id: Option<&str>) {
    if ctx.read_only {
        ctx.info("read-only — can't retract here");
        return;
    }
    let Some(message_id) = message_id else {
        ctx.info("no auto-sent message to retract");
        return;
    };
    let target = retractable_rows(ctx.conversation)
        .find(|row| row.id == message_id)
        .map(|row| (row.id.clone(), preview_line(&row.content, 48)));
    match target {
        Some((message_id, preview)) => {
            use crate::adapters::tui::decision_queue::{
                self, Responder, RetractTarget, TeamRetractPayload,
            };
            decision_queue::raise_team_retract(
                ctx.state,
                TeamRetractPayload {
                    target: RetractTarget::AutoResponse { message_id },
                    conversation_id: String::new(),
                    principal: None,
                    task: None,
                    card: format!(
                        "Retract this auto-sent message? {preview}\nRetract — marked in your log, never deleted"
                    ),
                    armed: true,
                    request: None,
                },
                Responder::Attached,
            );
        }
        None => ctx.info("that message can no longer be retracted — nothing sent"),
    }
    ctx.state.needs_redraw = true;
}

/// The retract of an auto-sent reply was sent: its result lands in the outcomes of the thread
/// window of the item the reply answered (an ambient Info notice too when that window is not on
/// top), else as one Info notice (Story 21.8 A14; never a transcript row).
fn retract_landed(ctx: &mut KeyCtx<'_>, message_id: &str) {
    use crate::adapters::daemon::protocol::RecipientActionTarget;
    const RESULT: &str =
        "Retracted. Marked in your log — never deleted. What was already read, was read.";
    let item = ctx
        .conversation
        .peer_row_controls
        .get(message_id)
        .and_then(|control| control.item_address.as_ref())
        .map(|address| address.item().as_str().to_owned())
        .or_else(|| {
            // A reply row without a control is tied to its item by its node.
            let node = message_id.strip_prefix("peer-response-")?;
            let inspector = ctx.state.thread_inspector.as_ref()?;
            (inspector.node_id.as_deref() == Some(node))
                .then(|| inspector.item_address())
                .flatten()
                .map(|address| address.item().as_str().to_owned())
        });
    match item {
        Some(item_id) => crate::adapters::tui::handlers::recipient_actions::show_notice(
            ctx.state,
            Some(&RecipientActionTarget::Item { item_id }),
            RESULT.to_owned(),
        ),
        None => notifications::info(ctx.state, NoticeSource::Team, RESULT),
    }
}

// ── submission ───────────────────────────────────────────────────────────────

/// Attached-local presentation slash commands, answered before the composer
/// routes a line to the daemon.
enum LocalSlash {
    Detach,
    Multiline,
    Quit,
    /// `/notifications`: open S-LOG.
    Notifications,
    /// A known built-in this client does not handle: refused, never model input.
    Unavailable(&'static str),
}

fn local_slash(text: &str) -> Option<LocalSlash> {
    let (name, _) = crate::domain::services::slash_command::parse_slash_command(text)?;
    match name {
        "detach" => Some(LocalSlash::Detach),
        "ml" => Some(LocalSlash::Multiline),
        "quit" => Some(LocalSlash::Quit),
        "notifications" => Some(LocalSlash::Notifications),
        other if builtin_commands::attached_unavailable(other) => {
            builtin_commands::find(other).map(|builtin| LocalSlash::Unavailable(builtin.name))
        }
        _ => None,
    }
}

fn run_local_slash(ctx: &mut KeyCtx<'_>, local: LocalSlash) -> KeyFlow {
    if matches!(local, LocalSlash::Quit) {
        // The typed `/quit` line is a command, not an unsent draft: consume it, but
        // leave every other unsent fact untouched so cancelling loses nothing (RU-14, AC7).
        ctx.state.input_buffer.clear();
        ctx.state.cursor_position = 0;
        ctx.state.input_scroll_offset = 0;
        ctx.state.needs_redraw = true;
        return request_quit(ctx);
    }
    // A command is consumed like the existing refusals: the draft is cleared.
    ctx.state.input_buffer.clear();
    ctx.state.cursor_position = 0;
    ctx.state.input_scroll_offset = 0;
    ctx.state.needs_redraw = true;
    match local {
        LocalSlash::Detach => KeyFlow::Detach,
        LocalSlash::Multiline => {
            ctx.state.multiline_mode = !ctx.state.multiline_mode;
            KeyFlow::Continue
        }
        LocalSlash::Notifications => {
            notification_log::open(ctx.state);
            KeyFlow::Continue
        }
        LocalSlash::Quit => request_quit(ctx),
        LocalSlash::Unavailable(name) => {
            notifications::info(
                ctx.state,
                NoticeSource::Attached,
                format!(
                    "/{name} is not available in an attached session. Detach ({detach}) and use it from a standalone `rustain` session.",
                    detach = crate::adapters::tui::keymap::render::leader_follow_up_long(
                        crate::adapters::tui::keymap::table::LeaderCmd::Detach
                    )
                ),
            );
            KeyFlow::Continue
        }
    }
}

/// The one submission path: slash interception, the team router, peer compose
/// and the model turn. Enter, Ctrl+Enter and `Ctrl+X Enter` all end here.
async fn submit_enter(ctx: &mut KeyCtx<'_>) -> KeyFlow {
    if let Some(local) = local_slash(&ctx.state.input_buffer) {
        return run_local_slash(ctx, local);
    }
    let text = match attached_composer_key_with_team_requests(
        KeyCode::Enter,
        ctx.read_only,
        ctx.state,
        ctx.frame_tx,
        ctx.team_requests,
    ) {
        ComposerOutcome::Handled => return KeyFlow::Continue,
        ComposerOutcome::FrameSent => {
            crate::adapters::tui::handlers::scroll::dispatch_scroll(
                ctx.view,
                ctx.state,
                crate::domain::models::view_state::ScrollDelta::ResumeFollowing,
            );
            return KeyFlow::Continue;
        }
        // Story 19.16g — a user-initiated log may scroll its own result into view.
        ComposerOutcome::LocalLog(args) => {
            open_team_log(ctx, &args).await;
            return KeyFlow::Continue;
        }
        ComposerOutcome::LocalView(item_id) => {
            let access = if ctx.read_only {
                crate::adapters::tui::state::ThreadAccess::ReadOnly
            } else {
                crate::adapters::tui::state::ThreadAccess::Writer
            };
            if let Err(error) = crate::infrastructure::runtime::thread_open::open_received(
                ctx.workspace,
                Some(ctx.conversation),
                &item_id,
                ctx.state,
                access,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            {
                notifications::warning(ctx.state, NoticeSource::Attached, error.to_string());
            }
            return KeyFlow::Continue;
        }
        ComposerOutcome::Submit(text) => text,
    };
    let submission = UserSubmission {
        text,
        images: vec![],
        synthetic: false,
        activation_set: None,
        agent_snapshot: None,
        turn_cancel: CancellationToken::new(),
    };
    let view = TurnViewState {
        conversation: ctx.conversation,
        streaming: ctx.streaming,
        state: ctx.state,
        lifecycle: ctx.lifecycle,
        session_manager: ctx.session_manager,
    };
    ctx.driver.submit(submission, view).await;
    crate::adapters::tui::handlers::scroll::dispatch_scroll(
        ctx.view,
        ctx.state,
        crate::domain::models::view_state::ScrollDelta::ResumeFollowing,
    );
    KeyFlow::Continue
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::apply_attached_approval_frame;
    use super::*;
    use crate::adapters::daemon::protocol::ProposalToken;
    use crate::adapters::daemon::protocol::{DaemonFrame, InterruptRefusal, TurnGeneration};
    use crate::adapters::tui::decision_queue::{self, DecisionKind, DecisionSubject};
    use crate::adapters::tui::keymap::render::leader_menu;
    use crate::adapters::tui::keymap::resolve::{Resolved, resolve, rows_of};
    use crate::adapters::tui::keymap::table::GlobalCmd;
    use crate::adapters::tui::keymap::table_chat::ChatCmd;
    use crate::adapters::tui::keymap::table_input::InputCmd;
    use crate::adapters::tui::keymap::{self, Availability, Context};
    use crate::domain::models::SendAuthorization;
    use crate::domain::models::session::SessionState;
    use std::collections::BTreeSet;

    /// Every local `run_attached` the helper needs, owned by the test.
    pub(crate) struct Rig {
        workspace: tempfile::TempDir,
        read_only: bool,
        pub(crate) state: TuiState,
        pub(crate) tx: mpsc::UnboundedSender<ClientFrame>,
        clock: std::sync::Arc<crate::domain::clock::MockClock>,
        assemblies: std::collections::HashMap<
            crate::domain::models::RequestId,
            crate::adapters::daemon::protocol::ExactInputAssembly,
        >,
        rx: mpsc::UnboundedReceiver<ClientFrame>,
        pub(crate) team_requests: AttachedTeamRequests,
        tab: TabRenderState,
        pub(crate) domain_tab: crate::domain::models::tab::TabState,
        interrupt: AttachedInterrupt,
        driver: SocketTurnDriver,
        lifecycle: TuiTurnLifecycle,
        session_manager: SessionManager,
        log_service: crate::infrastructure::transparency::TransparencyService,
        chrome: AttachedChrome,
    }

    impl Rig {
        pub(crate) fn new(read_only: bool) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let (tx, rx) = mpsc::unbounded_channel();
            let log_service = crate::infrastructure::transparency::TransparencyService::new(
                std::sync::Arc::new(
                    crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                        workspace.path(),
                    ),
                ),
                workspace.path().to_path_buf(),
            );
            let mut state = TuiState::new(80, 24);
            let clock = std::sync::Arc::new(crate::domain::clock::MockClock::new(
                std::time::Instant::now(),
            ));
            state.notifications =
                crate::adapters::tui::notifications::NotificationStore::from_config(
                    &crate::domain::models::NotificationsConfig::default(),
                    clock.clone(),
                );
            state.focus = FocusState::Input;
            Self {
                workspace,
                read_only,
                state,
                clock,
                assemblies: std::collections::HashMap::new(),
                driver: SocketTurnDriver::new(tx.clone()),
                tx,
                rx,
                team_requests: AttachedTeamRequests::default(),
                tab: TabRenderState::default(),
                domain_tab: crate::domain::models::tab::TabState::from_conversation(
                    0,
                    Conversation::default(),
                ),
                interrupt: AttachedInterrupt::new(read_only),
                lifecycle: TuiTurnLifecycle::default(),
                session_manager: SessionManager::new(SessionState::Empty),
                log_service,
                chrome: AttachedChrome::new(),
            }
        }

        pub(crate) fn approval_frame(&mut self, frame: DaemonFrame) {
            apply_attached_approval_frame(
                &mut self.state,
                &mut self.assemblies,
                frame,
                self.read_only,
            );
        }

        pub(crate) fn ready(&mut self) {
            self.paint();
            self.clock
                .advance(crate::adapters::tui::decision_queue::INERT_AFTER_PAINT);
        }

        async fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) -> KeyFlow {
            self.raw_key(KeyEvent::new(code, modifiers)).await
        }

        /// The front door's context, borrowed field by field from the rig.
        fn key_ctx(&mut self) -> KeyCtx<'_> {
            KeyCtx {
                workspace: self.workspace.path(),
                read_only: self.read_only,
                state: &mut self.state,
                frame_tx: &self.tx,
                team_requests: &mut self.team_requests,
                tab: &mut self.tab,
                conversation: &mut self.domain_tab.conversation,
                streaming: &mut self.domain_tab.streaming,
                view: &mut self.domain_tab.view_state,
                interrupt: &self.interrupt,
                driver: &self.driver,
                lifecycle: &mut self.lifecycle,
                session_manager: &mut self.session_manager,
                log_service: &self.log_service,
                chrome: &mut self.chrome,
            }
        }

        pub(crate) async fn raw_key(&mut self, key: KeyEvent) -> KeyFlow {
            let mut ctx = self.key_ctx();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                handle_attached_key(&mut ctx, key),
            )
            .await
            .expect("attached key deadline")
        }

        /// The composer's submission path (`Enter`) as the loop runs it, for a composer
        /// line set while a frame owns the keys (a typed key would be the frame's).
        async fn submit_composer(&mut self) -> KeyFlow {
            let mut ctx = self.key_ctx();
            tokio::time::timeout(std::time::Duration::from_secs(10), submit_enter(&mut ctx))
                .await
                .expect("attached submit deadline")
        }

        /// The scratch workspace whose journal the thread openers read.
        pub(crate) fn workspace(&self) -> &std::path::Path {
            self.workspace.path()
        }

        /// Open the one thread frame on `item_id` through the production opener and paint
        /// it once, so the viewer has its fitted rows (a scroll key clamps against them).
        async fn open_thread(
            &mut self,
            item_id: &str,
            access: crate::adapters::tui::state::ThreadAccess,
        ) {
            crate::infrastructure::runtime::thread_open::open_received(
                self.workspace.path(),
                Some(&self.domain_tab.conversation),
                item_id,
                &mut self.state,
                access,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .expect("open thread");
            self.paint();
        }

        async fn ch(&mut self, c: char) -> KeyFlow {
            self.press(KeyCode::Char(c), KeyModifiers::NONE).await
        }

        async fn ctrl(&mut self, c: char) -> KeyFlow {
            self.press(KeyCode::Char(c), KeyModifiers::CONTROL).await
        }

        async fn esc(&mut self) -> KeyFlow {
            self.press(KeyCode::Esc, KeyModifiers::NONE).await
        }

        async fn enter(&mut self) -> KeyFlow {
            self.press(KeyCode::Enter, KeyModifiers::NONE).await
        }

        pub(crate) async fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                assert_eq!(self.ch(c).await, KeyFlow::Continue, "typing {c:?}");
            }
        }

        pub(crate) fn frames(&mut self) -> Vec<ClientFrame> {
            let mut frames = Vec::new();
            while let Ok(frame) = self.rx.try_recv() {
                frames.push(frame);
            }
            frames
        }

        pub(crate) fn close_receiver(&mut self) {
            self.rx.close();
        }

        pub(crate) async fn paste(&mut self, text: &str) {
            let pending = false;
            self.chrome
                .paste(&mut self.state, text, pending, self.workspace.path())
                .await;
        }

        pub(crate) fn paint(&mut self) -> ratatui::Terminal<ratatui::backend::TestBackend> {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                self.state.terminal_width,
                self.state.terminal_height,
            ))
            .unwrap();
            let mut rendered = None;
            terminal
                .draw(|frame| {
                    rendered = super::super::paint_attached(
                        frame,
                        &mut self.state,
                        &self.domain_tab.conversation,
                        &self.domain_tab.streaming,
                        &self.domain_tab.view_state,
                        &mut self.tab,
                        &std::collections::HashMap::new(),
                        crate::domain::models::PermissionMode::Normal,
                        &crate::adapters::tui::state::AttachInfo {
                            read_only: self.read_only,
                            channel_count: 1,
                        },
                    );
                })
                .unwrap();
            if rendered.is_some() {
                super::super::after_attached_draw(&mut self.state, true);
            }
            terminal
        }

        fn feedback(&self, id: &str) -> Option<&str> {
            self.state
                .feedback_blocks
                .get(id)
                .map(|block| block.message.as_str())
        }

        /// The newest store notice from `source` (message text): the attached
        /// notices of Story 21.1 are records, never feedback blocks or flashes.
        fn notice(&self, source: NoticeSource) -> Option<&str> {
            self.state
                .notifications
                .records()
                .rev()
                .find(|record| record.source == source)
                .map(|record| record.message.as_str())
        }

        fn retractable(&mut self, id: &str, content: &str) {
            let mut row = system_message(content);
            row.id = id.to_owned();
            row.send_authorization = SendAuthorization::Automatic;
            self.domain_tab.conversation.messages.push(row);
        }

        fn consolidation(&mut self, token: u64) {
            super::super::apply_attached_consolidation_frame(
                &mut self.state,
                DaemonFrame::ConsolidationProposed {
                    token: ProposalToken(token),
                    proposals: vec![],
                },
                self.read_only,
            );
        }

        fn open_kind(&mut self, kind: DecisionKind) {
            let key = self
                .state
                .decisions
                .items()
                .find(|item| item.kind == kind)
                .unwrap()
                .key();
            if decision_queue::focused_key(&self.state).as_ref() != Some(&key) {
                assert!(decision_queue::open(&mut self.state, &key));
            }
            self.ready();
        }

        fn preview_retract(&mut self, armed: bool) {
            self.team_requests.pending.insert(
                "retract-a".into(),
                super::super::AttachedTeamRequest::RetractPreview,
            );
            super::super::apply_attached_team_frame(
                &mut self.state,
                &mut self.team_requests,
                DaemonFrame::TeamRetractPreviewed {
                    request: "retract-a".into(),
                    preview: crate::adapters::daemon::protocol::TeamRetractPreviewView::Card {
                        peer: "peer-a".into(),
                        item_id: "item-a".into(),
                        body: "team retract canary".into(),
                        armed,
                    },
                },
                false,
                &self.tx,
            );
        }

        fn palette_names(&self) -> Vec<String> {
            self.state
                .command_palette
                .filtered_entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect()
        }
    }

    fn user_text(frame: &ClientFrame) -> Option<&str> {
        match frame {
            ClientFrame::UserMessage { text, .. } => Some(text),
            _ => None,
        }
    }

    #[tokio::test]
    async fn story21_4_attached_draft_and_caret_are_state_owned() {
        for read_only in [false, true] {
            let mut rig = Rig::new(read_only);
            rig.state.input_buffer = "甲e\u{301}tail".to_owned();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();
            rig.state.cursor_position = 3;
            rig.ch('界').await;
            assert_eq!(rig.state.input_buffer, "甲e\u{301}界tail");
            assert_eq!(rig.state.cursor_position, 4);
            assert!(rig.frames().is_empty(), "editing never submits");
            rig.ctrl('a').await;
            assert_eq!(rig.state.cursor_position, 0);
            rig.ch('先').await;
            assert_eq!(rig.state.input_buffer, "先甲e\u{301}界tail");
            rig.ctrl('e').await;
            assert_eq!(
                rig.state.cursor_position,
                rig.state.input_buffer.chars().count()
            );
        }
    }

    #[test]
    fn story21_4_attached_paste_inserts_at_authoritative_caret() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "甲尾".to_owned();
        state.cursor_position = 1;
        handle_attached_paste(&mut state, "e\u{301}👩‍💻", false);
        assert_eq!(state.input_buffer, "甲e\u{301}👩‍💻尾");
        assert_eq!(state.cursor_position, 6);
        assert_eq!(
            state.cursor_position,
            state.input_buffer.chars().count() - 1
        );
    }

    // 7B-09: with a Messages frame on top, the attached host's one A20 remedy still answers the
    // chords it does not carry; closing the window could never make them available.
    #[tokio::test]
    async fn story21_7_attached_chords_with_messages_on_top_give_the_one_a20_remedy() {
        use crate::adapters::tui::handlers::team_command::log_blocks;
        use crate::adapters::tui::keymap::table::CLOSE_WINDOW_FIRST;
        for read_only in [false, true] {
            for key in ['s', 't', 'r', 'e', 'a', 'h', 'w'] {
                let mut rig = Rig::new(read_only);
                crate::adapters::tui::mission_control::open_messages(
                    &mut rig.state,
                    false,
                    &log_blocks(&[], false),
                    None,
                );
                assert!(!rig.state.modal_stack.is_empty(), "Messages is on top");
                let frames = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
                rig.ctrl('x').await;
                rig.ch(key).await;
                let message = rig
                    .notice(NoticeSource::Chord)
                    .unwrap_or_else(|| panic!("{read_only} {key}: no chord notice"));
                assert!(message.contains("shows Messages only"), "{key}: {message}");
                assert_ne!(message, CLOSE_WINDOW_FIRST, "{key}");
                assert!(rig.frames().is_empty(), "{key}: nothing sent");
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    frames,
                    "{key}: the frame is untouched"
                );
            }
        }
    }

    // ── AC6 / AC13: the leader and every A5 follow-up ────────────────────────

    #[tokio::test]
    async fn story21_2_attached_leader_routes_every_a5_follow_up() {
        let mut exercised: BTreeSet<String> = BTreeSet::new();

        // Capability-qualified unavailable chords: the same logical command's
        // explicit result with its real remedy, shown as a store notice.
        let unavailable: [(char, NoticeSource, &str); 11] = [
            ('p', NoticeSource::Chord, "rustain profile"),
            ('m', NoticeSource::Login, "rustain auth login"),
            ('i', NoticeSource::Login, "rustain auth login"),
            ('a', NoticeSource::Chord, "shows Messages only"),
            ('s', NoticeSource::Chord, "shows Messages only"),
            ('r', NoticeSource::Chord, "shows Messages only"),
            ('e', NoticeSource::Chord, "shows Messages only"),
            ('t', NoticeSource::Chord, "shows Messages only"),
            ('u', NoticeSource::Chord, "/team status"),
            ('w', NoticeSource::Chord, "shows Messages only"),
            ('h', NoticeSource::Chord, "shows Messages only"),
        ];
        for (key, source, remedy) in unavailable {
            let mut rig = Rig::new(false);
            rig.type_text("draft").await;
            assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
            assert!(rig.state.pending_leader.is_some(), "{key}: leader open");
            assert_eq!(rig.ch(key).await, KeyFlow::Continue, "{key}");
            assert!(rig.state.pending_leader.is_none(), "{key}: leader consumed");
            assert_eq!(rig.state.focus, FocusState::Input, "{key}: origin restored");
            assert_eq!(rig.state.input_buffer, "draft", "{key}: draft untouched");
            assert!(
                rig.state.feedback_blocks.is_empty(),
                "{key}: a notice, never a transcript feedback block"
            );
            let message = rig
                .notice(source)
                .unwrap_or_else(|| panic!("{key}: no {source:?} notice"));
            assert!(message.contains(remedy), "{key}: {message}");
            let row = keymap::table()
                .iter()
                .find(|row| {
                    row.context == Context::Leader
                        && row.hosts.includes(Host::Attached)
                        && row.keys == [Key::Char(key)].as_slice()
                })
                .unwrap();
            assert!(
                matches!(
                    row.availability(&AvailCtx::attached(80, false)),
                    Availability::Unavailable(r) if r == message
                ),
                "{key}: result is the registry row's remedy verbatim"
            );
            assert!(
                rig.frames().is_empty(),
                "{key}: an unavailable chord sends nothing"
            );
            exercised.insert(key.to_ascii_uppercase().to_string());
        }

        // `i`/`m` reuse the existing sign-in refusal: the typed `/login` notice.
        let mut typed = Rig::new(false);
        typed.type_text("/login").await;
        typed.state.autocomplete.dismiss();
        typed.enter().await;
        assert_eq!(typed.notice(NoticeSource::Login), Some(ATTACHED_SIGN_IN));

        // `l`: the same attached `/team log` local read as the typed command.
        let mut leader = Rig::new(false);
        leader.ctrl('x').await;
        assert_eq!(leader.ch('l').await, KeyFlow::Continue);
        let mut typed = Rig::new(false);
        typed.type_text("/team log").await;
        typed.enter().await;
        let log_view = |rig: &Rig| {
            let frame = rig.state.modal_stack.top().expect("log frame");
            assert_eq!(frame.surface, Surface::MissionControl);
            assert_eq!(
                rig.state.mission_control.tab,
                crate::adapters::tui::mission_control::McTab::Messages
            );
            assert_eq!(
                rig.state.mission_control.title(),
                "Mission Control · Messages"
            );
            let page = rig.state.mission_control.messages.as_ref().unwrap();
            assert!(!page.filtered);
            page.content.clone()
        };
        assert_eq!(log_view(&leader), log_view(&typed));
        assert!(screen(&mut leader).contains("Mission Control · Messages"));
        assert!(screen(&mut typed).contains("Mission Control · Messages"));
        assert!(leader.feedback("team-log").is_none());
        assert!(typed.feedback("team-log").is_none());
        assert!(leader.frames().is_empty(), "the log is a local read");
        exercised.insert("L".to_owned());

        // `d`: attached-only clean detach; no frame is sent by the key itself.
        let mut rig = Rig::new(false);
        rig.ctrl('x').await;
        assert_eq!(rig.ch('d').await, KeyFlow::Detach);
        assert!(rig.frames().is_empty());
        exercised.insert("D".to_owned());

        // `?`: the shared help overlay; Esc closes it and Input text survives.
        let mut rig = Rig::new(false);
        rig.type_text("keep me").await;
        rig.ctrl('x').await;
        assert_eq!(rig.ch('?').await, KeyFlow::Continue);
        assert!(presentation::is_open(&rig.state, Surface::Help));
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(!presentation::is_open(&rig.state, Surface::Help));
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.state.input_buffer, "keep me");
        exercised.insert("?".to_owned());

        // `Enter`: portable send — exactly one message through the submit path.
        let mut rig = Rig::new(false);
        rig.state.multiline_mode = true;
        rig.type_text("line one").await;
        assert_eq!(
            rig.press(KeyCode::Enter, KeyModifiers::ALT).await,
            KeyFlow::Continue
        );
        rig.type_text("line two").await;
        assert!(rig.frames().is_empty(), "nothing sent yet");
        rig.ctrl('x').await;
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        let frames = rig.frames();
        assert_eq!(frames.len(), 1, "sent exactly once");
        assert_eq!(user_text(&frames[0]), Some("line one\nline two"));
        assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
        exercised.insert("Enter".to_owned());

        // `Esc`: cancels and restores the origin.
        let mut rig = Rig::new(false);
        rig.state.focus = FocusState::Chat;
        rig.ctrl('x').await;
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(rig.state.pending_leader.is_none());
        assert_eq!(rig.state.focus, FocusState::Chat);
        exercised.insert("Esc".to_owned());

        // `n`: S-LOG opens over the origin focus; Esc closes it and the draft
        // is untouched.
        let mut rig = Rig::new(false);
        rig.type_text("keep me").await;
        rig.ctrl('x').await;
        assert_eq!(rig.ch('n').await, KeyFlow::Continue);
        assert!(rig.state.notification_log.is_some());
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(rig.state.notification_log.is_none());
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.state.input_buffer, "keep me");
        exercised.insert("N".to_owned());

        // `x`: archives every visible toast; the log keeps the records.
        let mut rig = Rig::new(false);
        notifications::info(&mut rig.state, NoticeSource::Attached, "one");
        assert_eq!(rig.state.notifications.visible().count(), 1);
        rig.ctrl('x').await;
        assert_eq!(rig.ch('x').await, KeyFlow::Continue);
        assert_eq!(rig.state.notifications.visible().count(), 0);
        assert_eq!(rig.state.notifications.records().len(), 1);
        exercised.insert("X".to_owned());

        // `q`: the attached client owns no decision pane, so the chord answers
        // honestly instead of painting `(n/a)` or doing nothing.
        let mut rig = Rig::new(false);
        rig.ctrl('x').await;
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        assert_eq!(rig.state.notifications.visible().count(), 1);
        assert!(rig.frames().is_empty(), "a local answer, nothing sent");
        exercised.insert("Q".to_owned());

        // The painted menu inventory equals the exercised routes, so hiding an
        // attached chord (or painting one without a caller) fails here.
        let painted: BTreeSet<String> = leader_menu(&AvailCtx::attached(80, false))
            .into_iter()
            .map(|item| item.key)
            .collect();
        assert_eq!(painted, exercised);
    }

    #[tokio::test]
    async fn story21_2_attached_leader_is_untimed_cancels_and_consumes_unknown_once() {
        let mut rig = Rig::new(false);
        rig.type_text("keep").await;
        rig.ctrl('x').await;
        // The attached host has no timeout state (nothing ticks the menu away) and
        // its resize arm only stores the terminal size, so there is no resize logic
        // to exercise here; the leader simply stays open until a follow-up.
        assert!(
            rig.state.pending_leader.is_some(),
            "untimed: stays open until a follow-up"
        );
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(rig.state.pending_leader.is_none());
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.state.input_buffer, "keep");

        // An unknown follow-up is consumed once with the origin restored.
        let mut rig = Rig::new(false);
        rig.state.focus = FocusState::Chat;
        rig.ctrl('x').await;
        assert_eq!(rig.ch('z').await, KeyFlow::Continue);
        assert!(rig.state.pending_leader.is_none());
        assert_eq!(rig.state.focus, FocusState::Chat);
        assert!(rig.state.feedback_blocks.is_empty());
        // Story 21.3 AC2: consumed aloud, with exactly one info record.
        assert_eq!(rig.state.notifications.records().len(), 1);
        assert_eq!(
            rig.notice(NoticeSource::Chord),
            Some("No binding for Ctrl+X z in Chat")
        );
        // …exactly once: the next `i` is the Chat command, not a second follow-up.
        rig.ch('i').await;
        assert_eq!(rig.state.focus, FocusState::Input);

        // Ctrl+X Enter on an empty composer sends nothing.
        let mut rig = Rig::new(false);
        rig.ctrl('x').await;
        rig.enter().await;
        assert!(rig.frames().is_empty());
    }

    // ── AC8 / AC13: Esc, Ctrl+D, Ctrl+C and q ────────────────────────────────

    #[tokio::test]
    async fn story21_2_attached_esc_ctrl_d_ctrl_c_never_detach() {
        let mut rig = Rig::new(false);
        rig.type_text("unsent").await;
        // Esc: Input → Chat → Input, text preserved, never an exit.
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert_eq!(rig.state.focus, FocusState::Chat);
        assert_eq!(rig.state.input_buffer, "unsent");
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert_eq!(rig.state.focus, FocusState::Input);
        // Ctrl+D is inert in Input.
        assert_eq!(rig.ctrl('d').await, KeyFlow::Continue);
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.state.input_buffer, "unsent");
        // Ctrl+D in Chat is a half-page scroll toward newer rows.
        rig.esc().await;
        rig.state.viewport_height = 20;
        rig.state.total_content_height = 200;
        rig.domain_tab.view_state.scroll_offset = 12;
        rig.domain_tab.view_state.mode = crate::domain::models::AnchorMode::Reading;
        assert_eq!(rig.ctrl('d').await, KeyFlow::Continue);
        assert_eq!(rig.domain_tab.view_state.scroll_offset, 7);
        // Ctrl+C with a draft asks first; it never detaches and keeps the draft.
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(rig.state.quit_confirm.is_some());
        assert_eq!(rig.state.input_buffer, "unsent");
        assert!(rig.frames().is_empty(), "no frame: nothing is detached");
    }

    #[tokio::test]
    async fn story21_2_attached_q_is_text_in_input_and_a_quit_request_in_chat() {
        let mut rig = Rig::new(false);
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        assert_eq!(rig.state.input_buffer, "q", "q in Input is plain text");
        assert!(rig.state.quit_confirm.is_none());
        // Chat: with a draft the guard opens; the draft survives.
        rig.esc().await;
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        assert!(rig.state.quit_confirm.is_some());
        assert_eq!(rig.state.input_buffer, "q");
        rig.esc().await; // cancel the frame
        assert!(rig.state.quit_confirm.is_none());
        assert_eq!(rig.state.focus, FocusState::Chat, "caller focus restored");

        // A clean idle host exits directly.
        let mut idle = Rig::new(false);
        idle.esc().await;
        assert_eq!(idle.ch('q').await, KeyFlow::Quit);
        // Ctrl+Q quits from any focus (Input here), guarded the same way.
        let mut idle = Rig::new(false);
        assert_eq!(idle.ctrl('q').await, KeyFlow::Quit);
        let mut busy = Rig::new(false);
        busy.type_text("draft").await;
        assert_eq!(busy.ctrl('q').await, KeyFlow::Continue);
        assert!(busy.state.quit_confirm.is_some());
    }

    #[tokio::test]
    async fn story21_2_attached_quit_frame_owns_keys_and_exits_only_on_y() {
        let mut rig = Rig::new(false);
        rig.type_text("draft").await;
        rig.retained_arrives(false);
        rig.ctrl('q').await;
        let facts = rig.state.quit_confirm.as_ref().unwrap().facts.clone();
        assert!(facts.composer_draft);
        assert_eq!(facts.pending_decisions, 1);
        assert_eq!(facts.host, QuitHost::Attached);
        // Anything else is swallowed: `y`-less letters, even a card's own key,
        // and nothing edits the draft or answers the card underneath.
        for c in ['x', 'a', 'z'] {
            assert_eq!(rig.ch(c).await, KeyFlow::Continue);
            assert!(rig.state.quit_confirm.is_some(), "{c} consumed");
        }
        assert_eq!(rig.state.input_buffer, "draft");
        assert_eq!(rig.state.decisions.undecided(), 1);
        assert!(rig.frames().is_empty());
        // n, Esc, Ctrl+C and Enter each cancel without exiting.
        for cancel in [
            (KeyCode::Char('n'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Enter, KeyModifiers::NONE),
        ] {
            assert!(rig.state.quit_confirm.is_some());
            assert_eq!(rig.press(cancel.0, cancel.1).await, KeyFlow::Continue);
            assert!(rig.state.quit_confirm.is_none(), "{cancel:?} cancels");
            assert_eq!(rig.state.input_buffer, "draft");
            assert_eq!(
                rig.state.decisions.undecided(),
                1,
                "the card was never answered"
            );
            assert!(rig.frames().is_empty());
            rig.ctrl('q').await;
        }
        // Explicit `y` is the only exit, and it answers no card.
        assert_eq!(rig.ch('y').await, KeyFlow::Quit);
        assert!(rig.state.quit_confirm.is_none());
        assert_eq!(rig.state.decisions.undecided(), 1);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_2_attached_ctrl_c_interrupts_quits_idle_or_refuses_read_only() {
        // A projected live turn: Ctrl+C sends the interrupt and stays attached.
        let mut rig = Rig::new(false);
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.domain_tab.streaming.is_streaming = true;
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        let frames = rig.frames();
        assert!(matches!(
            frames.as_slice(),
            [ClientFrame::InterruptTurn {
                generation: TurnGeneration(7)
            }]
        ));
        assert!(rig.state.quit_confirm.is_none());

        // An open overlay owns Ctrl+C: it closes, no interrupt is sent.
        rig.ctrl('p').await;
        assert!(presentation::is_open(&rig.state, Surface::CommandPalette));
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(!presentation::is_open(&rig.state, Surface::CommandPalette));
        assert!(rig.frames().is_empty());
        rig.ctrl('x').await;
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(rig.state.pending_leader.is_none());
        assert!(rig.frames().is_empty());

        // S-QUIT: Ctrl+C cancels the frame only.
        rig.ctrl('q').await;
        assert!(rig.state.quit_confirm.is_some());
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(rig.state.quit_confirm.is_none());
        assert!(rig.frames().is_empty());

        // Idle writer: a clean host exits directly.
        let mut idle = Rig::new(false);
        assert_eq!(idle.ctrl('c').await, KeyFlow::Quit);

        // Read-only with live work: an explicit refusal, never a detach.
        let mut viewer = Rig::new(true);
        viewer.domain_tab.streaming.is_streaming = true;
        assert_eq!(viewer.ctrl('c').await, KeyFlow::Continue);
        // Story 21.1 AC6: the refusal is a store notice (toast + S-LOG), not a
        // transcript row.
        assert_eq!(
            viewer
                .state
                .notifications
                .records()
                .last()
                .map(|r| r.message.clone()),
            Some(InterruptRefusal::ReadOnly.to_string())
        );
        assert!(viewer.domain_tab.conversation.messages.is_empty());
        assert!(viewer.frames().is_empty());
        // Read-only and idle follows the same quit rule as a writer.
        let mut viewer = Rig::new(true);
        assert_eq!(viewer.ctrl('c').await, KeyFlow::Quit);
    }

    // ── AC12: multiline and send ─────────────────────────────────────────────

    #[tokio::test]
    async fn story21_2_attached_multiline_toggle_newline_and_send() {
        let mut rig = Rig::new(false);
        // Alt+M toggles (and is not typed as text); Ctrl+E never toggles.
        assert!(!rig.state.multiline_mode);
        rig.press(KeyCode::Char('m'), KeyModifiers::ALT).await;
        assert!(rig.state.multiline_mode);
        assert!(
            rig.state.input_buffer.is_empty(),
            "Alt+M keeps its modifier"
        );
        rig.type_text("a").await;
        assert_eq!(rig.ctrl('e').await, KeyFlow::Continue);
        assert!(rig.state.multiline_mode, "Ctrl+E is end-of-line, no toggle");
        assert_eq!(rig.state.input_buffer, "a");
        // Plain Enter in multiline mode inserts a newline and never sends.
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert_eq!(rig.state.input_buffer, "a\n");
        assert!(rig.frames().is_empty());
        rig.type_text("b").await;
        // Esc never submits, even with multiline text.
        rig.esc().await;
        rig.esc().await;
        assert!(rig.frames().is_empty());
        assert_eq!(rig.state.input_buffer, "a\nb");
        // Ctrl+Enter sends exactly once through the submit path.
        assert_eq!(
            rig.press(KeyCode::Enter, KeyModifiers::CONTROL).await,
            KeyFlow::Continue
        );
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("a\nb"));
        assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
        // Alt+M again turns it off; plain Enter then sends.
        rig.press(KeyCode::Char('m'), KeyModifiers::ALT).await;
        assert!(!rig.state.multiline_mode);
        rig.type_text("single").await;
        rig.enter().await;
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("single"));
        // Alt+Enter / Shift+Enter always insert a newline, never send.
        rig.type_text("x").await;
        rig.press(KeyCode::Enter, KeyModifiers::ALT).await;
        rig.press(KeyCode::Enter, KeyModifiers::SHIFT).await;
        assert_eq!(rig.state.input_buffer, "x\n\n");
        assert!(rig.frames().is_empty());
        // Ctrl+Enter on an empty composer is inert.
        let mut empty = Rig::new(false);
        empty.press(KeyCode::Enter, KeyModifiers::CONTROL).await;
        assert!(empty.frames().is_empty());
    }

    // ── slash commands ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn story21_2_attached_detach_ml_quit_commands() {
        let mut rig = Rig::new(false);
        rig.type_text("/detach").await;
        // The Esc-restoring popup is open for the slash; Enter first completes.
        rig.state.autocomplete.dismiss();
        assert_eq!(rig.enter().await, KeyFlow::Detach);
        assert!(
            rig.frames().is_empty(),
            "the key sends no frame; the loop sends Detach"
        );
        assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());

        let mut rig = Rig::new(false);
        rig.type_text("/ml").await;
        rig.state.autocomplete.dismiss();
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert!(rig.state.multiline_mode, "/ml toggles on");
        assert!(rig.frames().is_empty());
        rig.type_text("/ml").await;
        rig.state.autocomplete.dismiss();
        rig.press(KeyCode::Enter, KeyModifiers::CONTROL).await;
        assert!(!rig.state.multiline_mode, "/ml toggles off");

        let mut rig = Rig::new(false);
        rig.type_text("/quit").await;
        rig.state.autocomplete.dismiss();
        assert_eq!(rig.enter().await, KeyFlow::Quit);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_2_attached_known_unavailable_builtin_is_refused_not_sent() {
        for name in ["mode", "config", "export", "new", "plan"] {
            let mut rig = Rig::new(false);
            rig.type_text(&format!("/{name} yolo")).await;
            rig.state.autocomplete.dismiss();
            assert_eq!(rig.enter().await, KeyFlow::Continue);
            assert!(rig.frames().is_empty(), "/{name} must not reach the model");
            let message = rig
                .notice(NoticeSource::Attached)
                .unwrap_or_else(|| panic!("/{name}: no refusal"));
            assert!(
                message.starts_with(&format!("/{name} is not available in an attached session")),
                "{message}"
            );
            assert!(
                rig.state.input_buffer.is_empty(),
                "the refused command is consumed"
            );
        }
        // Typed auth refusals keep their existing result and send nothing.
        let mut rig = Rig::new(false);
        rig.type_text("/accounts").await;
        rig.state.autocomplete.dismiss();
        rig.enter().await;
        assert!(rig.notice(NoticeSource::Accounts).is_some());
        assert!(rig.frames().is_empty());
        // A custom (non built-in) command keeps its daemon passthrough.
        let mut rig = Rig::new(false);
        rig.type_text("/my-custom arg").await;
        rig.state.autocomplete.dismiss();
        rig.enter().await;
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("/my-custom arg"));
    }

    #[tokio::test]
    async fn story21_2_attached_team_retract_stays_gated() {
        // Read-only: refused aloud, nothing sent.
        let mut rig = Rig::new(true);
        rig.state.focus = FocusState::Input;
        rig.type_text("/team retract peer-a item-1").await;
        rig.state.autocomplete.dismiss();
        rig.enter().await;
        assert!(rig.frames().is_empty());
        assert_eq!(
            rig.notice(NoticeSource::Attached),
            Some("read-only — can't retract here")
        );
        // Writer: only the PREVIEW request goes out (confirmation comes from
        // the daemon-served card); no retraction effect and no model turn.
        let mut rig = Rig::new(false);
        rig.type_text("/team retract peer-a item-1").await;
        rig.state.autocomplete.dismiss();
        rig.enter().await;
        let frames = rig.frames();
        assert!(matches!(
            frames.as_slice(),
            [ClientFrame::TeamRetractPreview { peer, item_id, .. }]
                if peer == "peer-a" && item_id == "item-1"
        ));
    }

    // ── AC3 / AC13: palette and the retract route ────────────────────────────

    #[tokio::test]
    async fn story21_2_attached_palette_lists_only_attached_commands_and_reopens_populated() {
        let mut rig = Rig::new(false);
        rig.ctrl('p').await;
        assert!(presentation::is_open(&rig.state, Surface::CommandPalette));
        let names = rig.palette_names();
        for expected in ["/clear", "/team", "/ml", "/quit", "/detach"] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} in {names:?}"
            );
        }
        for hidden in ["/mode", "/config", "/new", "/export", "/login", "new tab"] {
            assert!(
                !names.iter().any(|n| n == hidden),
                "{hidden} leaked: {names:?}"
            );
        }
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("mode:") || n.starts_with('!')),
            "local-only actions leaked: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "retract: no auto-sent message"),
            "an unavailable action is labelled before selection: {names:?}"
        );
        // Close and reopen: populated again, not a false empty list.
        rig.esc().await;
        assert!(!presentation::is_open(&rig.state, Surface::CommandPalette));
        assert_eq!(rig.state.focus, FocusState::Input);
        rig.ctrl('p').await;
        assert_eq!(rig.palette_names(), names);
        // Deleting a typed filter back to empty lists everything again.
        rig.type_text("detach").await;
        assert!(rig.palette_names().len() < names.len());
        for _ in 0.."detach".len() {
            rig.press(KeyCode::Backspace, KeyModifiers::NONE).await;
        }
        assert_eq!(rig.palette_names(), names);
    }

    #[tokio::test]
    async fn story21_4_bb2a_attached_paste_populates_before_paint_and_keeps_the_draft() {
        use ratatui::style::Modifier;
        for read_only in [false, true] {
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let mut rig = Rig::new(read_only);
                rig.state.input_buffer = ["retained draft"; 8].join("\n");
                rig.state.cursor_position = rig.state.input_buffer.chars().count();
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                let draft = rig.state.input_buffer.clone();
                let allocation = (
                    rig.state.input_buffer.as_ptr(),
                    rig.state.input_buffer.capacity(),
                );
                let stage = (
                    rig.domain_tab.view_state.scroll_offset,
                    matches!(
                        rig.domain_tab.view_state.mode,
                        crate::domain::models::AnchorMode::Following
                    ),
                );
                rig.ctrl('p').await;
                let frame = rig.state.modal_stack.top().unwrap().clone();
                let all = rig.palette_names();
                assert!(all.len() > 1 && all.iter().any(|name| name == "/ml"));
                let refreshes = rig.chrome.gate.refresh_count();
                rig.chrome
                    .paste(&mut rig.state, "/ml", false, rig.workspace.path())
                    .await;
                assert_eq!(rig.state.command_palette.filter_text.as_str(), "/ml");
                assert_eq!(
                    rig.palette_names(),
                    ["/ml"],
                    "actual populate effect, before any paint/key"
                );
                assert_eq!(rig.chrome.gate.refresh_count(), refreshes + 1);
                rig.key(Key::Home).await;
                rig.key(Key::Right).await;
                assert_eq!(
                    rig.chrome.gate.refresh_count(),
                    refreshes + 1,
                    "caret motion is not filtering"
                );
                rig.chrome
                    .paste(&mut rig.state, "界", false, rig.workspace.path())
                    .await;
                assert_eq!(rig.state.command_palette.filter_text.as_str(), "/界ml");
                assert!(
                    rig.palette_names().is_empty(),
                    "pasted query updates the rows, not just text"
                );
                rig.key(Key::Left).await;
                let snapshot = rig.state.command_palette.filter_text.clone();
                let caret_count = |rig: &Rig| {
                    paint_buffer_at(&rig.state, width, height, &rig.state.input_buffer)
                        .content
                        .iter()
                        .filter(|cell| {
                            cell.symbol() == "界" && cell.modifier.contains(Modifier::REVERSED)
                        })
                        .count()
                };
                assert_eq!(
                    caret_count(&rig),
                    1,
                    "{read_only} {width}x{height}: fitted actual query caret"
                );
                rig.ctrl('x').await;
                assert!(rig.state.pending_leader.is_some());
                assert_eq!(
                    caret_count(&rig),
                    0,
                    "the leader owns input, not the covered field"
                );
                let refreshes = rig.chrome.gate.refresh_count();
                rig.chrome
                    .paste(&mut rig.state, "blocked", false, rig.workspace.path())
                    .await;
                assert_eq!(rig.state.command_palette.filter_text, snapshot);
                assert_eq!(rig.chrome.gate.refresh_count(), refreshes);
                rig.esc().await;
                assert_eq!(caret_count(&rig), 1);
                rig.key(Key::Home).await;
                rig.ctrl('k').await;
                assert_eq!(
                    rig.palette_names(),
                    all,
                    "kill-line restores the unfiltered list through the real gate"
                );
                assert_eq!(rig.state.modal_stack.top(), Some(&frame));
                assert_eq!(rig.state.input_buffer, draft);
                assert_eq!(
                    (
                        rig.state.input_buffer.as_ptr(),
                        rig.state.input_buffer.capacity()
                    ),
                    allocation
                );
                assert_eq!(
                    (
                        rig.domain_tab.view_state.scroll_offset,
                        matches!(
                            rig.domain_tab.view_state.mode,
                            crate::domain::models::AnchorMode::Following
                        )
                    ),
                    stage
                );
                assert!(
                    rig.frames().is_empty(),
                    "no pasted key commands or submissions"
                );
            }
        }
    }

    #[tokio::test]
    async fn story21_4_attached_every_printable_inserts_at_each_shared_query_caret() {
        for (surface, read_only) in [
            Surface::CommandPalette,
            Surface::NotificationLog,
            Surface::Help,
        ]
        .into_iter()
        .flat_map(|surface| [false, true].map(move |read_only| (surface, read_only)))
        {
            let mut rig = Rig::new(read_only);
            rig.state.input_buffer = "draft".into();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();
            open_shared_query(&mut rig, surface).await;
            for ch in (0x20u8..0x7f).map(char::from) {
                rig.chrome
                    .paste(&mut rig.state, "ab", false, rig.workspace.path())
                    .await;
                rig.key(Key::Home).await;
                rig.key(Key::Right).await;
                let frame = rig.state.modal_stack.top().unwrap().clone();
                rig.ch(ch).await;
                assert_eq!(
                    shared_query(&rig, surface).as_str(),
                    format!("a{ch}b"),
                    "{read_only}: {ch:?}"
                );
                assert_eq!(rig.state.modal_stack.top(), Some(&frame));
                assert_eq!(rig.state.input_buffer, "draft");
                assert!(rig.frames().is_empty());
                assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
                rig.key(Key::Home).await;
                rig.ctrl('k').await;
                assert!(shared_query(&rig, surface).is_empty());
            }
        }
    }

    #[tokio::test]
    async fn story21_4_attached_readline_keys_edit_graphemes_in_every_shared_query() {
        const SEED: &str = "a界e\u{301} z";
        const FAMILY: &str = "a👩‍👩‍👧‍👦b";
        let cases: &[(Key, &str, &[Key], &str, usize)] = &[
            (Key::Ctrl('a'), SEED, &[], SEED, 0),
            (Key::Home, SEED, &[], SEED, 0),
            (Key::Ctrl('e'), SEED, &[Key::Home], SEED, 6),
            (Key::End, SEED, &[Key::Home], SEED, 6),
            (Key::Left, SEED, &[], SEED, 5),
            (Key::Right, SEED, &[Key::Home], SEED, 1),
            (
                Key::Delete,
                SEED,
                &[Key::Home, Key::Right],
                "ae\u{301} z",
                1,
            ),
            (
                Key::Backspace,
                SEED,
                &[Key::Home, Key::Right, Key::Right],
                "ae\u{301} z",
                1,
            ),
            (
                Key::Ctrl('h'),
                SEED,
                &[Key::Home, Key::Right, Key::Right],
                "ae\u{301} z",
                1,
            ),
            (Key::Ctrl('w'), SEED, &[], "a界e\u{301} ", 5),
            (
                Key::Ctrl('u'),
                SEED,
                &[Key::Home, Key::Right, Key::Right],
                "e\u{301} z",
                0,
            ),
            (
                Key::Ctrl('k'),
                SEED,
                &[Key::Home, Key::Right, Key::Right],
                "a界",
                2,
            ),
            (Key::Delete, FAMILY, &[Key::Home, Key::Right], "ab", 1),
            (
                Key::Backspace,
                FAMILY,
                &[Key::Home, Key::Right, Key::Right],
                "ab",
                1,
            ),
            (
                Key::Ctrl('h'),
                SEED,
                &[Key::Home, Key::Right, Key::Right, Key::Right],
                "a界 z",
                2,
            ),
            (
                Key::Ctrl('k'),
                "a\r\nb",
                &[Key::Home, Key::Left],
                "a\r\nb",
                1,
            ),
            (
                Key::Ctrl('k'),
                "a\r\nb",
                &[Key::Home, Key::Left, Key::Home],
                "\r\nb",
                0,
            ),
            (Key::Ctrl('k'), "a\r\nb", &[Key::Home], "a\r\n", 3),
            (Key::Ctrl('k'), "a\r\nb", &[], "a\r\nb", 4),
            (
                Key::Ctrl('u'),
                "a\r\nb",
                &[Key::Home, Key::Left],
                "\r\nb",
                0,
            ),
            (Key::Ctrl('u'), "a\r\nb", &[Key::Home], "a\r\nb", 3),
            (Key::Ctrl('u'), "a\r\nb", &[], "a\r\n", 3),
            (
                Key::Ctrl('w'),
                "a\r\nb",
                &[Key::Home, Key::Left],
                "\r\nb",
                0,
            ),
            (Key::Ctrl('w'), "a\r\nb", &[Key::Home], "b", 0),
            (Key::Ctrl('w'), "a\r\nb", &[], "a\r\n", 3),
            (Key::Backspace, "a\r\nb", &[Key::Home], "ab", 1),
            (
                Key::Backspace,
                "a\r\nb",
                &[Key::Home, Key::Left],
                "\r\nb",
                0,
            ),
            (Key::Ctrl('h'), "a\r\nb", &[Key::Home], "ab", 1),
            (Key::Delete, "a\r\nb", &[Key::Home, Key::Left], "ab", 1),
            (
                Key::Delete,
                "a\r\nb",
                &[Key::Home, Key::Left, Key::Home],
                "\r\nb",
                0,
            ),
            (Key::Delete, "a\r\nb", &[], "a\r\nb", 4),
        ];
        for (surface, read_only) in [
            Surface::CommandPalette,
            Surface::NotificationLog,
            Surface::Help,
        ]
        .into_iter()
        .flat_map(|surface| [false, true].map(move |read_only| (surface, read_only)))
        {
            for &(edit, seed, prefix, expected, cursor) in cases {
                let mut rig = Rig::new(read_only);
                rig.state.input_buffer = "untouched draft".into();
                rig.state.cursor_position = rig.state.input_buffer.chars().count();
                open_shared_query(&mut rig, surface).await;
                let frame = rig.state.modal_stack.top().unwrap().clone();
                let stage = (
                    rig.domain_tab.view_state.scroll_offset,
                    matches!(
                        rig.domain_tab.view_state.mode,
                        crate::domain::models::AnchorMode::Following
                    ),
                );
                rig.chrome
                    .paste(&mut rig.state, seed, false, rig.workspace.path())
                    .await;
                assert_eq!(
                    shared_query(&rig, surface).as_str(),
                    seed,
                    "actual paste control"
                );
                for &key in prefix {
                    rig.key(key).await;
                }
                assert_eq!(rig.key(edit).await, KeyFlow::Continue);
                assert_eq!(
                    shared_query(&rig, surface).as_str(),
                    expected,
                    "{read_only}: {edit:?} {seed}"
                );
                assert_eq!(
                    shared_query(&rig, surface).cursor(),
                    cursor,
                    "{read_only}: {edit:?} {seed}"
                );
                assert_eq!(rig.state.modal_stack.top(), Some(&frame));
                assert_eq!(
                    (
                        rig.domain_tab.view_state.scroll_offset,
                        matches!(
                            rig.domain_tab.view_state.mode,
                            crate::domain::models::AnchorMode::Following
                        )
                    ),
                    stage
                );
                assert_eq!(rig.state.input_buffer, "untouched draft");
                assert!(rig.frames().is_empty());
                assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
            }
        }
    }

    #[tokio::test]
    async fn story21_2_attached_palette_runs_commands_without_touching_the_draft() {
        let mut rig = Rig::new(false);
        rig.type_text("half-written").await;
        rig.ctrl('p').await;
        rig.type_text("ml").await;
        assert_eq!(rig.palette_names().first().map(String::as_str), Some("/ml"));
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert!(rig.state.multiline_mode, "palette /ml toggles");
        assert_eq!(
            rig.state.input_buffer, "half-written",
            "the draft is never the command"
        );
        assert!(!presentation::is_open(&rig.state, Surface::CommandPalette));
        assert_eq!(rig.state.focus, FocusState::Input);
        assert!(rig.frames().is_empty());

        // B-1: palette `quit` with a half-written draft asks first: the draft is
        // counted, untouched, and nothing exits.
        let mut quitting = Rig::new(false);
        quitting.type_text("half written").await;
        quitting.ctrl('p').await;
        quitting.type_text("quit").await;
        assert_eq!(
            quitting.enter().await,
            KeyFlow::Continue,
            "a draft must raise S-QUIT, never exit"
        );
        assert!(quitting.state.quit_confirm.is_some());
        assert_eq!(
            quitting.state.input_buffer, "half written",
            "the draft survives the ask"
        );
        assert!(quitting.frames().is_empty());

        rig.ctrl('p').await;
        rig.type_text("detach").await;
        assert_eq!(rig.enter().await, KeyFlow::Detach);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_palette_retract_selects_a_real_row_and_keeps_every_gate() {
        let mut rig = Rig::new(false);
        rig.retractable("peer-response-node-a", "first auto reply");
        rig.retractable("peer-response-node-b", "second auto reply");
        // Ctrl+X is the leader now — never an immediate retract.
        rig.ctrl('x').await;
        assert!(rig.state.pending_leader.is_some());
        assert!(!decision_queue::team_retract_pending(&rig.state));
        rig.esc().await;

        rig.ctrl('p').await;
        rig.type_text("retract").await;
        let names = rig.palette_names();
        let retract_names: Vec<&String> = names
            .iter()
            .filter(|n| n.starts_with("retract: "))
            .collect();
        assert_eq!(retract_names.len(), 2, "one entry per REAL row: {names:?}");
        // Select the older row (second entry sorts by name: "…first…" < "…second…").
        let first = names
            .iter()
            .position(|n| n.contains("first auto reply"))
            .unwrap();
        for _ in 0..first {
            rig.press(KeyCode::Down, KeyModifiers::NONE).await;
        }
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        // Preview + explicit confirmation: nothing was sent yet.
        assert!(rig.state.decisions.items().any(|item| matches!(
            &item.subject, DecisionSubject::TeamRetract(payload)
                if matches!(&payload.target, decision_queue::RetractTarget::AutoResponse { message_id } if message_id == "peer-response-node-a")
        )));
        assert!(rig.frames().is_empty(), "no immediate effect");
        // `n` declines; reopen and confirm with `y`.
        rig.open_kind(DecisionKind::TeamRetract);
        rig.ch('n').await;
        assert!(!decision_queue::team_retract_pending(&rig.state) && rig.frames().is_empty());
        assert_eq!(
            rig.state.focus,
            FocusState::Input,
            "decline returns borrowed focus"
        );
        rig.ctrl('p').await;
        rig.type_text("retract").await;
        for _ in 0..first {
            rig.press(KeyCode::Down, KeyModifiers::NONE).await;
        }
        rig.enter().await;
        rig.open_kind(DecisionKind::TeamRetract);
        rig.ch('y').await;
        let frames = rig.frames();
        assert!(matches!(
            frames.as_slice(),
            [ClientFrame::RetractAutoResponse { message_id, target_seq: None }]
                if message_id == "peer-response-node-a"
        ));
        assert!(rig.frames().is_empty());

        // A target retracted after opening cannot be sent again.
        let mut rig = Rig::new(false);
        rig.retractable("peer-response-node-a", "auto reply");
        assert_eq!(rig.state.focus, FocusState::Input);
        rig.state.input_buffer = "saved retract draft".into();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        rig.ctrl('p').await;
        rig.type_text("retract").await;
        rig.enter().await;
        assert!(decision_queue::team_retract_pending(&rig.state));
        assert_eq!(rig.state.input_buffer, "saved retract draft");
        rig.open_kind(DecisionKind::TeamRetract);
        rig.domain_tab.conversation.messages[0].retracted_at_ms = Some(1);
        rig.ch('y').await;
        assert!(rig.frames().is_empty(), "stale target never retracts");
        assert!(!decision_queue::team_retract_pending(&rig.state));

        // Read-only: the entry is labelled unavailable and selecting it refuses.
        let mut rig = Rig::new(true);
        rig.state.focus = FocusState::Input;
        rig.retractable("peer-response-node-a", "auto reply");
        rig.ctrl('p').await;
        rig.type_text("retract").await;
        assert!(
            rig.state.command_palette.filtered_entries[0]
                .description
                .contains("read-only")
        );
        rig.enter().await;
        assert!(!decision_queue::team_retract_pending(&rig.state));
        assert!(rig.frames().is_empty());
        assert_eq!(
            rig.notice(NoticeSource::Attached),
            Some("read-only — can't retract here")
        );
    }

    // ── AC4: slash popup ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn story21_2_attached_slash_popup_lists_attached_commands_and_esc_restores() {
        let mut rig = Rig::new(false);
        rig.ch('/').await;
        assert!(rig.state.autocomplete.is_open());
        assert_eq!(rig.state.input_buffer, "/");
        let names: Vec<String> = rig
            .state
            .autocomplete
            .suggestions
            .iter()
            .map(|s| match s {
                AutocompleteSuggestion::SlashCommand { name, .. } => name.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            ["clear", "team", "ml", "quit", "notifications", "detach"]
        );
        rig.type_text("cl").await;
        assert_eq!(rig.state.autocomplete.suggestions.len(), 1);
        // Esc closes the popup and restores the exact pre-trigger composer.
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(!rig.state.autocomplete.is_open());
        assert_eq!(rig.state.input_buffer, "");
        assert_eq!(
            rig.state.focus,
            FocusState::Input,
            "Esc closed only the popup"
        );

        // Tab completes; the completed command then runs through the router.
        rig.type_text("/cl").await;
        rig.press(KeyCode::Tab, KeyModifiers::NONE).await;
        assert_eq!(rig.state.input_buffer, "/clear");
        assert!(!rig.state.autocomplete.is_open());
        rig.enter().await;
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("/clear"));

        // A slash after other text is plain text — no popup.
        rig.type_text("a/").await;
        assert!(!rig.state.autocomplete.is_open());
        assert_eq!(rig.state.input_buffer, "a/");
    }

    #[tokio::test]
    async fn story21_2_attached_paste_is_blocked_while_an_overlay_owns_the_keyboard() {
        let mut rig = Rig::new(false);
        assert!(!presentation::route::blocks_paste(&rig.state));
        rig.ctrl('x').await;
        assert!(presentation::route::blocks_paste(&rig.state), "which-key");
        rig.esc().await;
        rig.ctrl('p').await;
        assert!(presentation::route::blocks_paste(&rig.state), "palette");
        rig.esc().await;
        rig.ctrl('x').await;
        rig.ch('n').await;
        assert!(presentation::route::blocks_paste(&rig.state), "S-LOG");
        rig.esc().await;
        rig.ctrl('q').await; // idle: exits directly, no frame
        rig.type_text("draft").await;
        rig.ctrl('q').await;
        assert!(presentation::route::blocks_paste(&rig.state), "S-QUIT");
        rig.esc().await;
        assert!(!presentation::route::blocks_paste(&rig.state));
    }

    #[tokio::test]
    async fn story21_6_attached_ticket_answers_send_real_input_response() {
        use crate::adapters::tui::decision_queue::{Responder, TicketPayload};
        use crate::domain::models::{AgentId, ArtifactId, ContentHash};
        use crossterm::event::KeyEventKind;

        for (key, expected) in [
            (
                KeyCode::Enter,
                serde_json::json!({"action": "accept", "content": {}}),
            ),
            (KeyCode::Char('d'), serde_json::json!({"action": "decline"})),
            (KeyCode::Char('x'), serde_json::json!({"action": "cancel"})),
        ] {
            let mut rig = Rig::new(false);
            let node = AgentId::parse("ticket-node").unwrap();
            let payload = TicketPayload::new(
                node.clone(),
                ArtifactId::from(ContentHash::from_bytes([1; 32])),
                node,
                1,
                0,
                serde_json::json!({"key": "request-1", "request": {"params": {
                    "requestedSchema": {"type": "object", "properties": {}}
                }}}),
            )
            .unwrap();
            decision_queue::raise_ticket(&mut rig.state, payload, Responder::Attached);
            rig.open_kind(DecisionKind::Ticket);
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
                    if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                        continue;
                    }
                    rig.raw_key(KeyEvent::new_with_kind(key, modifiers, kind))
                        .await;
                    assert!(rig.frames().is_empty());
                    assert!(rig.state.decisions.holds(DecisionKind::Ticket));
                    if key == KeyCode::Char('x')
                        && kind == KeyEventKind::Press
                        && modifiers == KeyModifiers::CONTROL
                    {
                        assert!(rig.state.pending_leader.is_some());
                        rig.esc().await;
                    }
                }
            }
            rig.press(key, KeyModifiers::NONE).await;
            assert!(matches!(rig.frames().as_slice(),
                [ClientFrame::InputResponse { node, responses }]
                    if node == "ticket-node" && responses == &serde_json::json!({"request-1": expected})));
            assert!(!rig.state.decisions.holds(DecisionKind::Ticket));
        }
    }

    #[tokio::test]
    async fn story21_6_attached_ticket_field_enter_requires_plain_press() {
        use crate::adapters::tui::decision_queue::{Responder, TicketPayload};
        use crate::domain::models::{AgentId, ArtifactId, ContentHash};
        use crossterm::event::KeyEventKind;

        let mut rig = Rig::new(false);
        let node = AgentId::parse("ticket-node").unwrap();
        let payload = TicketPayload::new(
            node.clone(),
            ArtifactId::from(ContentHash::from_bytes([2; 32])),
            node,
            1,
            0,
            serde_json::json!({"key": "request-2", "request": {"params": {
                "requestedSchema": {"type": "object", "properties": {
                    "answer": {"type": "string"}
                }, "required": ["answer"]}
            }}}),
        )
        .unwrap();
        decision_queue::raise_ticket(&mut rig.state, payload, Responder::Attached);
        rig.open_kind(DecisionKind::Ticket);
        rig.ch('e').await;
        rig.type_text("ok").await;
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
                if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                    continue;
                }
                rig.raw_key(KeyEvent::new_with_kind(KeyCode::Enter, modifiers, kind))
                    .await;
                assert!(rig.frames().is_empty());
                assert!(rig.state.decisions.holds(DecisionKind::Ticket));
            }
        }
        rig.enter().await;
        assert!(matches!(rig.frames().as_slice(),
        [ClientFrame::InputResponse { node, responses }]
            if node == "ticket-node" && responses == &serde_json::json!({
                "request-2": {"action": "accept", "content": {"answer": "ok"}}
            })));
    }

    #[tokio::test]
    async fn story21_6_attached_reply_read_uses_existing_thread_frame_and_guards_raw_keys() {
        use crossterm::event::KeyEventKind;
        let mut rig = Rig::new(false);
        decision_queue::reply::observe(&mut rig.state, "principal", "roster", "ri_1", 1);
        rig.open_kind(DecisionKind::ReplyArrived);
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
                    if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                        continue;
                    }
                    rig.raw_key(KeyEvent::new_with_kind(code, modifiers, kind))
                        .await;
                    assert!(rig.frames().is_empty());
                    assert_eq!(decision_queue::waiting(&rig.state), 1);
                    if code == KeyCode::Char('x')
                        && kind == KeyEventKind::Press
                        && modifiers == KeyModifiers::CONTROL
                    {
                        assert!(rig.state.pending_leader.is_some());
                        rig.esc().await;
                    }
                }
            }
        }
        rig.state.input_buffer = "unchanged".into();
        rig.enter().await;
        assert!(matches!(rig.frames().as_slice(),
            [ClientFrame::TeamThread { peer, item_id, .. }] if peer == "roster" && item_id == "ri_1"));
        assert_eq!(rig.state.input_buffer, "unchanged");
    }

    #[tokio::test]
    async fn story21_6_attached_ticket_and_reply_raw_ctrl_x_opens_queue() {
        use crate::adapters::tui::decision_queue::{Responder, TicketPayload};
        use crate::domain::models::{AgentId, ArtifactId, ContentHash};
        for reply in [false, true] {
            let mut rig = Rig::new(false);
            let kind = if reply {
                decision_queue::reply::observe(&mut rig.state, "principal", "alias", "ri_1", 1);
                DecisionKind::ReplyArrived
            } else {
                let node = AgentId::parse("ticket-node").unwrap();
                let payload = TicketPayload::new(
                    node.clone(),
                    ArtifactId::from(ContentHash::from_bytes([1; 32])),
                    node,
                    1,
                    0,
                    serde_json::json!({"key":"request", "request":{"params":{
                        "requestedSchema":{"type":"boolean"}
                    }}}),
                )
                .unwrap();
                decision_queue::raise_ticket(&mut rig.state, payload, Responder::Attached);
                DecisionKind::Ticket
            };
            rig.open_kind(kind);
            rig.ctrl('x').await;
            assert!(rig.state.pending_leader.is_some());
            rig.ch('q').await;
            assert!(presentation::is_open(&rig.state, Surface::DecisionQueue));
            assert!(rig.frames().is_empty());
        }
    }

    #[tokio::test]
    async fn story21_6_attached_reply_open_sends_no_frame() {
        let mut rig = Rig::new(false);
        decision_queue::reply::observe(&mut rig.state, "principal", "roster", "ri_1", 1);
        rig.ctrl('x').await;
        rig.ch('q').await;
        assert!(rig.frames().is_empty());
        rig.enter().await;
        assert!(rig.frames().is_empty());
        assert!(presentation::is_open(&rig.state, Surface::Decision));
    }

    #[tokio::test]
    async fn story21_6_attached_journal_read_withdraws_deferred_reply() {
        let mut rig = Rig::new(false);
        decision_queue::reply::observe(&mut rig.state, "principal", "roster", "ri_1", 1);
        rig.open_kind(DecisionKind::ReplyArrived);
        rig.esc().await;
        decision_queue::reply::journal_counts(
            &mut rig.state,
            &std::collections::HashMap::from([(("principal".to_owned(), "ri_1".to_owned()), 1)]),
        );
        assert_eq!(decision_queue::waiting(&rig.state), 0);
        assert!(rig.frames().is_empty());
    }

    // ── the pre-existing owners keep their keys ──────────────────────────────

    #[tokio::test]
    async fn story21_6_attached_retract_escape_defers_without_deciding() {
        let mut rig = Rig::new(false);
        rig.preview_retract(true);
        rig.open_kind(DecisionKind::TeamRetract);
        rig.esc().await;
        assert!(decision_queue::team_retract_pending(&rig.state));
        assert!(rig.frames().is_empty());
        rig.open_kind(DecisionKind::TeamRetract);
        rig.ch('n').await;
        assert!(matches!(rig.frames().as_slice(),
            [ClientFrame::TeamRetractDecision { request, confirm: false }] if request == "retract-a"));
        assert!(rig.frames().is_empty());
        assert!(!decision_queue::team_retract_pending(&rig.state));
    }

    impl Rig {
        /// An approval reaches the loop as the daemon sends it (request, input, complete);
        /// the loop then paints the card by its own rule.
        fn approval_arrives(&mut self, id: &str) {
            use crate::domain::models::tool_call::{ApprovalSource, RequestId};
            let data = r#"{"command":"ok"}"#;
            let request_id = || RequestId(id.into());
            for frame in [
                DaemonFrame::ApprovalRequest {
                    request_id: request_id(),
                    tool: "Bash".into(),
                    source: ApprovalSource::ForegroundTurn {
                        conversation_id: "c".into(),
                    },
                    risk: crate::domain::models::ToolRisk::Standard,
                    summary: "command summary".into(),
                    input_bytes: Some(data.len() as u64),
                },
                DaemonFrame::ApprovalInputChunk {
                    request_id: request_id(),
                    sequence: 0,
                    data: data.into(),
                },
                DaemonFrame::ApprovalInputComplete {
                    request_id: request_id(),
                    chunks: 1,
                },
            ] {
                self.approval_frame(frame);
            }
            self.paint_approvals();
        }

        /// The loop's own paint rule: a writer in Chat or Input with an empty composer.
        fn paint_approvals(&mut self) {
            self.paint();
        }
    }

    impl Rig {
        fn retained_arrives(&mut self, consolidation: bool) {
            if consolidation {
                super::super::apply_attached_consolidation_frame(
                    &mut self.state,
                    DaemonFrame::ConsolidationProposed {
                        token: ProposalToken(7),
                        proposals: vec![],
                    },
                    self.read_only,
                );
            } else {
                self.approval_frame(DaemonFrame::ApprovalRequest {
                    request_id: crate::domain::models::RequestId::new(),
                    tool: "a2a/sender-consent".into(),
                    source: crate::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                        conversation_id: "c".into(),
                    },
                    risk: crate::domain::models::ToolRisk::Standard,
                    summary: "consent".into(),
                    input_bytes: None,
                });
            }
        }
    }

    #[tokio::test]
    async fn story21_6_attached_consolidation_arrival_preserves_focus_and_draft() {
        for draft in ["", "saved draft"] {
            let mut rig = Rig::new(false);
            rig.state.input_buffer = draft.into();
            rig.state.cursor_position = draft.chars().count();
            rig.retained_arrives(true);
            assert_eq!(rig.state.input_buffer, draft);
            assert_eq!(rig.state.focus, FocusState::Input);
            assert!(rig.state.decisions.holds(DecisionKind::Consolidation));
            assert!(rig.frames().is_empty());
        }
    }

    #[tokio::test]
    async fn story21_3_arrival_behind_log_and_quit_attached() {
        for surface in ['n', 'q'] {
            let mut rig = Rig::new(false);
            rig.state.input_buffer = "saved draft".into();
            rig.state.cursor_position = rig.state.input_buffer.len();
            rig.domain_tab.streaming.is_streaming = true;
            if surface == 'q' {
                rig.ctrl('q').await;
            } else {
                rig.ctrl('x').await;
                rig.ch(surface).await;
            }
            rig.approval_arrives("behind-surface");
            assert_eq!(rig.state.decisions.undecided(), 1);
            rig.ch('d').await;
            assert_eq!(rig.state.input_buffer, "saved draft");
            rig.esc().await;
            assert_eq!(rig.state.focus, FocusState::Input);
            assert!(rig.frames().is_empty());
        }
    }

    #[tokio::test]
    async fn story21_3_attached_approval_arriving_behind_help_remains_discoverable() {
        let mut rig = Rig::new(false);
        rig.state.input_buffer = "saved draft".into();
        rig.state.cursor_position = rig.state.input_buffer.len();
        rig.ctrl('x').await;
        rig.ch('?').await;
        rig.approval_arrives("behind-help");
        assert!(presentation::is_open(&rig.state, Surface::Help));
        rig.esc().await;
        assert_eq!(rig.state.input_buffer, "saved draft");
        rig.ctrl('x').await;
        rig.ch('q').await;
        assert!(presentation::is_open(&rig.state, Surface::DecisionQueue));
        assert_eq!(rig.state.decisions.undecided(), 1);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_proposal_ctrl_c_defers_without_interrupting_live_turn() {
        let mut rig = Rig::new(false);
        rig.state.focus = FocusState::Chat;
        rig.paint();
        let conversation = rig.domain_tab.conversation.id.clone();
        super::super::apply_attached_proposal_frame(
            &mut rig.state,
            false,
            &mut rig.team_requests,
            &conversation,
            DaemonFrame::TeamProposal {
                proposal: "p".into(),
                recipients: vec!["peer".into()],
                message_type: crate::domain::models::SemanticMessageType::ArchitectureUpdate,
                body: "review".into(),
            },
        );
        rig.ready();
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.domain_tab.streaming.is_streaming = true;
        rig.ctrl('c').await;
        assert!(
            rig.frames().is_empty(),
            "deferring must not interrupt or decide"
        );
        let item = rig
            .state
            .decisions
            .items()
            .find(|item| item.kind == DecisionKind::TeamProposal)
            .unwrap();
        assert!(item.deferred);
        assert!(!presentation::is_open(&rig.state, Surface::Decision));
        rig.state.input_buffer = "saved draft".into();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        rig.ctrl('c').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::InterruptTurn { .. }]
        ));
        assert_eq!(rig.state.input_buffer, "saved draft");
        assert!(rig.state.decisions.holds(DecisionKind::TeamProposal));
    }

    fn screen(rig: &mut Rig) -> String {
        let terminal = rig.paint();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Palette retract posts one ambient result notice even over a scrolled transcript.
    #[tokio::test]
    async fn story21_6_attached_auto_response_retract_keeps_and_paints_its_notice() {
        use crate::adapters::tui::keymap::Key;
        let mut rig = Rig::new(false);
        for row in 0..40 {
            rig.domain_tab
                .conversation
                .messages
                .push(system_message(format!("retract-prior-{row:02}")));
        }
        rig.retractable("peer-response-node-a", "auto reply");
        screen(&mut rig);
        for _ in 0..3 {
            rig.key(Key::PageUp).await;
            screen(&mut rig);
        }
        assert!(
            !screen(&mut rig).contains("auto reply"),
            "the transcript is scrolled up"
        );
        rig.ctrl('p').await;
        rig.type_text("retract").await;
        rig.enter().await;
        rig.open_kind(DecisionKind::TeamRetract);
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::RetractAutoResponse { message_id, target_seq: None }]
                if message_id == "peer-response-node-a"
        ));
        assert!(!decision_queue::team_retract_pending(&rig.state));
        let notices: Vec<_> = rig
            .state
            .notifications
            .records()
            .filter(|record| {
                record.source == NoticeSource::Team
                    && record.message.starts_with("Retracted. Marked in your log")
            })
            .collect();
        assert_eq!(notices.len(), 1, "settlement posts exactly one result");
        assert!(rig.feedback("peer-retract").is_none());
        let painted = screen(&mut rig);
        assert!(
            painted
                .split_whitespace()
                .collect::<String>()
                .contains("Retracted.Markedinyourlog"),
            "the retract notice must be painted:\n{painted}"
        );
        rig.ch('j').await;
        assert_eq!(
            rig.state
                .notifications
                .records()
                .filter(|record| record.source == NoticeSource::Team
                    && record.message.starts_with("Retracted. Marked in your log"))
                .count(),
            1,
            "the next key does not clear the result"
        );
    }

    /// Same rule for the team proposal answers: the dismissal row (and the send rows)
    /// are blocks below the newest row and must be painted for a scrolled-up reader.
    #[tokio::test]
    async fn story21_6_attached_proposal_dismissal_row_is_painted_for_a_scrolled_up_transcript() {
        use crate::adapters::tui::keymap::Key;
        let mut rig = Rig::new(false);
        for row in 0..40 {
            rig.domain_tab
                .conversation
                .messages
                .push(system_message(format!("proposal-prior-{row:02}")));
        }
        screen(&mut rig);
        for _ in 0..3 {
            rig.key(Key::PageUp).await;
            screen(&mut rig);
        }
        let conversation = rig.domain_tab.conversation.id.clone();
        super::super::apply_attached_proposal_frame(
            &mut rig.state,
            false,
            &mut rig.team_requests,
            &conversation,
            DaemonFrame::TeamProposal {
                proposal: "p".into(),
                recipients: vec!["peer".into()],
                message_type: crate::domain::models::SemanticMessageType::ArchitectureUpdate,
                body: "review".into(),
            },
        );
        rig.open_kind(DecisionKind::TeamProposal);
        rig.ch('n').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::TeamProposalDecision { proposal, .. }] if proposal == "p"
        ));
        let painted = screen(&mut rig);
        assert!(
            painted
                .split_whitespace()
                .collect::<String>()
                .contains("Proposaldismissed.Nothingwassent."),
            "the dismissal row must be painted:\n{painted}"
        );
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_queue_has_no_bottom_card_or_bare_tab() {
        use crate::domain::models::{PeerRowControl, PeerRowStage, SemanticMessageType};
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut rig = Rig::new(false);
            rig.state.terminal_width = width;
            rig.state.terminal_height = height;
            rig.state.input_buffer = "retained draft".into();
            rig.state.cursor_position = rig.state.input_buffer.len();
            screen(&mut rig);
            let baseline = rig.state.viewport_height;
            for (id, alias) in [("one", "nina"), ("two", "alex")] {
                let mut row = system_message("draft text");
                row.id = id.into();
                rig.domain_tab.conversation.messages.push(row);
                rig.domain_tab.conversation.peer_row_controls.insert(
                    id.into(),
                    PeerRowControl {
                        node_id: id.into(),
                        item_address: None,
                        stories: None,
                        status: None,
                        selected_for: None,
                        retro: None,
                        message_type: SemanticMessageType::Consultation,
                        sender_label: alias.into(),
                        stage: PeerRowStage::Ready,
                        node_resolvable: true,
                    },
                );
            }
            decision_queue::sync_recipient_drafts(
                &mut rig.state,
                &rig.domain_tab.conversation,
                false,
            );
            let items = rig.state.decisions.items().collect::<Vec<_>>();
            assert_eq!(items.len(), 2);
            let key = items[0].key();
            assert!(
                matches!(&items[0].subject, DecisionSubject::RecipientDraft(payload)
                if payload.sender_label == "nina" && payload.node == "one")
            );
            let painted = screen(&mut rig);
            assert_eq!(
                rig.state.viewport_height, baseline,
                "recipient items do not reserve chat rows"
            );
            assert!(painted.contains("Needs you (2)"), "{painted}");
            assert!(
                !painted.contains("more queued") && !painted.contains("[Tab] Response actions"),
                "{painted}"
            );
            rig.press(KeyCode::Tab, KeyModifiers::NONE).await;
            assert!(rig.state.thread_inspector.is_none());
            assert_eq!(rig.state.input_buffer, "retained draft");
            assert!(rig.frames().is_empty());
            assert!(decision_queue::open(&mut rig.state, &key));
            let frame = screen(&mut rig);
            assert!(
                frame.contains("nina") && frame.contains("consultation"),
                "{frame}"
            );
            assert!(
                frame.contains("[y] Approve") && frame.contains("[n] Reject"),
                "{frame}"
            );
            assert!(
                !frame.contains("[Tab]") && !frame.contains("more queued"),
                "{frame}"
            );
        }
    }

    #[tokio::test]
    async fn story21_6_attached_mirror_never_paints_a_decision() {
        use crate::adapters::daemon::protocol::ApprovalRefusal;
        use crate::domain::models::RequestId;
        let mut rig = Rig::new(true);
        let mut snapshot = super::super::tests::snapshot_with(Vec::new(), 0);
        snapshot.retained_approvals = 2;
        snapshot.pending_consent_cards = 1;
        super::super::seed_mirror_waiting(&mut rig.state, &snapshot, true);
        rig.approval_arrives("private");
        rig.retained_arrives(false);
        let painted = screen(&mut rig);
        let layout = crate::adapters::tui::layout::compute_layout(
            ratatui::layout::Rect::new(0, 0, 80, 24),
            &rig.state.theme,
            "",
            1,
            false,
        )
        .unwrap();
        assert!(
            painted
                .lines()
                .nth(usize::from(layout.status_bar.y))
                .unwrap()
                .contains("3 approval(s) waiting for the writer"),
            "{painted}"
        );
        assert!(
            !painted.contains("Decision:")
                && !painted.contains("[r] Review")
                && !painted.contains("[y] Allow")
                && !painted.contains("Needs you"),
            "{painted}"
        );
        assert_eq!(rig.state.focus, FocusState::Input);
        assert!(rig.state.modal_stack.is_empty());
        assert!(rig.state.decisions.is_empty());
        rig.ctrl('x').await;
        rig.ch('?').await;
        let owner = rig.state.modal_stack.top().unwrap().clone();
        rig.approval_arrives("ignored-behind-help");
        rig.retained_arrives(false);
        assert_eq!(rig.state.modal_stack.top(), Some(&owner));
        rig.esc().await;
        rig.state.focus = FocusState::Chat;
        rig.ch('y').await;
        assert!(rig.state.input_buffer.is_empty());
        assert!(rig.frames().is_empty());
        for (id, reason, expected) in [
            ("first", None, 2),
            ("first", None, 2),
            ("other", Some(ApprovalRefusal::WriterOnly), 2),
            ("other", Some(ApprovalRefusal::NoLongerPending), 1),
        ] {
            let request_id = RequestId(id.into());
            rig.approval_frame(match reason {
                None => DaemonFrame::ApprovalClosed { request_id },
                Some(reason) => DaemonFrame::ApprovalRefused { request_id, reason },
            });
            assert_eq!(rig.state.decisions.mirror_waiting(), expected);
            let painted = screen(&mut rig);
            assert!(
                painted.contains(&format!("{expected} approval(s) waiting for the writer")),
                "{painted}"
            );
            assert!(
                !painted.contains("Decision:") && !painted.contains("[r] Review"),
                "{painted}"
            );
        }
    }

    #[tokio::test]
    async fn story21_6_attached_writer_decides_exactly_once_and_retires_after_send() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("writer");
        let painted = screen(&mut rig);
        assert!(painted.contains("Decision: Permission"), "{painted}");
        assert!(
            painted.contains("[y] Allow  [s] Session  [a] Always"),
            "{painted}"
        );
        assert!(
            painted.contains("[n] Deny  [f] Deny + feedback  [r] Review"),
            "{painted}"
        );
        assert!(
            painted.lines().any(|line| line.starts_with("││ ")),
            "{painted}"
        );
        rig.ch('y').await;
        assert!(
            rig.frames().is_empty(),
            "first paint starts the inert window"
        );
        assert_eq!(rig.state.decisions.undecided(), 1);
        rig.clock
            .advance(crate::adapters::tui::decision_queue::INERT_AFTER_PAINT);
        rig.ch('y').await;
        assert!(
            matches!(rig.frames().as_slice(), [ClientFrame::ApprovalResponse {
            request_id, outcome: crate::domain::models::ApprovalOutcome::Once,
        }] if request_id.0 == "writer")
        );
        assert!(rig.state.decisions.is_empty());
        assert!(!presentation::is_open(&rig.state, Surface::Decision));
        rig.ch('y').await;
        assert!(rig.frames().is_empty());

        let mut failed = Rig::new(false);
        failed.approval_arrives("closed");
        failed.ready();
        failed.rx.close();
        failed.ch('y').await;
        assert_eq!(failed.state.decisions.undecided(), 1);
        assert!(presentation::is_open(&failed.state, Surface::Decision));
    }

    #[tokio::test]
    async fn story21_6_attached_only_plain_press_can_decide_either_queued_item() {
        use crate::domain::models::{ApprovalSource, RequestId, ToolRisk};
        use crossterm::event::KeyEventKind;

        for consent in [false, true] {
            let mut rig = Rig::new(false);
            for id in ["first", "second"] {
                if consent {
                    rig.approval_frame(DaemonFrame::ApprovalRequest {
                        request_id: RequestId(id.into()),
                        tool: "a2a/sender-consent".into(),
                        source: ApprovalSource::ForegroundTurn {
                            conversation_id: "c".into(),
                        },
                        risk: ToolRisk::Standard,
                        summary: "consent".into(),
                        input_bytes: None,
                    });
                } else {
                    rig.approval_arrives(id);
                }
            }
            rig.ready();
            let before: Vec<_> = rig.state.decisions.items().cloned().collect();
            assert_eq!(before.len(), 2);
            for code in ['y', 's', 'a', 'n', 'f', 'r', 'Y'] {
                for modifiers in [
                    KeyModifiers::SHIFT,
                    KeyModifiers::CONTROL,
                    KeyModifiers::ALT,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ] {
                    rig.raw_key(KeyEvent::new(KeyCode::Char(code), modifiers))
                        .await;
                    assert_eq!(
                        rig.state.decisions.items().cloned().collect::<Vec<_>>(),
                        before
                    );
                    assert!(rig.frames().is_empty());
                }
                for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                    rig.raw_key(KeyEvent::new_with_kind(
                        KeyCode::Char(code),
                        KeyModifiers::NONE,
                        kind,
                    ))
                    .await;
                    assert_eq!(
                        rig.state.decisions.items().cloned().collect::<Vec<_>>(),
                        before
                    );
                    assert!(rig.frames().is_empty());
                }
            }
            rig.ch('y').await;
            assert!(matches!(rig.frames().as_slice(),
                [ClientFrame::ApprovalResponse { request_id, .. }] if request_id.0 == "first"));
            assert_eq!(rig.state.decisions.undecided(), 1);
            rig.ready();
            rig.raw_key(KeyEvent::new_with_kind(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            ))
            .await;
            assert!(rig.frames().is_empty());
            assert_eq!(rig.state.decisions.items().next(), Some(&before[1]));
        }
    }

    #[tokio::test]
    async fn story21_6_attached_feedback_submit_requires_plain_press_but_editing_remains_live() {
        use crossterm::event::KeyEventKind;
        let mut rig = Rig::new(false);
        rig.approval_arrives("feedback-raw");
        rig.ready();
        rig.ch('f').await;
        rig.type_text("abc").await;
        rig.ctrl('a').await;
        rig.press(KeyCode::Char('Z'), KeyModifiers::SHIFT).await;
        rig.ctrl('e').await;
        rig.ctrl('h').await;
        rig.ctrl('w').await;
        rig.type_text("temporary").await;
        rig.ctrl('a').await;
        rig.ctrl('k').await;
        rig.type_text("temporary").await;
        rig.ctrl('u').await;
        rig.type_text("Zab").await;
        let before: Vec<_> = rig.state.decisions.items().cloned().collect();
        for key in [
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        ] {
            rig.raw_key(key).await;
            assert_eq!(
                rig.state.decisions.items().cloned().collect::<Vec<_>>(),
                before
            );
            assert!(rig.frames().is_empty());
        }
        rig.enter().await;
        assert!(matches!(rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse { outcome: crate::domain::models::ApprovalOutcome::Reject { feedback: Some(text) }, .. }]
                if text == "Zab"));
    }

    #[tokio::test]
    async fn story21_6_attached_waiting_permissions_and_queued_decisions() {
        for deferred in [false, true] {
            for card in ["consolidation", "retract", "team"] {
                let mut rig = Rig::new(false);
                if !deferred {
                    rig.state.pending_leader = Some(Context::Input);
                }
                rig.approval_arrives("waiting");
                if deferred {
                    rig.esc().await;
                } else {
                    rig.state.pending_leader = None;
                }
                assert!(!presentation::is_open(&rig.state, Surface::Decision));
                match card {
                    "consolidation" => rig.retained_arrives(true),
                    "retract" => {
                        rig.retractable("peer-response-test", "retract canary");
                        rig.ctrl('p').await;
                        rig.type_text("retract").await;
                        rig.enter().await;
                    }
                    "team" if deferred => {
                        // 6D-L1-03: a set-aside permission is not a reason to decline an
                        // unrelated retract preview: it is queued beside it, nothing is sent.
                        rig.preview_retract(true);
                        assert!(rig.frames().is_empty());
                        assert_eq!(rig.state.decisions.undecided(), 2);
                        continue;
                    }
                    "team" => {
                        rig.preview_retract(true);
                        assert!(matches!(rig.frames().as_slice(),
                            [ClientFrame::TeamRetractDecision { request, confirm: false }] if request == "retract-a"));
                        assert_eq!(rig.state.decisions.undecided(), 1);
                        continue;
                    }
                    _ => unreachable!(),
                }
                rig.open_kind(if card == "consolidation" {
                    DecisionKind::Consolidation
                } else {
                    DecisionKind::TeamRetract
                });
                let painted = screen(&mut rig);
                let expected = match card {
                    "consolidation" => "memory",
                    "retract" => "retract canary",
                    "team" => "team retract canary",
                    _ => unreachable!(),
                };
                assert!(
                    painted.contains(expected),
                    "{card} deferred={deferred}: {painted}"
                );
                rig.ch('y').await;
                let frames = rig.frames();
                assert!(
                    match card {
                        "consolidation" => matches!(
                            frames.as_slice(),
                            [ClientFrame::ConsolidationResolve {
                                token: ProposalToken(7),
                                accept: true
                            }]
                        ),
                        "retract" =>
                            matches!(frames.as_slice(), [ClientFrame::RetractAutoResponse { message_id, target_seq: None }] if message_id == "peer-response-test"),
                        _ => false,
                    },
                    "{card} deferred={deferred}: {frames:?}"
                );
                assert!(rig.frames().is_empty());
                assert_eq!(rig.state.decisions.undecided(), 1);
            }
        }
    }

    #[tokio::test]
    async fn story21_6_attached_esc_defers_and_never_decides() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("defer");
        rig.esc().await;
        assert!(rig.frames().is_empty());
        assert_eq!(rig.state.decisions.undecided(), 1);
        assert!(!presentation::is_open(&rig.state, Surface::Decision));
        let painted = screen(&mut rig);
        assert!(painted.contains("Needs you (1) ^X q"), "{painted}");
        rig.ctrl('x').await;
        rig.ch('q').await;
        assert!(presentation::is_open(&rig.state, Surface::DecisionQueue));
        rig.enter().await;
        rig.ready();
        rig.ch('n').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse {
                outcome: crate::domain::models::ApprovalOutcome::Reject { feedback: None },
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn story21_6_attached_stale_after_a_closed_frame_shows_the_source_copy() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("stale");
        rig.approval_frame(DaemonFrame::ApprovalClosed {
            request_id: crate::domain::models::RequestId("stale".into()),
        });
        let painted = screen(&mut rig);
        assert!(
            painted.contains("approval refused: that request is no longer pending"),
            "{painted}"
        );
        assert!(painted.contains("[Esc] Close"), "{painted}");
        assert!(!painted.contains("[y] Allow"), "{painted}");
        rig.ch('y').await;
        assert!(rig.frames().is_empty());
        rig.esc().await;
        assert!(rig.state.decisions.is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_closed_review_and_feedback_cannot_send() {
        for key in ['r', 'f'] {
            let mut rig = Rig::new(false);
            rig.approval_arrives("closed-subcontext");
            rig.ready();
            rig.ch(key).await;
            rig.approval_frame(DaemonFrame::ApprovalClosed {
                request_id: crate::domain::models::RequestId("closed-subcontext".into()),
            });
            rig.approval_frame(DaemonFrame::ApprovalInputChunk {
                request_id: crate::domain::models::RequestId("closed-subcontext".into()),
                sequence: 0,
                data: "{}".into(),
            });
            rig.approval_frame(DaemonFrame::ApprovalInputComplete {
                request_id: crate::domain::models::RequestId("closed-subcontext".into()),
                chunks: 1,
            });
            rig.ch('y').await;
            rig.enter().await;
            assert!(rig.frames().is_empty());
            assert_eq!(rig.state.decisions.undecided(), 0);
            assert!(screen(&mut rig).contains("[Esc] Close"));
            rig.esc().await;
            assert!(rig.state.decisions.is_empty());
        }
    }

    #[tokio::test]
    async fn story21_6_attached_review_waits_for_exact_input() {
        use crate::domain::models::{ApprovalSource, RequestId, ToolRisk};
        let mut rig = Rig::new(false);
        let data = r#"{"command":"EXACT-INPUT-CANARY"}"#;
        rig.approval_frame(DaemonFrame::ApprovalRequest {
            request_id: RequestId("receiving".into()),
            tool: "Bash".into(),
            source: ApprovalSource::ForegroundTurn {
                conversation_id: "c".into(),
            },
            risk: ToolRisk::Standard,
            summary: "summary".into(),
            input_bytes: Some(data.len() as u64),
        });
        rig.ready();
        rig.ch('r').await;
        assert!(screen(&mut rig).contains("receiving exact input"));
        rig.approval_frame(DaemonFrame::ApprovalInputChunk {
            request_id: RequestId("receiving".into()),
            sequence: 0,
            data: data.into(),
        });
        rig.approval_frame(DaemonFrame::ApprovalInputComplete {
            request_id: RequestId("receiving".into()),
            chunks: 1,
        });
        rig.ch('r').await;
        assert!(screen(&mut rig).contains("EXACT-INPUT-CANARY"));
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_review_keeps_request_identity_and_exact_tail() {
        use crate::domain::models::{ApprovalSource, RequestId, ToolRisk};
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut rig = Rig::new(false);
            rig.state.terminal_width = width;
            rig.state.terminal_height = height;
            let value = serde_json::json!({
                "command": format!("HEAD {} \n[y] fake control \u{1b}[31m TAIL-CANARY", "long content ".repeat(120))
            });
            let data = serde_json::to_string(&value).unwrap();
            rig.approval_frame(DaemonFrame::ApprovalRequest {
                request_id: RequestId("review-a".into()),
                tool: format!("mcp__{}__{}", "server".repeat(100), "operation".repeat(100)),
                source: ApprovalSource::ForegroundSubagent {
                    conversation_id: "c".into(),
                    parent_tool_call_id: "parent".into(),
                    subagent_type: "source".repeat(100),
                },
                risk: ToolRisk::Standard,
                summary: "review input".into(),
                input_bytes: Some(data.len() as u64),
            });
            rig.approval_frame(DaemonFrame::ApprovalInputChunk {
                request_id: RequestId("review-a".into()),
                sequence: 0,
                data,
            });
            rig.approval_frame(DaemonFrame::ApprovalInputComplete {
                request_id: RequestId("review-a".into()),
                chunks: 1,
            });
            rig.ready();
            rig.ch('r').await;
            rig.approval_arrives("review-b");
            let mut pages = String::new();
            for _ in 0..80 {
                let page = screen(&mut rig);
                assert!(
                    page.contains("[y] Allow") && page.contains("[n] Deny"),
                    "{page}"
                );
                pages.push_str(&page);
                rig.key(Key::PageDown).await;
            }
            assert!(
                pages.contains("HEAD") && pages.contains("TAIL-CANARY"),
                "{pages}"
            );
            assert!(!pages.contains('\u{1b}'), "{pages}");
            assert!(!pages.contains(r#""command": "ok""#));
            assert!(rig.frames().is_empty());
            rig.ch('y').await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::ApprovalResponse { request_id, .. }]
                if request_id.0 == "review-a")
            );
            assert_eq!(rig.state.decisions.undecided(), 1);
        }
    }

    #[tokio::test]
    async fn story21_6_attached_consent_keys_and_labels() {
        use crate::domain::models::{ApprovalOutcome, ApprovalScope};
        for (key, expected) in [
            ('y', ApprovalOutcome::Once),
            (
                'a',
                ApprovalOutcome::AlwaysAndSave {
                    scope: ApprovalScope::Tool("a2a/sender-consent".into()),
                },
            ),
            ('n', ApprovalOutcome::Reject { feedback: None }),
        ] {
            let mut rig = Rig::new(false);
            rig.retained_arrives(false);
            let painted = screen(&mut rig);
            assert!(painted.contains("Decision: Peer consent"), "{painted}");
            assert!(
                painted.contains("[y] Allow once  [a] Always allow  [n] Decline"),
                "{painted}"
            );
            rig.clock
                .advance(crate::adapters::tui::decision_queue::INERT_AFTER_PAINT);
            rig.ch(key).await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::ApprovalResponse { outcome, .. }] if outcome == &expected)
            );
            assert!(rig.state.decisions.is_empty());
        }
        let mut reader = Rig::new(true);
        reader.retained_arrives(false);
        assert!(reader.state.decisions.is_empty());
        assert!(!screen(&mut reader).contains("Decision: Peer consent"));
    }

    #[tokio::test]
    async fn story21_6_attached_feedback_field_denies_with_text() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("feedback");
        rig.ready();
        rig.state.input_buffer = "untouched".into();
        rig.state.cursor_position = 3;
        rig.ch('f').await;
        rig.type_text("not safe").await;
        rig.enter().await;
        assert!(
            matches!(rig.frames().as_slice(), [ClientFrame::ApprovalResponse {
            outcome: crate::domain::models::ApprovalOutcome::Reject { feedback: Some(text) }, ..
        }] if text == "not safe")
        );
        assert_eq!(rig.state.input_buffer, "untouched");
        assert_eq!(rig.state.cursor_position, 3);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_quit_counts_live_but_not_stale() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("quit");
        rig.ctrl('q').await;
        assert_eq!(
            rig.state
                .quit_confirm
                .as_ref()
                .unwrap()
                .facts
                .pending_decisions,
            1
        );
        rig.esc().await;
        rig.approval_frame(DaemonFrame::ApprovalClosed {
            request_id: crate::domain::models::RequestId("quit".into()),
        });
        assert_eq!(rig.state.decisions.undecided(), 0);
        assert_eq!(rig.ctrl('q').await, KeyFlow::Quit);
    }

    #[tokio::test]
    async fn story21_6_attached_paint_starts_inert_window() {
        let mut rig = Rig::new(false);
        rig.approval_frame(DaemonFrame::ApprovalRequest {
            request_id: crate::domain::models::RequestId("paint".into()),
            tool: "Bash".into(),
            source: crate::domain::models::ApprovalSource::ForegroundTurn {
                conversation_id: "c".into(),
            },
            risk: crate::domain::models::ToolRisk::Standard,
            summary: "receiving".into(),
            input_bytes: Some(2),
        });
        rig.clock
            .advance(crate::adapters::tui::decision_queue::INERT_AFTER_PAINT);
        rig.ch('y').await;
        assert!(rig.frames().is_empty(), "an unpainted frame is inert");
        rig.paint();
        rig.ch('y').await;
        assert!(
            rig.frames().is_empty(),
            "the first paint restarts the inert interval"
        );
        rig.clock
            .advance(crate::adapters::tui::decision_queue::INERT_AFTER_PAINT);
        rig.ch('y').await;
        assert_eq!(rig.frames().len(), 1);
    }

    #[tokio::test]
    async fn story21_6_attached_permission_scope_and_deny_outcomes() {
        use crate::domain::models::{ApprovalOutcome, ApprovalScope};
        for (key, expected) in [
            (
                's',
                ApprovalOutcome::AlwaysTool {
                    tool_name: "Bash".into(),
                },
            ),
            (
                'a',
                ApprovalOutcome::AlwaysAndSave {
                    scope: ApprovalScope::Tool("Bash".into()),
                },
            ),
            ('n', ApprovalOutcome::Reject { feedback: None }),
        ] {
            let mut rig = Rig::new(false);
            rig.approval_arrives("scope");
            rig.ready();
            rig.ch(key).await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::ApprovalResponse { request_id, outcome }]
                if request_id.0 == "scope" && outcome == &expected)
            );
            assert!(rig.state.decisions.is_empty());
        }
    }

    #[tokio::test]
    async fn story21_3_attached_approval_arrival_never_takes_focus_from_a_draft_or_a_reader() {
        for read_only in [false, true] {
            let mut rig = Rig::new(read_only);
            rig.state.input_buffer = "draft ".into();
            rig.state.cursor_position = rig.state.input_buffer.len();
            rig.approval_arrives("draft");
            rig.ch('y').await;
            assert_eq!(rig.state.input_buffer, "draft y");
            assert_eq!(rig.state.decisions.undecided(), usize::from(!read_only));
            assert!(rig.frames().is_empty());
        }
    }

    #[tokio::test]
    async fn story21_6_attached_consolidation_answers_and_defers() {
        let mut rig = Rig::new(false);
        rig.state.input_buffer = "draft".into();
        rig.state.cursor_position = 5;
        rig.consolidation(7);
        rig.ch('y').await;
        rig.ch('n').await;
        assert_eq!(rig.state.input_buffer, "draftyn");
        assert!(rig.frames().is_empty());
        for ctrl_c in [false, true] {
            rig.open_kind(DecisionKind::Consolidation);
            if ctrl_c {
                rig.ctrl('c').await;
            } else {
                rig.esc().await;
            }
            assert!(rig.frames().is_empty());
            assert!(rig.state.decisions.holds(DecisionKind::Consolidation));
        }
        rig.open_kind(DecisionKind::Consolidation);
        rig.ch('n').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ConsolidationResolve {
                token: ProposalToken(7),
                accept: false
            }]
        ));
        assert!(rig.frames().is_empty());
        assert!(!rig.state.decisions.holds(DecisionKind::Consolidation));
        assert_eq!(rig.state.input_buffer, "draftyn");
    }

    #[tokio::test]
    async fn story21_6_attached_consolidations_resolve_under_their_own_tokens() {
        let mut rig = Rig::new(false);
        rig.consolidation(7);
        rig.consolidation(8);
        assert_eq!(rig.state.decisions.count(DecisionKind::Consolidation), 2);
        for (token, key, accept) in [(7, 'y', true), (8, 'n', false)] {
            rig.open_kind(DecisionKind::Consolidation);
            rig.ch(key).await;
            assert!(matches!(rig.frames().as_slice(),
                [ClientFrame::ConsolidationResolve { token: ProposalToken(actual), accept: value }]
                    if *actual == token && *value == accept));
            assert!(rig.frames().is_empty());
        }
        assert!(rig.state.decisions.is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_mismatched_consolidation_token_sends_nothing() {
        use decision_queue::{
            Arrival, ConsolidationPayload, DecisionDraft, PendingConsolidationCard, Responder,
        };
        let mut rig = Rig::new(false);
        decision_queue::raise(
            &mut rig.state,
            DecisionDraft::new(DecisionKind::Consolidation, "")
                .source("7")
                .responder(Responder::Attached)
                .subject(DecisionSubject::Consolidation(Box::new(
                    ConsolidationPayload {
                        card: PendingConsolidationCard {
                            conversation_id: String::new(),
                            proposals: vec![],
                        },
                        token: Some(ProposalToken(8)),
                    },
                ))),
            Arrival::Automatic,
        );
        rig.open_kind(DecisionKind::Consolidation);
        rig.ch('y').await;
        assert!(rig.frames().is_empty());
        assert_eq!(rig.state.decisions.undecided(), 0);
    }

    #[tokio::test]
    async fn story21_6_attached_disarmed_retract_consumes_yes() {
        let mut rig = Rig::new(false);
        rig.preview_retract(false);
        rig.open_kind(DecisionKind::TeamRetract);
        rig.ch('y').await;
        assert!(rig.frames().is_empty());
        assert!(decision_queue::team_retract_pending(&rig.state));
        rig.ch('n').await;
        assert!(matches!(rig.frames().as_slice(),
            [ClientFrame::TeamRetractDecision { request, confirm: false }] if request == "retract-a"));
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_raw_modified_and_nonpress_keys_never_decide() {
        use crossterm::event::KeyEventKind;
        for kind in [DecisionKind::Consolidation, DecisionKind::TeamRetract] {
            let mut rig = Rig::new(false);
            if kind == DecisionKind::Consolidation {
                rig.consolidation(7);
            } else {
                rig.preview_retract(true);
            }
            rig.open_kind(kind);
            for c in ['y', 'n'] {
                for event in [
                    KeyEvent::new_with_kind(
                        KeyCode::Char(c),
                        KeyModifiers::NONE,
                        KeyEventKind::Repeat,
                    ),
                    KeyEvent::new_with_kind(
                        KeyCode::Char(c),
                        KeyModifiers::NONE,
                        KeyEventKind::Release,
                    ),
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT),
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT),
                ] {
                    rig.raw_key(event).await;
                    assert!(rig.frames().is_empty());
                    assert!(rig.state.decisions.holds(kind));
                }
            }
            rig.ch('n').await;
            let frames = rig.frames();
            assert!(match kind {
                DecisionKind::Consolidation => matches!(
                    frames.as_slice(),
                    [ClientFrame::ConsolidationResolve {
                        token: ProposalToken(7),
                        accept: false
                    }]
                ),
                _ => matches!(frames.as_slice(),
                    [ClientFrame::TeamRetractDecision { request, confirm: false }] if request == "retract-a"),
            });
            assert!(rig.frames().is_empty());
        }
    }

    fn story21_6_attached_recipient_rig() -> Rig {
        use crate::domain::models::{
            ChatMessage, PeerRowControl, PeerRowStage, SemanticMessageType,
        };
        let mut rig = Rig::new(false);
        rig.domain_tab.conversation.id = "recipient-origin".to_owned();
        rig.domain_tab.conversation.messages.push(ChatMessage {
            id: "row".to_owned(),
            content: "original draft".to_owned(),
            ..Default::default()
        });
        rig.domain_tab.conversation.peer_row_controls.insert(
            "row".to_owned(),
            PeerRowControl {
                node_id: "node-a".to_owned(),
                item_address: None,
                stories: None,
                status: None,
                retro: None,
                selected_for: None,
                message_type: SemanticMessageType::Consultation,
                sender_label: "sender".to_owned(),
                stage: PeerRowStage::Ready,
                node_resolvable: true,
            },
        );
        decision_queue::sync_recipient_drafts(&mut rig.state, &rig.domain_tab.conversation, false);
        rig.open_kind(DecisionKind::RecipientDraftApproval);
        rig
    }

    #[tokio::test]
    async fn story21_6_withdrawn_recipient_copies_unsent_field_without_resolving() {
        let mut rig = story21_6_attached_recipient_rig();
        let copied = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let sink = copied.clone();
        rig.state.clipboard_copier = Some(std::sync::Arc::new(move |text| {
            sink.lock().push(text.to_owned());
            crate::adapters::tui::clipboard::ClipboardResult::Osc52Success
        }));
        rig.state.input_buffer = "composer remains".into();
        rig.ch('w').await;
        rig.type_text("my unsent reply").await;
        assert!(super::super::apply_peer_row_operation(
            &mut rig.domain_tab.conversation,
            crate::domain::models::PeerRowOperation::Remove {
                revision: 1,
                row_id: "row".into()
            },
        ));
        decision_queue::sync_recipient_drafts(&mut rig.state, &rig.domain_tab.conversation, false);
        assert!(rig.state.decisions.items().next().unwrap().is_stale());
        let painted = screen(&mut rig);
        assert!(painted.contains("not sent"), "{painted}");
        assert!(painted.contains("Copy unsent response"), "{painted}");
        rig.ch('c').await;
        assert_eq!(&*copied.lock(), &["my unsent reply"]);
        rig.enter().await;
        assert!(rig.frames().is_empty());
        assert_eq!(rig.state.input_buffer, "composer remains");
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_raw_guard_and_positive_send() {
        use crossterm::event::KeyEventKind;
        let mut rig = story21_6_attached_recipient_rig();
        for c in ['y', 'n', 'e', 'w', 'v'] {
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
                    if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                        continue;
                    }
                    rig.raw_key(KeyEvent::new_with_kind(KeyCode::Char(c), modifiers, kind))
                        .await;
                    assert!(rig.frames().is_empty());
                    assert!(
                        rig.state
                            .decisions
                            .holds(DecisionKind::RecipientDraftApproval)
                    );
                }
            }
        }
        rig.ch('y').await;
        assert!(
            matches!(rig.frames().as_slice(), [ClientFrame::ResolvePeerDraft {
            node, action: crate::adapters::daemon::protocol::PeerDraftAction::Approve
        }] if node == "node-a")
        );
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_field_enter_requires_plain_press() {
        use crossterm::event::KeyEventKind;
        let mut rig = story21_6_attached_recipient_rig();
        rig.ch('w').await;
        rig.ch('x').await;
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
                if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                    continue;
                }
                rig.raw_key(KeyEvent::new_with_kind(KeyCode::Enter, modifiers, kind))
                    .await;
                assert!(rig.frames().is_empty());
                assert!(
                    rig.state
                        .decisions
                        .holds(DecisionKind::RecipientDraftApproval)
                );
            }
        }
        rig.enter().await;
        assert!(
            matches!(rig.frames().as_slice(), [ClientFrame::ResolvePeerDraft {
            action: crate::adapters::daemon::protocol::PeerDraftAction::WriteOwn { content }, ..
        }] if content == "x")
        );
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_edit_and_own_leave_composer_untouched() {
        use crate::adapters::daemon::protocol::PeerDraftAction;
        for key in ['e', 'w'] {
            let mut rig = story21_6_attached_recipient_rig();
            rig.state.input_buffer = "other draft".to_owned();
            rig.state.cursor_position = 3;
            rig.state.input_scroll_offset = 2;
            let composer_height = |state: &TuiState| {
                crate::adapters::tui::layout::compute_layout(
                    Rect::new(0, 0, 80, 24),
                    &state.theme,
                    &state.input_buffer,
                    1,
                    false,
                )
                .unwrap()
                .input_area
                .height
            };
            let height = composer_height(&rig.state);
            let before = (
                rig.state.input_buffer.clone(),
                rig.state.cursor_position,
                rig.state.input_scroll_offset,
            );
            rig.ch(key).await;
            rig.ch('x').await;
            rig.enter().await;
            assert_eq!(
                (
                    rig.state.input_buffer.clone(),
                    rig.state.cursor_position,
                    rig.state.input_scroll_offset
                ),
                before
            );
            assert_eq!(composer_height(&rig.state), height);
            let frames = rig.frames();
            assert!(match frames.as_slice() {
                [
                    ClientFrame::ResolvePeerDraft {
                        node,
                        action: PeerDraftAction::Edit { content },
                    },
                ] if key == 'e' => node == "node-a" && content == "original draftx",
                [
                    ClientFrame::ResolvePeerDraft {
                        node,
                        action: PeerDraftAction::WriteOwn { content },
                    },
                ] if key == 'w' => node == "node-a" && content == "x",
                _ => false,
            });
        }
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_inert_then_send() {
        let mut rig = story21_6_attached_recipient_rig();
        rig.press(KeyCode::Esc, KeyModifiers::NONE).await;
        let key = rig
            .state
            .decisions
            .items()
            .find(|item| item.kind == DecisionKind::RecipientDraftApproval)
            .unwrap()
            .key();
        assert!(decision_queue::open(&mut rig.state, &key));
        rig.paint();
        rig.ch('y').await;
        assert!(rig.frames().is_empty());
        assert!(
            rig.state
                .decisions
                .holds(DecisionKind::RecipientDraftApproval)
        );
        rig.clock.advance(decision_queue::INERT_AFTER_PAINT);
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ResolvePeerDraft { .. }]
        ));
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_failed_enqueue_retains_item() {
        let mut rig = story21_6_attached_recipient_rig();
        rig.rx.close();
        rig.ch('n').await;
        assert!(
            rig.state
                .decisions
                .holds(DecisionKind::RecipientDraftApproval)
        );
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_recipient_other_tab_cannot_send() {
        let mut rig = story21_6_attached_recipient_rig();
        rig.domain_tab.conversation.id = "other".to_owned();
        rig.ch('y').await;
        assert!(rig.frames().is_empty());
        assert!(
            rig.state
                .decisions
                .holds(DecisionKind::RecipientDraftApproval)
        );
        rig.domain_tab.conversation.id = "recipient-origin".to_owned();
        rig.open_kind(DecisionKind::RecipientDraftApproval);
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ResolvePeerDraft { .. }]
        ));
    }

    #[tokio::test]
    async fn story21_6_attached_read_only_never_holds_consolidation_or_retract() {
        let mut rig = Rig::new(true);
        rig.consolidation(7);
        rig.retractable("peer-response-node-a", "auto reply");
        rig.ctrl('p').await;
        rig.type_text("retract").await;
        rig.enter().await;
        assert!(!rig.state.decisions.holds(DecisionKind::Consolidation));
        assert!(!decision_queue::team_retract_pending(&rig.state));
        assert!(rig.frames().is_empty());
    }

    // ── draw: the attached overlays at 60 / 80 / 130 columns ─────────────────

    fn paint_buffer(state: &TuiState, width: u16) -> ratatui::buffer::Buffer {
        paint_buffer_at(state, width, 24, &state.input_buffer)
    }

    fn paint_buffer_at(
        state: &TuiState,
        width: u16,
        height: u16,
        input: &str,
    ) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let layout = crate::adapters::tui::layout::compute_layout(
                    area,
                    &state.theme,
                    input,
                    1,
                    false,
                )
                .unwrap();
                let shell = crate::adapters::tui::presentation::Shell::new(area, &layout);
                crate::adapters::tui::presentation::compose(
                    f,
                    state,
                    &shell,
                    Host::Attached,
                    &log_facts("conversation-1"),
                    &mut crate::adapters::tui::presentation::occlusion::OpaqueRects::new(
                        shell.chat_pane,
                    ),
                    |_| {},
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn paint(state: &TuiState, width: u16) -> Vec<String> {
        let buffer = paint_buffer(state, width);
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    #[tokio::test]
    async fn story21_2_attached_menu_and_quit_frames_paint_complete_at_60_80_130() {
        for width in [60u16, 80, 130] {
            let mut rig = Rig::new(false);
            rig.ctrl('x').await;
            let rows = paint(&rig.state, width);
            let painted = rows.join("\n");
            for item in leader_menu(&AvailCtx::attached(width, false)) {
                assert!(
                    painted.contains(&item.text()),
                    "{width} cols: `{}` missing/clipped in\n{painted}",
                    item.text()
                );
            }
            // The menu never covers the status row.
            assert!(
                rows.last().unwrap().trim().is_empty(),
                "status row untouched"
            );

            let mut rig = Rig::new(false);
            rig.type_text("draft").await;
            rig.domain_tab.streaming.is_streaming = true;
            rig.ctrl('q').await;
            let painted = paint(&rig.state, width).join("\n");
            assert!(
                painted.contains("Leave this attached client?"),
                "{width}: {painted}"
            );
            assert!(
                painted.contains("Daemon work in progress continues"),
                "{painted}"
            );
            assert!(painted.contains("draft will be lost"), "{painted}");
        }
    }

    #[test]
    fn story21_2_attached_status_hint_never_says_esc_detaches() {
        let mut state = TuiState::new(80, 24);
        let hint = status_hint(&state).unwrap();
        assert_eq!(hint, "^X d detach");
        state.multiline_mode = true;
        assert!(status_hint(&state).unwrap().contains("[^X Enter] Send"));
        for hint in [hint, status_hint(&state).unwrap()] {
            assert!(!hint.contains("Esc") && !hint.contains("Ctrl+C"), "{hint}");
        }
        // The attached caret is always at the end: the composer window is the
        // visual tail of the width-aware projection (was `composer_scroll_offset`).
        let area = ratatui::layout::Rect::new(0, 0, 80, 10);
        assert_eq!(input_box::tail_scroll("one", area), 0);
        assert_eq!(
            input_box::tail_scroll(&"x\n".repeat(20), area),
            21 - input_box::MAX_INPUT_LINES
        );
    }

    #[test]
    fn story21_3_attached_chat_hint_names_i_and_never_repeats_the_menu_tail() {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Chat;
        let hint = status_hint(&state).unwrap();
        assert!(hint.starts_with("^X d detach · i "), "{hint}");
        assert!(!hint.contains("menu"), "{hint}");
        state.focus = FocusState::Input;
        assert_eq!(status_hint(&state).as_deref(), Some("^X d detach"));
    }

    // ── Story 21.1 AC5/AC6: S-LOG and toasts in the attached host ────────────

    #[tokio::test]
    async fn story21_1_attached_slash_notifications_opens_the_log_which_owns_every_key() {
        let mut rig = Rig::new(false);
        notifications::warning(&mut rig.state, NoticeSource::Attached, "something happened");
        rig.type_text("/notifications").await;
        rig.state.autocomplete.dismiss();
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert!(rig.state.input_buffer.is_empty(), "the command is consumed");
        assert!(rig.state.notification_log.is_some());
        assert!(rig.frames().is_empty(), "never sent to the daemon");
        assert_eq!(
            rig.state.notifications.unseen(),
            0,
            "presenting the log clears the badge"
        );
        // Every key belongs to S-LOG: text never reaches the composer, and `r`
        // is inert because the attached client has no retry path.
        rig.type_text("rhi").await;
        assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
        assert!(rig.frames().is_empty());
        assert!(rig.state.notification_log.is_some());
        // AC2/A18: even an idle exit leaves the interrupted Log untouched.
        let log = rig.state.notification_log.clone();
        assert_eq!(rig.ctrl('q').await, KeyFlow::Quit);
        assert!(rig.state.notification_log.is_some());
        assert_eq!(rig.state.notification_log, log);
    }

    #[tokio::test]
    async fn story21_1_attached_toasts_and_log_paint_in_overlay_order() {
        let mut rig = Rig::new(false);
        notifications::info(&mut rig.state, NoticeSource::Attached, "toast marker");
        let painted = paint(&rig.state, 80).join("\n");
        assert!(painted.contains("toast marker"), "{painted}");
        assert!(painted.contains("Info"), "severity word painted: {painted}");
        rig.ctrl('x').await;
        rig.ch('n').await;
        let painted = paint(&rig.state, 80).join("\n");
        assert!(painted.contains("Notifications (1)"), "{painted}");
    }

    /// K8 — both retained chords dispatch through the real attached chord
    /// arms: `Ctrl+X n` opens S-LOG with the origin focus as its caller,
    /// `Ctrl+X x` archives every visible toast and keeps the log rows.
    #[tokio::test]
    async fn story21_1_attached_ctrl_x_n_and_x_dispatch_through_the_chord_arms() {
        let mut rig = Rig::new(false);
        rig.state.focus = FocusState::Chat;
        notifications::notify(
            &mut rig.state,
            NoticeSource::Attached,
            NoticeSeverity::Error,
            "an error",
        );
        notifications::info(&mut rig.state, NoticeSource::Attached, "an info");
        assert_eq!(rig.state.notifications.visible().count(), 2);

        assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
        assert_eq!(rig.ch('x').await, KeyFlow::Continue);
        assert_eq!(rig.state.notifications.visible().count(), 0, "x clears");
        assert_eq!(rig.state.notifications.len(), 2, "the log keeps both");
        assert!(rig.state.notification_log.is_none(), "x is not the log");
        assert_eq!(rig.state.focus, FocusState::Chat);

        assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
        assert_eq!(rig.ch('n').await, KeyFlow::Continue);
        let log = rig.state.notification_log.as_ref().expect("S-LOG is open");
        assert_eq!(log.caller_focus, FocusState::Chat);
        assert!(rig.frames().is_empty(), "never sent to the daemon");
    }

    /// With an empty attached queue the chord answers without sending a frame.
    #[tokio::test]
    async fn story21_1_attached_ctrl_x_q_answers_that_no_decision_waits_in_this_client() {
        let mut rig = Rig::new(false);
        assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        let notice = rig.state.notifications.visible().next().expect("a toast");
        assert_eq!(notice.message, "No decisions waiting");
        assert_eq!(notice.severity, NoticeSeverity::Info);
        assert!(rig.state.decisions.is_empty());
        assert!(rig.frames().is_empty(), "never sent to the daemon");
    }

    /// K6 (attached families) — a refused send and a dead socket reach the user
    /// as store notices through the real key ingress, never as a status
    /// flash or a transcript row: the read-only send refusal is an Info toast,
    /// the dead-socket stop request is an Error toast.
    #[tokio::test]
    async fn story21_1_attached_flash_families_are_store_notices_not_status_or_rows() {
        use crate::domain::models::StatusState;

        let mut viewer = Rig::new(true);
        viewer.type_text("hello").await;
        assert_eq!(viewer.enter().await, KeyFlow::Continue);
        let refusal = viewer
            .state
            .notifications
            .visible()
            .next()
            .expect("a toast");
        assert_eq!(refusal.message, "read-only — can't send here");
        assert_eq!(refusal.severity, NoticeSeverity::Info);
        assert_eq!(refusal.source, NoticeSource::Attached);
        assert!(viewer.frames().is_empty(), "nothing reached the daemon");
        assert_eq!(viewer.state.status, StatusState::Idle, "no sticky status");
        assert!(viewer.domain_tab.conversation.messages.is_empty());

        let mut rig = Rig::new(false);
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.domain_tab.streaming.is_streaming = true;
        rig.rx.close();
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        let dead = rig.state.notifications.visible().next().expect("a toast");
        assert_eq!(
            dead.message,
            "Lost the connection to the daemon, so the stop request was not sent.\n\
             Reattach with `rustain daemon attach`, then stop the turn again."
        );
        assert_eq!(dead.severity, NoticeSeverity::Error);
        assert_eq!(rig.state.status, StatusState::Idle, "no sticky status");
        assert!(rig.domain_tab.conversation.messages.is_empty());
    }

    // ── Story 21.3 (3-A): the attached leader and KG-2 truth from the registry ──

    /// The crossterm event a terminal sends for one registry `Key`.
    pub(crate) fn key_event(key: Key) -> (KeyCode, KeyModifiers) {
        use KeyModifiers as M;
        match key {
            Key::Char(c) if c.is_ascii_uppercase() => (KeyCode::Char(c), M::SHIFT),
            Key::Char(c) => (KeyCode::Char(c), M::NONE),
            Key::Ctrl(c) => (KeyCode::Char(c), M::CONTROL),
            Key::Alt(c) => (KeyCode::Char(c), M::ALT),
            Key::Enter => (KeyCode::Enter, M::NONE),
            Key::ShiftEnter => (KeyCode::Enter, M::SHIFT),
            Key::CtrlEnter => (KeyCode::Enter, M::CONTROL),
            Key::AltEnter => (KeyCode::Enter, M::ALT),
            Key::Esc => (KeyCode::Esc, M::NONE),
            Key::Tab => (KeyCode::Tab, M::NONE),
            Key::ShiftTab => (KeyCode::BackTab, M::SHIFT),
            Key::Backspace => (KeyCode::Backspace, M::NONE),
            Key::Delete => (KeyCode::Delete, M::NONE),
            Key::Up => (KeyCode::Up, M::NONE),
            Key::Down => (KeyCode::Down, M::NONE),
            Key::Left => (KeyCode::Left, M::NONE),
            Key::Right => (KeyCode::Right, M::NONE),
            Key::Home => (KeyCode::Home, M::NONE),
            Key::End => (KeyCode::End, M::NONE),
            Key::PageUp => (KeyCode::PageUp, M::NONE),
            Key::PageDown => (KeyCode::PageDown, M::NONE),
            Key::F(n) => (KeyCode::F(n), M::NONE),
            Key::Other => (KeyCode::Insert, M::NONE),
            Key::Combo(c, bits) => {
                let held = [(1, M::CONTROL), (2, M::ALT), (4, M::SHIFT)];
                let mods = held
                    .iter()
                    .filter(|(bit, _)| bits & bit != 0)
                    .fold(M::NONE, |all, (_, m)| all | *m);
                (KeyCode::Char(c), mods)
            }
        }
    }

    impl Rig {
        /// A writer rig with `focus` current and `draft` in the composer.
        fn at(focus: FocusState, draft: &str) -> Self {
            let mut rig = Self::new(false);
            rig.state.focus = focus;
            rig.state.input_buffer = draft.to_owned();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();
            rig
        }

        pub(crate) async fn key(&mut self, key: Key) -> KeyFlow {
            let (code, modifiers) = key_event(key);
            self.press(code, modifiers).await
        }

        /// A transcript with room to scroll both ways, so a scroll key moves it.
        fn scrollable(mut self) -> Self {
            self.state.viewport_height = 20;
            self.state.total_content_height = 200;
            self.domain_tab.view_state.scroll_offset = 12;
            self.domain_tab.view_state.mode = crate::domain::models::AnchorMode::Reading;
            self
        }
    }

    /// Everything a key with no row must leave alone.
    #[derive(Debug, PartialEq)]
    struct Salient {
        focus: FocusState,
        input: String,
        multiline: bool,
        /// which-key, help, palette, S-LOG, S-QUIT, popup, thread window.
        surfaces: [bool; 7],
        scroll: (usize, bool),
        notices: usize,
        feedback: usize,
    }

    fn salient(rig: &Rig) -> Salient {
        let state = &rig.state;
        Salient {
            focus: state.focus.clone(),
            input: rig.state.input_buffer.clone(),
            multiline: state.multiline_mode,
            surfaces: [
                state.pending_leader.is_some(),
                presentation::is_open(state, Surface::Help),
                presentation::is_open(state, Surface::CommandPalette),
                presentation::is_open(state, Surface::NotificationLog),
                presentation::is_open(state, Surface::QuitConfirm),
                state.autocomplete.is_open(),
                state.thread_inspector.is_some(),
            ],
            scroll: (
                rig.domain_tab.view_state.scroll_offset,
                matches!(
                    rig.domain_tab.view_state.mode,
                    crate::domain::models::AnchorMode::Following
                ),
            ),
            notices: state.notifications.records().len(),
            feedback: state.feedback_blocks.len(),
        }
    }

    fn single_key(row: &keymap::Binding) -> Key {
        match row.keys {
            [key] => *key,
            other => panic!("{row:?}: an attached row is one key, got {other:?}"),
        }
    }

    #[test]
    fn story21_3_attached_truth_key_helper_round_trips_the_universe() {
        for key in Key::universe() {
            let (code, modifiers) = key_event(key);
            assert_eq!(
                Key::from_crossterm(&KeyEvent::new(code, modifiers)),
                key,
                "{key:?}"
            );
        }
    }

    /// K2 (attached): an unknown follow-up is consumed once, aloud — one info
    /// record, the leader closed, the origin focus and the composer untouched,
    /// nothing forwarded to the daemon.
    #[tokio::test]
    async fn story21_3_attached_unknown_follow_up_posts_no_binding_toast() {
        let unknown = [
            (Key::Char('j'), "j"),
            (Key::Char('5'), "5"),
            (Key::Char('f'), "f"),
            (Key::Tab, "Tab"),
            (Key::Left, "Left"),
            (Key::F(2), "F2"),
            (Key::Ctrl('a'), "Ctrl+A"),
            (Key::Alt('z'), "Alt+Z"),
            (Key::Ctrl('j'), "Ctrl+J"),
        ];
        for (key, label) in unknown {
            let mut rig = Rig::at(FocusState::Input, "draft");
            rig.ctrl('x').await;
            assert!(rig.state.pending_leader.is_some(), "{label}: leader open");
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{label}");
            assert!(rig.state.pending_leader.is_none(), "{label}: leader closed");
            assert_eq!(rig.state.focus, FocusState::Input, "{label}: origin");
            assert_eq!(
                rig.state.input_buffer, "draft",
                "{label}: composer unchanged"
            );
            assert!(rig.frames().is_empty(), "{label}: nothing forwarded");
            assert_eq!(rig.state.notifications.records().len(), 1, "{label}");
            let expected = format!("No binding for Ctrl+X {label} in Input");
            assert_eq!(rig.notice(NoticeSource::Chord), Some(expected.as_str()));
            assert_eq!(
                rig.state.notifications.records().last().map(|r| r.severity),
                Some(NoticeSeverity::Info),
                "{label}: an info record"
            );
            // Consumed exactly once: the next key is composer text again.
            rig.ch('x').await;
            assert_eq!(
                rig.state.input_buffer, "draftx",
                "{label}: not a second follow-up"
            );
        }

        // Enter is a send only from Input; from Chat it has no binding.
        let mut rig = Rig::at(FocusState::Chat, "draft");
        rig.ctrl('x').await;
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert_eq!(rig.state.notifications.records().len(), 1);
        assert_eq!(
            rig.notice(NoticeSource::Chord),
            Some("No binding for Ctrl+X Enter in Chat")
        );
        assert_eq!(rig.state.focus, FocusState::Chat);
        assert_eq!(rig.state.input_buffer, "draft");
        assert!(rig.frames().is_empty(), "no send from Chat");

        // Positive control: a bound follow-up posts no no-binding record.
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('x').await;
        assert_eq!(rig.ch('?').await, KeyFlow::Continue);
        assert!(presentation::is_open(&rig.state, Surface::Help));
        assert_eq!(rig.state.notifications.records().len(), 0);

        // Esc and Ctrl+C cancel silently and never reach the interrupt path,
        // even with a live projected turn.
        for key in [Key::Esc, Key::Ctrl('c')] {
            let mut rig = Rig::at(FocusState::Chat, "draft");
            rig.interrupt.apply(&DaemonFrame::ActiveTurn {
                generation: Some(TurnGeneration(7)),
            });
            rig.domain_tab.streaming.is_streaming = true;
            rig.ctrl('x').await;
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{key:?}");
            assert!(rig.state.pending_leader.is_none(), "{key:?}: closed");
            assert_eq!(rig.state.focus, FocusState::Chat, "{key:?}: origin");
            assert_eq!(rig.state.notifications.records().len(), 0, "{key:?}");
            assert!(rig.state.quit_confirm.is_none(), "{key:?}");
            assert!(rig.frames().is_empty(), "{key:?}: no InterruptTurn");
        }
    }

    /// K2 (attached): every Leader row the attached host sees runs its command
    /// by value through the key front door. The `match` is exhaustive, so a new
    /// `LeaderCmd` forces a witness here.
    #[tokio::test]
    async fn story21_3_attached_leader_runs_every_registry_row() {
        let avail = AvailCtx::attached(80, false);
        let mut ran = 0;
        for row in rows_of(Context::Leader, Host::Attached) {
            let CommandId::Leader(cmd) = row.command else {
                panic!("{row:?}: a Leader row carries a LeaderCmd");
            };
            let [key] = row.keys else {
                panic!("{row:?}: a follow-up is one key");
            };
            let key = *key;
            let label = row.long_label();
            let mut rig = Rig::at(FocusState::Input, "draft");
            if cmd == LeaderCmd::ClearToasts {
                notifications::info(&mut rig.state, NoticeSource::Attached, "one");
            }
            let seeded = rig.state.notifications.records().len();
            rig.ctrl('x').await;
            assert!(rig.state.pending_leader.is_some(), "{label}: leader open");
            let flow = rig.key(key).await;
            let posted = rig.state.notifications.records().len() - seeded;
            assert_eq!(
                rig.state.pending_leader.is_some(),
                cmd == LeaderCmd::QuitRequest,
                "{label}: AC2/A18 retains only the Quit caller's pending leader"
            );
            let expected_flow = if cmd == LeaderCmd::Detach {
                KeyFlow::Detach
            } else {
                KeyFlow::Continue
            };
            assert_eq!(flow, expected_flow, "{label}");
            match cmd {
                LeaderCmd::Profile
                | LeaderCmd::Model
                | LeaderCmd::SignIn
                | LeaderCmd::Adapters
                | LeaderCmd::Agents
                | LeaderCmd::Room
                | LeaderCmd::Artifacts
                | LeaderCmd::Tasks
                | LeaderCmd::Usage
                | LeaderCmd::History
                | LeaderCmd::Monitor => {
                    let Availability::Unavailable(remedy) = row.availability(&avail) else {
                        panic!("{label}: unavailable in the attached host");
                    };
                    let source = if remedy == ATTACHED_SIGN_IN {
                        NoticeSource::Login
                    } else {
                        NoticeSource::Chord
                    };
                    assert_eq!(rig.notice(source), Some(remedy), "{label}");
                    assert_eq!(posted, 1, "{label}: exactly the remedy");
                    assert!(rig.frames().is_empty(), "{label}: sends nothing");
                    assert_eq!(rig.state.input_buffer, "draft", "{label}");
                    assert_eq!(rig.state.focus, FocusState::Input, "{label}");
                }
                LeaderCmd::A2aLog => {
                    assert_eq!(
                        rig.state.modal_stack.top().map(|frame| frame.surface),
                        Some(Surface::MissionControl),
                        "{label}"
                    );
                    assert_eq!(
                        rig.state.mission_control.title(),
                        "Mission Control · Messages",
                        "{label}"
                    );
                    assert!(
                        rig.state
                            .mission_control
                            .messages
                            .as_ref()
                            .is_some_and(|page| !page.filtered),
                        "{label}"
                    );
                    assert!(rig.frames().is_empty(), "{label}: a local read");
                    assert_eq!(rig.state.input_buffer, "draft", "{label}");
                }
                LeaderCmd::Detach => {
                    assert!(rig.frames().is_empty(), "{label}: the loop sends Detach");
                    // Follow-up letters are case-insensitive.
                    let mut upper = Rig::at(FocusState::Input, "draft");
                    upper.ctrl('x').await;
                    assert_eq!(upper.ch('D').await, KeyFlow::Detach);
                }
                LeaderCmd::Help => {
                    assert!(presentation::is_open(&rig.state, Surface::Help), "{label}");
                    assert_eq!(rig.state.focus, FocusState::Input);
                    assert_eq!(posted, 0, "{label}");
                }
                LeaderCmd::Notifications => {
                    assert!(rig.state.notification_log.is_some(), "{label}");
                    assert_eq!(rig.state.focus, FocusState::Input, "{label}: caller");
                    assert_eq!(rig.state.input_buffer, "draft", "{label}");
                }
                LeaderCmd::ClearToasts => {
                    assert_eq!(rig.state.notifications.visible().count(), 0, "{label}");
                    assert_eq!(rig.state.notifications.records().len(), 1, "log keeps it");
                }
                LeaderCmd::Decisions => {
                    let toast = rig.state.notifications.visible().next().expect("a toast");
                    assert_eq!(toast.message, "No decisions waiting");
                    assert_eq!(toast.severity, NoticeSeverity::Info);
                    assert_eq!(posted, 1, "{label}");
                    assert!(rig.frames().is_empty(), "{label}: a local answer");
                    rig.approval_arrives("leader-witness");
                    rig.ctrl('x').await;
                    rig.ch('q').await;
                    assert!(presentation::is_open(&rig.state, Surface::DecisionQueue));
                    assert_eq!(rig.state.decisions.undecided(), 1);
                }
                LeaderCmd::Send => {
                    let frames = rig.frames();
                    assert_eq!(frames.len(), 1, "{label}: one user frame");
                    assert_eq!(user_text(&frames[0]), Some("draft"));
                    assert!(rig.state.input_buffer.is_empty(), "{label}: composer sent");
                    assert_eq!(posted, 0, "{label}");
                }
                LeaderCmd::Cancel => {
                    assert_eq!(posted, 0, "{label}: cancels silently");
                    assert!(rig.frames().is_empty(), "{label}");
                    assert_eq!(rig.state.input_buffer, "draft", "{label}");
                    assert_eq!(rig.state.focus, FocusState::Input, "{label}");
                }
                LeaderCmd::QuitRequest => {
                    assert!(rig.state.quit_confirm.is_some(), "{label}: asks first");
                    assert!(rig.frames().is_empty(), "{label}: nothing is interrupted");
                    assert_eq!(rig.state.focus, FocusState::Input);
                    assert_eq!(rig.state.pending_leader, Some(Context::Input));
                }
            }
            ran += 1;
        }
        assert!(
            ran >= 19,
            "the attached leader table was walked ({ran} rows)"
        );
    }

    /// K4 (attached): for the Global, Input and Chat rows visible to the
    /// attached host, each row's key produces its command's effect by value.
    /// A row without a witness fails the test (the wildcard arms panic), so a
    /// new attached command forces one.
    #[tokio::test]
    async fn story21_3_attached_truth_rows_produce_their_commands_effect() {
        let mut checked = 0;

        for focus in [FocusState::Input, FocusState::Chat] {
            for row in rows_of(Context::Global, Host::Attached) {
                let CommandId::Global(cmd) = row.command else {
                    panic!("{row:?}: a Global row carries a GlobalCmd");
                };
                let key = single_key(row);
                match cmd {
                    GlobalCmd::OpenLeader => {
                        let mut rig = Rig::at(focus.clone(), "draft");
                        assert_eq!(rig.key(key).await, KeyFlow::Continue);
                        assert!(rig.state.pending_leader.is_some(), "{focus:?}");
                        assert_eq!(rig.state.focus, focus);
                        assert_eq!(rig.state.input_buffer, "draft");
                    }
                    GlobalCmd::QuitRequest => {
                        let mut rig = Rig::at(focus.clone(), "draft");
                        assert_eq!(rig.key(key).await, KeyFlow::Continue);
                        assert!(rig.state.quit_confirm.is_some(), "{focus:?}: asks first");
                        assert_eq!(rig.state.input_buffer, "draft");
                        let mut idle = Rig::at(focus.clone(), "");
                        assert_eq!(idle.key(key).await, KeyFlow::Quit, "{focus:?}: idle");
                    }
                }
                checked += 1;
            }
        }

        for row in rows_of(Context::Input, Host::Attached) {
            let CommandId::Input(cmd) = row.command else {
                panic!("{row:?}: an Input row carries an InputCmd");
            };
            let key = single_key(row);
            let draft = if cmd == InputCmd::OpenSlashPopup {
                ""
            } else {
                "hi"
            };
            let mut rig = Rig::at(FocusState::Input, draft).scrollable();
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{row:?}");
            let frames = rig.frames();
            let no_frames = |what: &str| assert!(frames.is_empty(), "{what}: {frames:?}");
            match cmd {
                InputCmd::SubmitOrNewline => {
                    assert_eq!(frames.len(), 1, "{row:?}");
                    assert_eq!(user_text(&frames[0]), Some("hi"));
                    assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
                    let mut multiline = Rig::at(FocusState::Input, "hi");
                    multiline.state.multiline_mode = true;
                    multiline.key(key).await;
                    assert_eq!(
                        multiline.state.input_buffer, "hi\n",
                        "multi-line: a newline"
                    );
                    assert!(multiline.frames().is_empty());
                }
                InputCmd::Send => {
                    assert_eq!(frames.len(), 1, "{row:?}");
                    assert_eq!(user_text(&frames[0]), Some("hi"));
                    assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
                }
                InputCmd::InsertNewline => {
                    no_frames("newline");
                    assert_eq!(rig.state.input_buffer, "hi\n", "{row:?}");
                }
                InputCmd::LineStart
                | InputCmd::LineEnd
                | InputCmd::CursorLeft
                | InputCmd::CursorRight
                | InputCmd::CursorHome
                | InputCmd::CursorEnd
                | InputCmd::DeleteForward
                | InputCmd::KillToLineEnd => {
                    no_frames("shared caret editor");
                    let expected = if matches!(cmd, InputCmd::LineStart | InputCmd::CursorHome) {
                        0
                    } else if cmd == InputCmd::CursorLeft {
                        1
                    } else {
                        2
                    };
                    assert_eq!(rig.state.cursor_position, expected, "{row:?}");
                    assert_eq!(rig.state.input_buffer, "hi");
                }
                InputCmd::DeleteWordBack | InputCmd::KillToLineStart => {
                    no_frames("Ctrl+W/U");
                    assert_eq!(
                        rig.state.input_buffer, "",
                        "the tail of the one-line draft goes"
                    );
                }
                InputCmd::ToggleMultiline => {
                    no_frames("Alt+M");
                    assert!(rig.state.multiline_mode);
                    assert_eq!(rig.state.input_buffer, "hi", "Alt+M is not typed");
                }
                InputCmd::ReturnToChat => {
                    no_frames("Esc");
                    assert_eq!(rig.state.focus, FocusState::Chat);
                    assert_eq!(rig.state.input_buffer, "hi", "unsent text kept");
                }
                InputCmd::OpenPalette => {
                    assert!(presentation::is_open(&rig.state, Surface::CommandPalette));
                    assert_eq!(rig.state.input_buffer, "hi");
                }
                InputCmd::CancelOrQuit => {
                    no_frames("Ctrl+C");
                    assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
                    assert_eq!(rig.state.input_buffer, "hi");
                }
                InputCmd::DeleteBack => {
                    no_frames("Backspace");
                    assert_eq!(rig.state.input_buffer, "h");
                }
                InputCmd::OpenSlashPopup => {
                    no_frames("/");
                    assert_eq!(rig.state.input_buffer, "/");
                    assert!(rig.state.autocomplete.is_open());
                    assert_eq!(
                        rig.state.autocomplete.kind,
                        Some(AutocompleteKind::SlashCommand)
                    );
                }
                InputCmd::TranscriptPageUp => {
                    no_frames("PgUp");
                    // Legacy bounded page; the owned ViewState, not a scalar, receives it.
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        (17, false)
                    );
                }
                InputCmd::TranscriptPageDown => {
                    no_frames("PgDn");
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        (7, false)
                    );
                }
                // Any other command is local-only: it must not be an attached row.
                _ => panic!("{row:?}: an attached Input row has no witness"),
            }
            checked += 1;
        }

        for row in rows_of(Context::Chat, Host::Attached) {
            let CommandId::Chat(cmd) = row.command else {
                panic!("{row:?}: a Chat row carries a ChatCmd");
            };
            let key = single_key(row);
            // Viewport6: half-page3; full-page5 retains one visible overlap row.
            let mut rig = Rig::at(FocusState::Chat, "hi").scrollable();
            rig.state.viewport_height = 6;
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{row:?}");
            assert!(rig.frames().is_empty(), "{row:?}: a Chat key sends nothing");
            match cmd {
                ChatCmd::FocusToggle | ChatCmd::FocusInput => {
                    assert_eq!(rig.state.focus, FocusState::Input, "{row:?}");
                    assert_eq!(rig.state.input_buffer, "hi");
                }
                ChatCmd::Quit | ChatCmd::CancelOrQuit => {
                    assert!(
                        rig.state.quit_confirm.is_some(),
                        "{row:?}: a draft asks first"
                    );
                    assert_eq!(rig.state.input_buffer, "hi");
                    let mut idle = Rig::at(FocusState::Chat, "");
                    assert_eq!(idle.key(key).await, KeyFlow::Quit, "{row:?}: idle");
                }
                ChatCmd::OpenHelp => {
                    assert!(presentation::is_open(&rig.state, Surface::Help));
                    assert_eq!(rig.state.focus, FocusState::Chat);
                }
                ChatCmd::OpenPalette => {
                    assert!(presentation::is_open(&rig.state, Surface::CommandPalette))
                }
                ChatCmd::ScrollHalfPageDown => {
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        (9, false),
                        "{row:?}"
                    );
                }
                ChatCmd::ScrollPageDown => {
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        (7, false),
                        "{row:?}"
                    );
                }
                ChatCmd::ScrollHalfPageUp => {
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        (15, false),
                        "{row:?}"
                    );
                }
                ChatCmd::ScrollPageUp => {
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        (17, false),
                        "{row:?}"
                    );
                }
                // Any other command is local-only: it must not be an attached row.
                _ => panic!("{row:?}: an attached Chat row has no witness"),
            }
            checked += 1;
        }
        assert!(checked >= 20, "the attached rows were walked ({checked})");
    }

    /// K4 (attached): every universe key with no row in the context (own,
    /// inherited or Global) is inert; printable keys in Input append exactly
    /// themselves. Local-only rows are "no row" for this host.
    #[tokio::test]
    async fn story21_3_attached_truth_keys_without_a_row_are_inert_or_plain_typing() {
        let mut swept = 0;
        for (context, focus) in [
            (Context::Input, FocusState::Input),
            (Context::Chat, FocusState::Chat),
        ] {
            for key in Key::universe() {
                let has_row = !matches!(resolve(context, Host::Attached, &[key]), Resolved::Miss)
                    || matches!(
                        resolve(Context::Global, Host::Attached, &[key]),
                        Resolved::Hit(_)
                    );
                if has_row {
                    continue;
                }
                let mut rig = Rig::at(focus.clone(), "abc").scrollable();
                let mut expected = salient(&rig);
                assert_eq!(rig.key(key).await, KeyFlow::Continue, "{context:?} {key:?}");
                assert!(rig.frames().is_empty(), "{context:?} {key:?}: sent a frame");
                if let (Context::Input, Key::Char(c)) = (context, key) {
                    expected.input.push(c);
                }
                assert_eq!(
                    salient(&rig),
                    expected,
                    "{context:?} {key:?} has no row and must not act"
                );
                swept += 1;
            }
        }
        assert!(swept > 250, "the sweep covered the universe ({swept} keys)");

        // Positive control: keys WITH a row visibly change the same snapshot.
        for (focus, key) in [
            (FocusState::Input, Key::Esc),
            (FocusState::Chat, Key::Char('i')),
            (FocusState::Input, Key::Alt('m')),
            (FocusState::Chat, Key::PageUp),
            (FocusState::Input, Key::Backspace),
        ] {
            let mut rig = Rig::at(focus, "abc").scrollable();
            let before = salient(&rig);
            rig.key(key).await;
            assert_ne!(salient(&rig), before, "{key:?} has a row and changes state");
        }
    }

    /// AC12 (attached): a Ctrl/Alt-modified character is a consumed no-op in
    /// both foci: never typed, never a command (Ctrl+J does not scroll, Ctrl+Y
    /// approves nothing, Alt+C/L/U type no letters; Ctrl+A/K have no cursor to
    /// move; Ctrl+W/U/H are the append-only editing rows, witnessed above).
    #[tokio::test]
    async fn story21_3_attached_truth_modified_characters_are_consumed_never_typed() {
        let mut events = Vec::new();
        for c in ['a', 'k', 'j', 'y'] {
            events.push((KeyCode::Char(c), KeyModifiers::CONTROL));
        }
        for c in ['c', 'l', 'u', 'x', '/', '1'] {
            events.push((KeyCode::Char(c), KeyModifiers::ALT));
        }
        events.push((KeyCode::Char('C'), KeyModifiers::ALT | KeyModifiers::SHIFT));
        for focus in [FocusState::Input, FocusState::Chat] {
            for (code, modifiers) in &events {
                let mut rig = Rig::at(focus.clone(), "abc").scrollable();
                let before = salient(&rig);
                assert_eq!(rig.press(*code, *modifiers).await, KeyFlow::Continue);
                assert!(rig.frames().is_empty(), "{code:?}+{modifiers:?}");
                assert_eq!(
                    salient(&rig),
                    before,
                    "{focus:?}: {code:?}+{modifiers:?} must be consumed"
                );
            }
        }
        // Control: the same letter unmodified is typed in Input.
        let mut rig = Rig::at(FocusState::Input, "abc");
        rig.ch('a').await;
        assert_eq!(rig.state.input_buffer, "abca");
    }

    /// A12 (attached): opening the slash popup runs discovery (`sync_slash`)
    /// and announces each shadowed custom command once per generation.
    #[tokio::test]
    async fn story21_3_attached_announces_shadows_after_sync() {
        let mut rig = Rig::new(false);
        let commands = rig.workspace.path().join(".claude").join("commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("quit.md"), "shadowed").unwrap();
        std::fs::write(commands.join("deploy.md"), "not shadowed").unwrap();
        let warnings = |rig: &Rig| -> Vec<String> {
            rig.state
                .notifications
                .records()
                .filter(|r| r.severity == NoticeSeverity::Warning)
                .map(|r| r.message.clone())
                .collect()
        };
        assert!(warnings(&rig).is_empty(), "nothing before discovery");

        rig.ch('/').await;
        assert!(rig.state.autocomplete.is_open());
        let first = warnings(&rig);
        assert_eq!(
            first.len(),
            1,
            "one Warning, for the shadowed name: {first:?}"
        );
        assert!(first[0].contains("/quit"), "{first:?}");
        assert!(!first[0].contains("deploy"), "a free name stays silent");

        // Same discovery generation: closing and reopening the popup is silent.
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(!rig.state.autocomplete.is_open());
        rig.ch('/').await;
        assert!(rig.state.autocomplete.is_open());
        assert_eq!(warnings(&rig), first, "no repeat in the same generation");
    }

    /// AC20a, amended by 21-4 AC2/A18: Ctrl+Q asks without closing the leader;
    /// Cancel returns to the same pending sequence, never a no-binding toast.
    #[tokio::test]
    async fn story21_3_attached_ctrl_q_with_the_leader_open_asks_to_quit() {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('x').await;
        let focus = rig.state.focus.clone();
        let pending_leader = rig.state.pending_leader;
        assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
        assert!(
            rig.state.pending_leader.is_some(),
            "the leader stays underneath Quit"
        );
        assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
        assert_eq!(rig.state.notifications.records().len(), 0, "no toast");
        assert_eq!(rig.state.input_buffer, "draft");
        assert!(rig.frames().is_empty());
        rig.esc().await;
        assert!(
            rig.state.pending_leader.is_some(),
            "Cancel restores the leader"
        );
        assert_eq!(rig.state.focus, focus);
        assert_eq!(rig.state.pending_leader, pending_leader);
        rig.ch('?').await;
        assert!(
            presentation::is_open(&rig.state, Surface::Help),
            "the pending sequence still runs"
        );
        // Idle and clean: the same keys leave the client.
        let mut rig = Rig::at(FocusState::Input, "");
        rig.ctrl('x').await;
        assert_eq!(rig.ctrl('q').await, KeyFlow::Quit);
    }

    /// AC20b (attached leg): an extra-modifier key is its own key, never Ctrl+C,
    /// Ctrl+X, Ctrl+Q or Alt+M, with a live turn that Ctrl+C would interrupt.
    #[tokio::test]
    async fn story21_3_attached_extra_modifier_keys_never_alias_plain_keys() {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.domain_tab.streaming.is_streaming = true;
        for key in [
            Key::Combo('c', 5),
            Key::Combo('q', 5),
            Key::Combo('x', 5),
            Key::Combo('m', 6),
        ] {
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{key:?}");
        }
        assert!(rig.frames().is_empty(), "Ctrl+Shift+C never interrupts");
        assert!(rig.state.quit_confirm.is_none(), "Ctrl+Shift+Q never quits");
        assert!(
            rig.state.pending_leader.is_none(),
            "Ctrl+Shift+X opens no leader"
        );
        assert!(!rig.state.multiline_mode, "Alt+Shift+M toggles nothing");
        assert_eq!(rig.state.input_buffer, "draft");
        // Positive controls: the plain keys still act.
        rig.key(Key::Alt('m')).await;
        assert!(rig.state.multiline_mode);
        rig.ctrl('c').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::InterruptTurn { .. }]
        ));
        // In the open leader the combined key is a named unknown follow-up.
        rig.ctrl('x').await;
        rig.key(Key::Combo('c', 5)).await;
        assert_eq!(
            rig.notice(NoticeSource::Chord),
            Some("No binding for Ctrl+X Ctrl+Shift+C in Input")
        );
    }

    /// K10 (attached leg, AC13): Ctrl+C in an open surface closes exactly that
    /// surface and sends no `InterruptTurn`, with a live turn; the positive control
    /// (no surface) interrupts.
    #[tokio::test]
    async fn story21_3_attached_ctrl_c_in_overlay_never_interrupts() {
        async fn live_rig() -> Rig {
            let mut rig = Rig::at(FocusState::Input, "draft");
            rig.interrupt.apply(&DaemonFrame::ActiveTurn {
                generation: Some(TurnGeneration(7)),
            });
            rig.domain_tab.streaming.is_streaming = true;
            rig
        }
        // Help (leader `?`), the slash popup, S-LOG (leader `n`), S-QUIT (Ctrl+Q).
        let mut rig = live_rig().await;
        rig.ctrl('x').await;
        rig.ch('?').await;
        assert!(presentation::is_open(&rig.state, Surface::Help));
        rig.ctrl('c').await;
        assert!(
            !presentation::is_open(&rig.state, Surface::Help),
            "Help closed"
        );
        assert_eq!(rig.state.focus, FocusState::Input, "caller restored");
        assert!(rig.frames().is_empty(), "Help: no interrupt");

        let mut rig = live_rig().await;
        rig.state.input_buffer.clear();
        rig.state.cursor_position = 0;
        rig.state.input_scroll_offset = 0;
        rig.ch('/').await;
        assert!(rig.state.autocomplete.is_open());
        rig.ctrl('c').await;
        assert!(!rig.state.autocomplete.is_open(), "popup closed");
        assert!(
            rig.state.input_buffer.is_empty(),
            "the composer is restored"
        );
        assert!(rig.frames().is_empty(), "popup: no interrupt");

        let mut rig = live_rig().await;
        rig.ctrl('x').await;
        rig.ch('n').await;
        assert!(rig.state.notification_log.is_some());
        rig.ctrl('c').await;
        assert!(rig.state.notification_log.is_none(), "S-LOG closed");
        assert!(rig.frames().is_empty(), "S-LOG: no interrupt");

        let mut rig = live_rig().await;
        rig.ctrl('q').await;
        assert!(rig.state.quit_confirm.is_some());
        rig.ctrl('c').await;
        assert!(rig.state.quit_confirm.is_none(), "S-QUIT cancelled");
        assert!(rig.frames().is_empty(), "S-QUIT: no interrupt");

        let mut rig = live_rig().await;
        rig.ctrl('c').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::InterruptTurn { .. }]
        ));
    }

    /// K8 migrated by AC16: tail deletions remain grapheme-safe and the same
    /// authoritative composer now supports real Ctrl+A/E/K caret motions.
    #[tokio::test]
    async fn story21_3_attached_composer_edits_its_tail_by_grapheme() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let mut rig = Rig::at(FocusState::Input, &format!("alpha beta {family}"));
        rig.ctrl('w').await;
        assert_eq!(
            rig.state.input_buffer, "alpha beta ",
            "the family is one word"
        );
        rig.ctrl('w').await;
        assert_eq!(rig.state.input_buffer, "alpha ");
        rig.state.input_buffer = "xe\u{301}".into();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        rig.ctrl('h').await;
        assert_eq!(
            rig.state.input_buffer, "x",
            "the accent goes with its letter"
        );
        rig.state.input_buffer = "one\ntwo three".into();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        rig.ctrl('u').await;
        assert_eq!(
            rig.state.input_buffer, "one\n",
            "Ctrl+U deletes the last logical line"
        );
        rig.state.input_buffer = "keep".into();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        for (c, cursor) in [('a', 0), ('e', 4), ('k', 4)] {
            assert_eq!(rig.ctrl(c).await, KeyFlow::Continue);
            assert_eq!(rig.state.input_buffer, "keep");
            assert_eq!(
                rig.state.cursor_position, cursor,
                "Ctrl+{c} moves the real caret"
            );
        }
        // Chat focus: the keys do not edit the unsent draft.
        let mut rig = Rig::at(FocusState::Chat, "draft");
        rig.ctrl('w').await;
        assert_eq!(rig.state.input_buffer, "draft");
    }

    // ── Story 21.3 slice 3-B1: attached twins of the K4 overlay witnesses ────
    // Per overlay: one witness per attached row (outcome by value, through `handle_attached_key`),
    // then a sweep: every `Key::universe()` key without a row leaves the state alone (fresh rig per key).

    /// One line of the state an overlay's rows move (compared by value).
    type Digest = fn(&Rig) -> String;

    fn attached_keys(context: Context) -> Vec<Key> {
        rows_of(context, Host::Attached).map(single_key).collect()
    }

    /// Every attached row of `context` has a witness and yields its `flow | digest`, sending no frame.
    #[rustfmt::skip]
    async fn witness_rows<F, Fut>(context: Context, open: F, digest: Digest, cases: &[(Key, String)])
    where F: Fn(Key) -> Fut, Fut: Future<Output = Rig>,
    {
        let rows = attached_keys(context);
        for key in &rows {
            assert!(cases.iter().any(|(k, _)| k == key), "{context:?}: row {key:?} has no witness");
        }
        for (key, expected) in cases {
            assert!(rows.contains(key), "{context:?}: {key:?} is not an attached row");
            let mut rig = open(*key).await;
            let flow = rig.key(*key).await;
            assert_eq!(&format!("{flow:?} | {}", digest(&rig)), expected, "{context:?} {key:?}");
            match command_in(context, *key) {
                Some(CommandId::Global(GlobalCmd::OpenLeader)) => {
                    assert_eq!(rig.state.pending_leader, Some(context));
                }
                Some(CommandId::Input(InputCmd::OpenPalette)) => {
                    assert_eq!(rig.state.modal_stack.top().unwrap().surface, Surface::CommandPalette);
                    assert!(!rig.state.command_palette.filtered_entries.is_empty());
                }
                _ => {}
            }
            assert!(rig.frames().is_empty(), "{context:?} {key:?} sent a frame");
        }
    }

    /// Every non-`exempt` universe key without a row is inert (`Continue`, state, no frame); offenders are listed at once.
    #[rustfmt::skip]
    async fn witness_inert<F, Fut>(context: Context, open: F, digest: Digest, exempt: fn(Key) -> bool)
    where F: Fn(Key) -> Fut, Fut: Future<Output = Rig>,
    {
        let rows = attached_keys(context);
        let mut bad = Vec::new();
        for key in Key::universe().into_iter().filter(|k| !rows.contains(k) && !exempt(*k)) {
            let mut rig = open(key).await;
            let before = (salient(&rig), digest(&rig));
            let flow = rig.key(key).await;
            let after = (salient(&rig), digest(&rig));
            if flow != KeyFlow::Continue || before != after || !rig.frames().is_empty() {
                bad.push(format!("{key:?}: {flow:?}\n  {}\n  {}", before.1, after.1));
            }
        }
        assert!(bad.is_empty(), "{context:?}: keys without a row acted: {bad:#?}");
    }

    fn open_surface(rig: &Rig) -> String {
        let s = &rig.state;
        format!(
            "quit={} focus={:?} input={}",
            s.quit_confirm.is_some(),
            s.focus,
            rig.state.input_buffer
        )
    }

    async fn help_rig(_: Key) -> Rig {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('x').await;
        rig.ch('?').await;
        rig.state.help_overlay.scroll_offset = 5;
        rig
    }

    #[rustfmt::skip]
    fn help_digest(rig: &Rig) -> String {
        let h = &rig.state.help_overlay;
        let scroll = if h.scroll_offset > 1_000_000 { "bottom".to_owned() } else { h.scroll_offset.to_string() };
        format!("help={} scroll={scroll} {}{}", presentation::is_open(&rig.state, Surface::Help), open_surface(rig), if h.search.editing { " search=active" } else { "" })
    }

    /// Catches a Help row that scrolls the wrong way, closes into the wrong focus or skips the quit question, or a rowless key that acts.
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_help() {
        let at = |scroll: &str| format!("Continue | help=true scroll={scroll} quit=false focus=Input input=draft");
        let closed = |quit: bool| format!("Continue | help=false scroll=5 quit={quit} focus=Input input=draft");
        let cases = [
            (Key::Char('j'), at("6")), (Key::Down, at("6")), (Key::Char('k'), at("4")), (Key::Up, at("4")),
            (Key::Char('G'), at("bottom")), (Key::Char('g'), at("0")),
            (Key::Char('?'), closed(false)), (Key::Esc, closed(false)), (Key::Ctrl('c'), closed(false)),
            // AC2/A18: the exact Help caller survives underneath Quit.
            (Key::Ctrl('q'), "Continue | help=true scroll=5 quit=true focus=Input input=draft".to_owned()),
            (Key::Ctrl('x'), at("5")), (Key::Ctrl('p'), at("5")),
            (Key::Char('/'), at("5") + " search=active"),
        ];
        witness_rows(Context::Help, help_rig, help_digest, &cases).await;
        witness_inert(Context::Help, help_rig, help_digest, |_| false).await;
    }

    async fn palette_rig(key: Key) -> Rig {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('p').await;
        match key {
            Key::Enter | Key::Backspace => rig.type_text("ml").await,
            _ => {
                rig.key(Key::Down).await; // selection 1: Up and Down both move
            }
        }
        rig
    }

    #[rustfmt::skip]
    fn palette_digest(rig: &Rig) -> String {
        let p = &rig.state.command_palette;
        format!("palette={} sel={} filter={} ml={} {}", presentation::is_open(&rig.state, Surface::CommandPalette), p.selected_index, p.filter_text.as_str(), rig.state.multiline_mode, open_surface(rig))
    }

    /// Catches a palette row that runs the wrong entry, keeps the palette open, edits the filter wrongly or swallows Ctrl+Q.
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_command_palette() {
        use crate::adapters::tui::keymap::table_input::FieldCmd;
        let open = |sel: u8, filter: &str, ml: bool| format!("Continue | palette=true sel={sel} filter={filter} ml={ml} quit=false focus=Input input=draft");
        let closed = |ml: bool, quit: bool| format!("Continue | palette=false sel=0 filter= ml={ml} quit={quit} focus=Input input=draft");
        let cases = [
            (Key::Up, open(0, "", false)), (Key::Down, open(2, "", false)),
            (Key::Enter, closed(true, false)), // "ml" is the first entry: toggles multiline, draft untouched
            (Key::Esc, closed(false, false)), (Key::Ctrl('c'), closed(false, false)),
            // AC2/A18: selection and filter survive underneath Quit.
            (Key::Ctrl('q'), "Continue | palette=true sel=1 filter= ml=false quit=true focus=Input input=draft".to_owned()),
            (Key::Ctrl('x'), open(1, "", false)), (Key::Ctrl('p'), open(1, "", false)),
        ];
        witness_rows(Context::CommandPalette, palette_rig, palette_digest, &cases).await;
        // Backspace moved from an overlay row to the shared caret-field ancestor.
        assert!(matches!(keymap::resolve(Context::CommandPalette, Host::Attached, &[Key::Backspace]),
            Resolved::Hit(row) if row.context == Context::CaretField && row.command == CommandId::Field(FieldCmd::DeleteBack)));
        let mut backspace = palette_rig(Key::Backspace).await;
        let flow = backspace.key(Key::Backspace).await;
        assert_eq!(format!("{flow:?} | {}", palette_digest(&backspace)), open(0, "m", false));
        assert!(backspace.frames().is_empty());
        // Non-row keys stay inert; shared field rows have exhaustive edit witnesses above.
        witness_inert(Context::CommandPalette, palette_rig, palette_digest, |key| matches!(key, Key::Char(_)) ||
            matches!(keymap::resolve(Context::CommandPalette, Host::Attached, &[key]), Resolved::Hit(row)
                if matches!(row.command, CommandId::Field(_)))).await;
        // The inherited caret rows retain the old end-of-query edit behavior.
        for (key, want) in [(Key::Ctrl('w'), "ab "), (Key::Ctrl('u'), ""), (Key::Ctrl('h'), "ab c")] {
            let mut rig = Rig::at(FocusState::Input, "draft");
            rig.ctrl('p').await;
            rig.type_text("ab cd").await;
            assert_eq!(rig.key(key).await, KeyFlow::Continue);
            assert_eq!(rig.state.command_palette.filter_text.as_str(), want, "{key:?}");
            assert!(presentation::is_open(&rig.state, Surface::CommandPalette) && rig.state.input_buffer == "draft" && rig.frames().is_empty());
        }
        let mut typing = Rig::at(FocusState::Input, "draft");
        typing.ctrl('p').await;
        typing.ch('z').await;
        assert_eq!(typing.state.command_palette.filter_text.as_str(), "z");
        assert!(presentation::is_open(&typing.state, Surface::CommandPalette));
        assert_eq!(typing.state.input_buffer, "draft", "the palette filter never touches the composer");
        assert!(typing.frames().is_empty());
    }

    async fn slash_rig(key: Key) -> Rig {
        let mut rig = Rig::at(FocusState::Input, "");
        rig.ch('/').await;
        if key == Key::Up {
            rig.key(Key::Down).await; // selection 1 ("team"): Up moves back to 0
        }
        rig
    }

    #[rustfmt::skip]
    fn slash_digest(rig: &Rig) -> String {
        let a = &rig.state.autocomplete;
        format!("popup={} sel={} filter={} {}", a.is_open(), a.selected_index, a.filter_text, open_surface(rig))
    }

    /// Catches a popup row that accepts the wrong suggestion, fails to restore the composer or leaks to it.
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_autocomplete_rows() {
        let popup = |sel: u8| format!("Continue | popup=true sel={sel} filter= quit=false focus=Input input=/");
        let shut = |input: &str| format!("Continue | popup=false sel=0 filter= quit=false focus=Input input={input}");
        let cases = [
            (Key::Up, popup(0)), (Key::Down, popup(1)),
            (Key::Tab, shut("/clear")), (Key::Enter, shut("/clear")), // sends nothing: the popup consumed it
            (Key::Esc, shut("")), (Key::Ctrl('c'), shut("")),
            (Key::Backspace, shut("")), // at the trigger the `/` goes with the popup
            (Key::Ctrl('q'), "Continue | popup=true sel=0 filter= quit=true focus=Input input=/".into()),
        ];
        witness_rows(Context::Autocomplete, slash_rig, slash_digest, &cases).await;
        // Inside the filter Backspace only deletes the last filter character.
        let mut rig = slash_rig(Key::Esc).await;
        rig.ch('c').await;
        rig.key(Key::Backspace).await;
        assert_eq!(slash_digest(&rig), "popup=true sel=0 filter= quit=false focus=Input input=/");
    }

    /// Catches a rowless popup key that leaves the popup open, sends a frame or quits (they dismiss it, then act normally).
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_autocomplete_rowless_keys_dismiss_then_act_normally() {
        let rows = attached_keys(Context::Autocomplete);
        let mut bad = Vec::new();
        for key in Key::universe().into_iter().filter(|k| !rows.contains(k)) {
            let mut rig = slash_rig(key).await;
            let flow = rig.key(key).await;
            let wrong = match key {
                Key::Char(_) => false, // typed into the filter (or the composer once a space closes it)
                // Ctrl+Q is the universal quit request, answered before the popup is consulted.
                Key::Ctrl('q') => rig.state.quit_confirm.is_none(),
                _ => rig.state.autocomplete.is_open(),
            };
            // Ctrl+Enter submits the dismissed popup's composer like Ctrl+Enter anywhere.
            let sent = key != Key::CtrlEnter && !rig.frames().is_empty();
            if flow != KeyFlow::Continue || wrong || sent {
                bad.push(format!("{key:?}: {flow:?} wrong={wrong} sent={sent}"));
            }
        }
        assert!(bad.is_empty(), "rowless keys misbehaved with the popup open: {bad:#?}");
    }

    async fn log_rig(key: Key) -> Rig {
        let mut rig = Rig::at(FocusState::Input, "draft");
        notifications::info(&mut rig.state, NoticeSource::Attached, "first");
        notifications::info(&mut rig.state, NoticeSource::Attached, "second");
        rig.ctrl('x').await;
        rig.ch('n').await;
        if matches!(key, Key::Char('k') | Key::Up) {
            rig.ch('j').await; // selection on "first": k / Up move back to "second"
        }
        rig
    }

    #[rustfmt::skip]
    fn log_digest(rig: &Rig) -> String {
        use crate::adapters::tui::notifications::Presentation;
        let s = &rig.state;
        let (sel, detail, scroll) = match &s.notification_log {
            Some(log) => (log.selected_id.and_then(|id| s.notifications.get(id)).map_or("-", |r| r.message.as_str()), log.detail, log.detail_scroll),
            None => ("-", false, 0),
        };
        let archived = s.notifications.records().filter(|r| r.source == NoticeSource::Attached && r.presentation != Presentation::Visible).count();
        let clip = rig.notice(NoticeSource::Clipboard).is_some();
        format!("log={} sel={sel} detail={detail} scroll={scroll} archived={archived} records={} clip={clip} {}{}", s.notification_log.is_some(), s.notifications.len(), open_surface(rig), if s.notification_log.as_ref().is_some_and(|log| log.search.editing) { " search=active" } else { "" })
    }

    /// Catches a log row that moves the selection wrongly, copies or dismisses the wrong record, or swallows Ctrl+Q.
    // AC20 fixed: `Shift+Enter` (`Enter if plain`) and `Ctrl+Shift+C` (`Char('c') if CONTROL`) no longer alias
    // Enter / Ctrl+C in `notification_log::handle_raw_key`. `c` really copies through the host clipboard helper.
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_notification_log() {
        let on = |sel: &str, detail: bool, archived: u8, records: u8, clip: bool| format!("Continue | log=true sel={sel} detail={detail} scroll=0 archived={archived} records={records} clip={clip} quit=false focus=Input input=draft");
        let off = |quit: bool| format!("Continue | log=false sel=- detail=false scroll=0 archived=0 records=2 clip=false quit={quit} focus=Input input=draft");
        let cases = [
            (Key::Esc, off(false)), (Key::Ctrl('c'), off(false)),
            // AC2/A18: Quit is pushed without resetting the selected Log record.
            (Key::Ctrl('q'), "Continue | log=true sel=second detail=false scroll=0 archived=0 records=2 clip=false quit=true focus=Input input=draft".to_owned()),
            (Key::Char('j'), on("first", false, 0, 2, false)), (Key::Down, on("first", false, 0, 2, false)),
            (Key::Char('k'), on("second", false, 0, 2, false)), (Key::Up, on("second", false, 0, 2, false)),
            (Key::Enter, on("second", true, 0, 2, false)),
            (Key::Char('c'), on("second", false, 0, 3, true)), // the copy result is a third (Clipboard) notice
            (Key::Char('x'), on("second", false, 1, 2, false)),
            (Key::Ctrl('x'), on("second", false, 0, 2, false)),
            (Key::Ctrl('p'), on("second", false, 0, 2, false)),
            (Key::Char('/'), on("second", false, 0, 2, false) + " search=active"),
        ];
        witness_rows(Context::NotificationLog, log_rig, log_digest, &cases).await;
        // `r`, `K`, `n` are local-only rows: with no retry path and no decisions they are inert here.
        witness_inert(Context::NotificationLog, log_rig, log_digest, |_| false).await;
    }

    async fn quit_rig(_: Key) -> Rig {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('q').await;
        rig
    }

    /// Catches a stray key confirming the quit, Enter that quits instead of staying, or a rowless key leaking underneath.
    // AC20 fixed: `Shift+Enter` (`Enter if plain`) and `Ctrl+Shift+C` (`Char('c') if CONTROL`) no longer alias
    // Enter / Ctrl+C in `quit_confirm::handle_raw_key`.
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_quit_confirm() {
        let shut = |flow: &str| format!("{flow} | quit=false focus=Input input=draft");
        let cases = [
            (Key::Char('y'), shut("Quit")), (Key::Char('Y'), shut("Quit")),
            (Key::Char('n'), shut("Continue")), (Key::Char('N'), shut("Continue")), (Key::Esc, shut("Continue")),
            (Key::Ctrl('c'), shut("Continue")), (Key::Enter, shut("Continue")),
        ];
        witness_rows(Context::QuitConfirm, quit_rig, open_surface, &cases).await;
        witness_inert(Context::QuitConfirm, quit_rig, open_surface, |_| false).await;
    }

    fn thread_peer() -> crate::domain::models::PeerId {
        crate::domain::models::PeerId::from_public_key(&[91; 32]).unwrap()
    }

    /// Journal one received item (the fixture of the opener, never a hand-built slot).
    async fn seed_thread_item(
        journal: &crate::infrastructure::subagent::node_journal::NodeJournal,
        peer: &crate::domain::models::PeerId,
        id: &str,
        message_type: crate::domain::models::SemanticMessageType,
        in_reply_to: Option<&str>,
        content: &str,
        stories: Option<crate::domain::models::ReceivedStories>,
    ) -> crate::domain::models::ItemAddress {
        use crate::domain::models::{
            Authorship, ItemAddress, ItemId, RoomEvent, SendAuthorization,
        };
        let address = ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay(id));
        journal
            .append_room(RoomEvent::RecipientItemReceived {
                stories,
                selected_for: None,
                address: address.clone(),
                task: format!("task-{id}"),
                alias: Some("peer".to_owned()),
                message_type,
                content: content.to_owned(),
                in_reply_to: in_reply_to.map(str::to_owned),
                capability_hash: None,
                reply_capability_matched: false,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            })
            .await
            .expect("append received item");
        address
    }

    /// A read-only thread of one 200-line item, opened by the production opener over Chat
    /// (draft in the composer) and scrolled to row 20: no action letter is live, and the
    /// thread has no omitted page.
    async fn thread_witness_rig(_: Key) -> Rig {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::SemanticMessageType;
        let mut rig = Rig::at(FocusState::Chat, "draft");
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            rig.workspace(),
        )
        .await
        .unwrap();
        let body = (0..200)
            .map(|n| format!("line {n}\n\n"))
            .collect::<String>();
        seed_thread_item(
            &journal,
            &thread_peer(),
            "ri-witness",
            SemanticMessageType::Consultation,
            None,
            &body,
            None,
        )
        .await;
        drop(journal);
        rig.open_thread("ri-witness", ThreadAccess::ReadOnly).await;
        rig.state.clipboard_copier = Some(std::sync::Arc::new(|_| {
            crate::adapters::tui::clipboard::ClipboardResult::Osc52Success
        }));
        rig.state.thread_inspector.as_mut().unwrap().viewer.top = 20;
        rig
    }

    #[rustfmt::skip]
    fn thread_digest(rig: &Rig) -> String {
        let inspector = rig.state.thread_inspector.as_ref();
        format!(
            "thread={} top={} earlier={} later={} {}",
            presentation::is_open(&rig.state, Surface::ThreadInspector),
            inspector.map_or(0, |i| i.viewer.top),
            inspector.is_some_and(|i| i.omitted_before > 0),
            inspector.is_some_and(|i| i.omitted_after > 0),
            open_surface(rig),
        )
    }

    /// Catches a thread row that closes into the wrong focus, scrolls by the wrong step, pages
    /// a page that is not there or skips the quit question, or a key leaking below. The body
    /// (`InspectorBody`, `MarkdownBody`) rows are both-host rows resolved through the frame;
    /// `h 0 $ v l` have nothing to act on at the root of a thread.
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_thread_inspector() {
        let probe = thread_witness_rig(Key::Esc).await;
        let (rows, viewport) = probe.state.thread_inspector.as_ref().unwrap().fit.get();
        let max = rows.saturating_sub(viewport);
        assert!(viewport >= 4 && max > 20 + viewport, "the fixture scrolls: rows={rows} viewport={viewport}");
        let (half, page) = ((viewport / 2).max(1), viewport - 1);
        let at = |top: usize, thread: bool, quit: bool| format!("Continue | thread={thread} top={top} earlier=false later=false quit={quit} focus=Chat input=draft");
        let same = at(20, true, false);
        let own = [
            (Key::Esc, at(0, false, false)), (Key::Ctrl('c'), at(0, false, false)),
            (Key::Ctrl('q'), at(20, true, true)),
            (Key::Ctrl('x'), same.clone()), (Key::Ctrl('p'), same.clone()),
            // No omitted page: both pagers are consumed no-ops; read-only: no action letter is live.
            (Key::Char('p'), same.clone()), (Key::Char('l'), same.clone()),
            (Key::Char('c'), same.clone()), (Key::Char('/'), same.clone()),
            (Key::Enter, same.clone()), (Key::Tab, same.clone()), (Key::Char('o'), same.clone()),
            (Key::Char('r'), same.clone()), (Key::Char('d'), same.clone()), (Key::Char('b'), same.clone()),
            (Key::Char('a'), same.clone()), (Key::Char('y'), same.clone()), (Key::Char('s'), same.clone()),
            (Key::Char('n'), same.clone()), (Key::Char('1'), same.clone()), (Key::Char('2'), same.clone()),
            (Key::Char('3'), same.clone()), (Key::Char('u'), same.clone()), (Key::Char('x'), same.clone()),
        ];
        witness_rows(Context::ThreadInspector, thread_witness_rig, thread_digest, &own).await;
        let body = [
            (Key::Char('j'), at(21, true, false)), (Key::Down, at(21, true, false)),
            (Key::Char('k'), at(19, true, false)), (Key::Up, at(19, true, false)),
            (Key::Ctrl('d'), at(20 + half, true, false)), (Key::Ctrl('u'), at(20 - half, true, false)),
            (Key::PageDown, at(20 + page, true, false)), (Key::PageUp, at(20 - page, true, false)),
            (Key::Char('g'), at(0, true, false)), (Key::Char('G'), at(max, true, false)),
        ];
        witness_rows(Context::InspectorBody, thread_witness_rig, thread_digest, &body).await;
        let markdown = [
            (Key::Char('h'), same.clone()), (Key::Char('l'), same.clone()), (Key::Char('0'), same.clone()),
            (Key::Char('$'), same.clone()), (Key::Char('v'), same.clone()),
        ];
        witness_rows(Context::MarkdownBody, thread_witness_rig, thread_digest, &markdown).await;
        witness_inert(
            Context::ThreadInspector,
            thread_witness_rig,
            thread_digest,
            |k| matches!(
                k,
                Key::Char('j' | 'k' | 'g' | 'G' | 'h' | '0' | '$' | 'v')
                    | Key::Down | Key::Up | Key::PageDown | Key::PageUp | Key::Ctrl('d' | 'u')
            ),
        ).await;
        let mut rig = thread_witness_rig(Key::Esc).await;
        rig.ctrl('x').await;
        assert!(rig.state.pending_leader.is_some() && rig.state.thread_inspector.is_some());
        assert!(rig.frames().is_empty());
    }

    /// Catches a writer's thread action key that sends nothing or the wrong frame, and the
    /// old `c` context key that no longer has a row.
    #[tokio::test]
    async fn story21_3_attached_thread_action_key_sends_its_frame() {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::SemanticMessageType;
        let mut rig = Rig::at(FocusState::Chat, "draft");
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            rig.workspace(),
        )
        .await
        .unwrap();
        seed_thread_item(
            &journal,
            &thread_peer(),
            "ri-writer",
            SemanticMessageType::ArchitectureUpdate,
            None,
            "body",
            None,
        )
        .await;
        drop(journal);
        rig.open_thread("ri-writer", ThreadAccess::Writer).await;
        assert_eq!(rig.key(Key::Char('x')).await, KeyFlow::Continue);
        assert!(
            rig.frames().is_empty(),
            "a letter with no action sends nothing"
        );
        let copied = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let sink = copied.clone();
        rig.state.clipboard_copier = Some(std::sync::Arc::new(move |text| {
            sink.lock().push(text.to_owned());
            crate::adapters::tui::clipboard::ClipboardResult::Osc52Success
        }));
        assert_eq!(rig.key(Key::Char('c')).await, KeyFlow::Continue);
        assert!(rig.frames().is_empty(), "`c` copies without sending");
        assert_eq!(&*copied.lock(), &["body"]);
        assert_eq!(rig.key(Key::Char('u')).await, KeyFlow::Continue);
        let frames = rig.frames();
        assert!(
            matches!(frames.as_slice(), [ClientFrame::SetRecipientContext { item_id, selected: true }] if item_id == "ri-writer"),
            "{frames:?}"
        );
        assert!(
            rig.state.thread_inspector.is_some(),
            "the thread stays open while the daemon answers"
        );
    }

    /// K9 (attached twin): in Chat `q` is the guarded quit request, `i` and `?` act, an
    /// unbound printable is inert and Ctrl+U scrolls half a page up, mirroring Ctrl+D
    /// (the registry row is `Both`; it was inert here before 3-B2).
    #[tokio::test]
    async fn story21_3_attached_k9_chat_keys_act_by_value() {
        let mut rig = Rig::at(FocusState::Chat, "draft");
        assert_eq!(rig.key(Key::Char('q')).await, KeyFlow::Continue);
        assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
        assert_eq!(rig.state.input_buffer, "draft");
        let mut idle = Rig::at(FocusState::Chat, "");
        assert_eq!(idle.key(Key::Char('q')).await, KeyFlow::Quit);

        let mut rig = Rig::at(FocusState::Chat, "draft");
        rig.key(Key::Char('i')).await;
        assert_eq!(rig.state.focus, FocusState::Input);
        let mut rig = Rig::at(FocusState::Chat, "draft");
        rig.key(Key::Char('?')).await;
        assert!(presentation::is_open(&rig.state, Surface::Help));
        assert_eq!(rig.state.focus, FocusState::Chat);

        let mut rig = Rig::at(FocusState::Chat, "draft").scrollable();
        let before = salient(&rig);
        rig.key(Key::Char('z')).await;
        assert_eq!(
            salient(&rig),
            before,
            "an unbound printable is inert in Chat"
        );

        let mut rig = Rig::at(FocusState::Chat, "draft").scrollable();
        rig.state.viewport_height = 6;
        assert_eq!(rig.key(Key::Ctrl('u')).await, KeyFlow::Continue);
        assert_eq!(
            (
                rig.domain_tab.view_state.scroll_offset,
                matches!(
                    rig.domain_tab.view_state.mode,
                    crate::domain::models::AnchorMode::Following
                )
            ),
            (15, false)
        );
        assert_eq!(
            rig.state.input_buffer, "draft",
            "Ctrl+U in Chat never edits the draft"
        );
        assert_eq!(rig.key(Key::Ctrl('d')).await, KeyFlow::Continue);
        assert_eq!(
            (
                rig.domain_tab.view_state.scroll_offset,
                matches!(
                    rig.domain_tab.view_state.mode,
                    crate::domain::models::AnchorMode::Following
                )
            ),
            (12, false)
        );
        assert!(rig.frames().is_empty());
    }

    /// K9 (attached twin): Input types printables (shifted letters keep their case), the `/`
    /// trigger needs an empty composer, Alt+M toggles multi-line without typing, Ctrl+Enter
    /// sends a non-empty draft and PgUp/PgDn page the transcript in both foci.
    #[tokio::test]
    async fn story21_3_attached_k9_input_keys_act_by_value() {
        let mut rig = Rig::at(FocusState::Input, "");
        for key in [
            Key::Char('h'),
            Key::Char('I'),
            Key::Char('?'),
            Key::Char('@'),
        ] {
            rig.key(key).await;
        }
        assert_eq!(rig.state.input_buffer, "hI?@");
        assert!(!rig.state.autocomplete.is_open());

        let mut rig = Rig::at(FocusState::Input, "");
        rig.key(Key::Char('/')).await;
        assert!(rig.state.autocomplete.is_open());
        assert_eq!(rig.state.input_buffer, "/");
        let mut rig = Rig::at(FocusState::Input, "a");
        rig.key(Key::Char('/')).await;
        assert!(
            !rig.state.autocomplete.is_open(),
            "only an empty composer triggers"
        );
        assert_eq!(rig.state.input_buffer, "a/");

        let mut rig = Rig::at(FocusState::Input, "hi");
        rig.key(Key::Alt('m')).await;
        assert!(rig.state.multiline_mode);
        assert_eq!(rig.state.input_buffer, "hi", "Alt+M is not typed");

        let mut rig = Rig::at(FocusState::Input, "hi");
        assert_eq!(rig.key(Key::CtrlEnter).await, KeyFlow::Continue);
        let frames = rig.frames();
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(user_text(&frames[0]), Some("hi"));
        assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
        let mut empty = Rig::at(FocusState::Input, "");
        empty.key(Key::CtrlEnter).await;
        assert!(empty.frames().is_empty(), "an empty composer sends nothing");

        for focus in [FocusState::Input, FocusState::Chat] {
            let mut rig = Rig::at(focus.clone(), "hi").scrollable();
            rig.key(Key::PageUp).await;
            assert_eq!(
                (
                    rig.domain_tab.view_state.scroll_offset,
                    matches!(
                        rig.domain_tab.view_state.mode,
                        crate::domain::models::AnchorMode::Following
                    )
                ),
                (17, false),
                "{focus:?}"
            );
            rig.key(Key::PageDown).await;
            rig.key(Key::PageDown).await;
            assert_eq!(
                (
                    rig.domain_tab.view_state.scroll_offset,
                    matches!(
                        rig.domain_tab.view_state.mode,
                        crate::domain::models::AnchorMode::Following
                    )
                ),
                (7, false),
                "{focus:?}"
            );
            assert_eq!(rig.state.input_buffer, "hi");
        }
    }

    /// K9, amended by 21-4 AC2/A18: Ctrl+Q retains the palette and S-LOG
    /// underneath Quit; their state is restored exactly when Quit is cancelled.
    #[tokio::test]
    async fn story21_3_attached_k9_ctrl_q_in_palette_and_log_asks_to_quit() {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('p').await;
        assert!(presentation::is_open(&rig.state, Surface::CommandPalette));
        let palette = palette_digest(&rig);
        let caller = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
        assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
        assert!(presentation::is_open(&rig.state, Surface::CommandPalette));
        assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
        rig.esc().await;
        assert_eq!(palette_digest(&rig), palette);
        assert_eq!(
            rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            caller
        );
        let mut idle = Rig::at(FocusState::Input, "");
        idle.ctrl('p').await;
        assert_eq!(idle.ctrl('q').await, KeyFlow::Quit);

        let mut rig = log_rig(Key::Ctrl('q')).await;
        let log = rig.state.notification_log.clone();
        assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
        assert!(rig.state.notification_log.is_some());
        assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
        rig.ch('n').await;
        assert_eq!(rig.state.notification_log, log);

        let mut rig = log_rig(Key::Ctrl('q')).await;
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let outcome = notification_log::handle_raw_key(&mut rig.state, ctrl_q, &log_facts("c"));
        assert_eq!(outcome, LogOutcome::Quit);
        assert!(
            rig.state.notification_log.is_some(),
            "AC2/A18: requesting Quit does not close the log"
        );
    }

    #[tokio::test]
    async fn story21_6_attached_ctrl_q_over_team_retract_preserves_item() {
        let mut rig = Rig::at(FocusState::Input, "retained draft");
        rig.preview_retract(true);
        let before = (
            rig.state.decisions.items().cloned().collect::<Vec<_>>(),
            rig.state.focus.clone(),
            rig.state.input_buffer.clone(),
        );
        assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
        assert!(presentation::is_open(
            &rig.state,
            presentation::Surface::QuitConfirm
        ));
        assert!(presentation::is_open(&rig.state, Surface::Decision));
        assert_eq!(
            (
                rig.state.decisions.items().cloned().collect::<Vec<_>>(),
                rig.state.focus.clone(),
                rig.state.input_buffer.clone()
            ),
            before
        );
        rig.ch('n').await;
        assert!(presentation::is_open(&rig.state, Surface::Decision));
        assert_eq!(
            (
                rig.state.decisions.items().cloned().collect::<Vec<_>>(),
                rig.state.focus.clone(),
                rig.state.input_buffer.clone()
            ),
            before
        );
        assert_eq!(rig.team_requests.open_retract.as_deref(), Some("retract-a"));
        assert!(
            rig.frames().is_empty(),
            "Quit never decides or interrupts the card"
        );
    }

    #[tokio::test]
    async fn story21_4_attached_only_the_top_frame_receives_keys() {
        for surface in [
            Surface::Help,
            Surface::CommandPalette,
            Surface::NotificationLog,
            Surface::QuitConfirm,
        ] {
            for key in Key::universe()
                .into_iter()
                .chain((' '..='~').map(Key::Char))
            {
                let mut rig = Rig::at(FocusState::Input, "unchanged draft");
                match surface {
                    Surface::Help => {
                        rig.ctrl('x').await;
                        rig.ch('?').await;
                    }
                    Surface::CommandPalette => {
                        rig.ctrl('p').await;
                        rig.type_text("ml").await;
                    }
                    Surface::NotificationLog | Surface::QuitConfirm => {
                        rig.ctrl('x').await;
                        rig.ch('n').await;
                        if surface == Surface::QuitConfirm {
                            rig.ctrl('q').await;
                        }
                    }
                    Surface::MissionControl => {
                        rig.ctrl('x').await;
                        rig.ch('l').await;
                        assert!(presentation::is_open(&rig.state, Surface::MissionControl));
                    }
                    Surface::Decision
                    | Surface::DecisionQueue
                    | Surface::ModelSelector
                    | Surface::ProfileSwitcher
                    | Surface::UsagePanel
                    | Surface::Search
                    | Surface::BookmarkList
                    | Surface::WaveOverlay
                    | Surface::CrossSearch
                    | Surface::ToolInspector(_)
                    | Surface::BlockViewer(_)
                    | Surface::ArtifactWindow
                    | Surface::AgentWindow
                    | Surface::TaskWindow
                    | Surface::Adapters
                    | Surface::History
                    | Surface::Tour
                    | Surface::Login => unreachable!("surface not offered attached"),
                    Surface::ThreadInspector => unreachable!(
                        "the thread frame owns its keys by value: story21_8_attached_thread_frame_keys_act_by_value"
                    ),
                }
                let before = (
                    rig.state.input_buffer.clone(),
                    rig.domain_tab.view_state.scroll_offset,
                    matches!(
                        rig.domain_tab.view_state.mode,
                        crate::domain::models::AnchorMode::Following
                    ),
                );
                let flow = rig.key(key).await;
                assert_eq!(
                    (
                        rig.state.input_buffer.clone(),
                        rig.domain_tab.view_state.scroll_offset,
                        matches!(
                            rig.domain_tab.view_state.mode,
                            crate::domain::models::AnchorMode::Following
                        )
                    ),
                    before,
                    "{surface:?} key={key:?}: {flow:?}"
                );
                assert!(
                    rig.frames().is_empty(),
                    "{surface:?} key={key:?}: no card/interrupt frame"
                );
                assert!(!matches!(flow, KeyFlow::Detach));
            }
        }
        let mut base = Rig::at(FocusState::Input, "");
        base.ch('y').await;
        assert_eq!(
            base.state.input_buffer, "y",
            "positive control: base keys still reach Input"
        );
    }

    /// A20: the attached hub is Messages only. Its keys act by value: Esc closes (dropping the
    /// page), `Left`/`Right` are inert (one tab), `/` opens the filter field, `e` (export, a
    /// local row) is inert and sends nothing, and the Ctrl+X leader reaches its frame and
    /// returns to the same page.
    #[tokio::test]
    async fn story21_7_attached_mission_control_messages_keys_act_by_value() {
        use crate::adapters::tui::mission_control::McTab;
        let top = |rig: &Rig| rig.state.modal_stack.top().map(|frame| frame.surface);
        let open = || async {
            let mut rig = Rig::at(FocusState::Input, "draft");
            rig.ctrl('x').await;
            rig.ch('l').await;
            assert_eq!(top(&rig), Some(Surface::MissionControl));
            assert!(screen(&mut rig).contains("Mission Control · Messages"));
            rig
        };

        // Left/Right: one tab, so nothing moves.
        let mut rig = open().await;
        for key in [Key::Left, Key::Right] {
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{key:?}");
            assert_eq!(top(&rig), Some(Surface::MissionControl), "{key:?}");
            assert_eq!(rig.state.mission_control.tab, McTab::Messages, "{key:?}");
            assert!(rig.state.mission_control.messages.is_some(), "{key:?}");
            assert!(rig.frames().is_empty(), "{key:?}");
            assert_eq!(rig.state.input_buffer, "draft", "{key:?}");
        }

        // `e`: the export is local-only, inert attached.
        let content = rig
            .state
            .mission_control
            .messages
            .as_ref()
            .unwrap()
            .content
            .clone();
        assert_eq!(rig.ch('e').await, KeyFlow::Continue);
        assert_eq!(top(&rig), Some(Surface::MissionControl));
        assert_eq!(rig.state.mission_control.tab, McTab::Messages);
        assert_eq!(
            rig.state.mission_control.messages.as_ref().unwrap().content,
            content
        );
        assert!(rig.frames().is_empty(), "`e` sends nothing attached");
        assert_eq!(rig.state.input_buffer, "draft");

        // `/` opens the filter field; typing edits it, Esc clears and closes the field only.
        screen(&mut rig);
        assert_eq!(rig.ch('/').await, KeyFlow::Continue);
        assert!(
            rig.state
                .mission_control
                .messages
                .as_ref()
                .unwrap()
                .search
                .editing
        );
        rig.type_text("zq").await;
        assert_eq!(
            rig.state
                .mission_control
                .messages
                .as_ref()
                .unwrap()
                .search
                .query
                .as_str(),
            "zq"
        );
        assert_eq!(rig.state.input_buffer, "draft", "the field takes the text");
        rig.esc().await;
        let page = rig.state.mission_control.messages.as_ref().unwrap();
        assert!(!page.search.editing && page.search.query.as_str().is_empty());
        assert_eq!(top(&rig), Some(Surface::MissionControl));

        // The Ctrl+X leader reaches its frame; Esc returns to the same page.
        rig.ctrl('x').await;
        rig.ch('?').await;
        assert_eq!(top(&rig), Some(Surface::Help));
        rig.esc().await;
        assert_eq!(top(&rig), Some(Surface::MissionControl));
        assert_eq!(rig.state.mission_control.tab, McTab::Messages);
        assert!(rig.frames().is_empty());

        // Esc closes the hub and drops the page; the composer draft is untouched.
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(rig.state.modal_stack.is_empty());
        assert!(rig.state.mission_control.messages.is_none());
        assert_eq!(rig.state.input_buffer, "draft");
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_4_attached_quit_cancel_restores_log_and_help() {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('x').await;
        rig.ch('n').await;
        let log = rig.state.notification_log.clone();
        rig.ctrl('q').await;
        rig.ch('n').await;
        assert_eq!(
            rig.state.notification_log, log,
            "Quit must not close/reset Log"
        );
        rig.esc().await;
        rig.ctrl('x').await;
        rig.ch('?').await;
        rig.state.help_overlay.scroll_offset = 7;
        let focus = rig.state.focus.clone();
        rig.ctrl('q').await;
        rig.ch('n').await;
        assert!(presentation::is_open(&rig.state, Surface::Help));
        assert_eq!(rig.state.help_overlay.scroll_offset, 7);
        assert_eq!(rig.state.focus, focus);
        assert_eq!(rig.state.input_buffer, "draft");
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_4_attached_tiny_keys_and_paste_never_answer_or_edit() {
        let mut rig = Rig::at(FocusState::Input, "");
        rig.approval_arrives("tiny-terminal");
        rig.paint_approvals();
        rig.state.terminal_width = 59;
        for key in [Key::Char('y'), Key::Ctrl('c'), Key::Ctrl('q')] {
            assert_eq!(rig.key(key).await, KeyFlow::Continue);
            assert!(
                rig.frames().is_empty(),
                "{key:?}: no response or interrupt below minimum"
            );
        }
        super::handle_attached_paste(&mut rig.state, "hidden paste", false);
        assert!(rig.state.cursor_position <= rig.state.input_buffer.chars().count());
        assert_eq!(rig.state.decisions.undecided(), 1);
        rig.state.terminal_width = 80;
        rig.ready();
        rig.ch('y').await;
        assert!(
            matches!(
                rig.frames().as_slice(),
                [ClientFrame::ApprovalResponse { .. }]
            ),
            "positive control: the visible permission answers at 80x24"
        );
    }

    /// AC2/A18: quit cancellation preserves the complete legacy slash popup,
    /// including its rollback, so the next Esc still restores the original draft.
    #[tokio::test]
    async fn story21_4_attached_quit_cancel_restores_legacy_popup_payload() {
        for cancel in [Key::Esc, Key::Ctrl('c'), Key::Enter, Key::Char('n')] {
            let mut rig = Rig::at(FocusState::Input, "");
            rig.type_text("/te").await;
            let snapshot = |rig: &Rig| {
                let popup = &rig.state.autocomplete;
                (
                    popup.is_open(),
                    popup.kind,
                    popup.trigger_position,
                    popup.filter_text.clone(),
                    popup.suggestions.clone(),
                    popup.selected_index,
                    popup.scroll_offset,
                    popup.slash_rollback.clone(),
                )
            };
            let before = snapshot(&rig);
            assert!(before.0);
            assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
            assert!(rig.state.quit_confirm.is_some());
            assert_eq!(snapshot(&rig), before, "Quit must not dismiss the popup");
            assert_eq!(rig.key(cancel).await, KeyFlow::Continue);
            assert!(rig.state.quit_confirm.is_none());
            assert_eq!(
                snapshot(&rig),
                before,
                "{cancel:?}: exact popup restoration"
            );
            assert_eq!(rig.state.focus, FocusState::Input);
            assert_eq!(rig.state.input_buffer, "/te");
            assert!(rig.frames().is_empty());
            rig.esc().await;
            assert!(!rig.state.autocomplete.is_open());
            assert!(
                rig.state.input_buffer.is_empty(),
                "the original slash rollback still works"
            );
        }
    }

    /// A17 covers either undersized dimension and every key, with or without a
    /// migrated frame. Blocked paste leaves the existing borrowed draft in place.
    #[tokio::test]
    async fn story21_4_attached_both_tiny_dimensions_block_all_key_and_paste_ingress() {
        for (width, height) in [(59, 24), (80, 15)] {
            for surface in [None, Some('n'), Some('q')] {
                let mut rig = Rig::at(FocusState::Input, "draft");
                match surface {
                    Some('n') => {
                        rig.ctrl('x').await;
                        rig.ch('n').await;
                    }
                    Some('q') => {
                        rig.ctrl('q').await;
                    }
                    _ => {}
                }
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                let before = salient(&rig);
                let log = rig.state.notification_log.clone();
                for key in Key::universe() {
                    assert_eq!(
                        rig.key(key).await,
                        KeyFlow::Continue,
                        "{width}x{height}: {key:?}"
                    );
                    assert_eq!(salient(&rig), before, "{width}x{height}: {key:?}");
                    assert_eq!(rig.state.notification_log, log);
                    assert!(rig.frames().is_empty());
                }
                let allocation = (
                    rig.state.input_buffer.as_ptr(),
                    rig.state.input_buffer.capacity(),
                );
                super::handle_attached_paste(&mut rig.state, "hidden paste", false);
                assert_eq!(salient(&rig), before);
                assert_eq!(
                    (
                        rig.state.input_buffer.as_ptr(),
                        rig.state.input_buffer.capacity()
                    ),
                    allocation
                );
            }
        }
        let mut visible = Rig::at(FocusState::Input, "");
        super::handle_attached_paste(&mut visible.state, "visible paste", false);
        assert_eq!(
            visible.state.input_buffer, "visible paste",
            "normal-size base paste still edits"
        );
    }

    #[tokio::test]
    async fn story21_4_attached_hints_and_wheel_follow_the_frame_owner() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 2,
                modifiers: KeyModifiers::NONE,
            })
        };
        let mut rig = Rig::at(FocusState::Chat, "draft");
        let hint = status_hint(&rig.state);
        assert!(hint.is_some());
        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
        ] {
            assert_eq!(route_event(&mut rig.state, &wheel(kind)), None);
        }
        rig.ctrl('x').await;
        rig.ch('n').await;
        assert_eq!(status_hint(&rig.state), None);
        assert_eq!(status_tail(&rig.state), StatusTail::Hidden);
        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
        ] {
            assert_eq!(
                route_event(&mut rig.state, &wheel(kind)),
                Some(RouteEffect::Consumed)
            );
        }
        rig.ctrl('q').await;
        assert_eq!(status_hint(&rig.state), None);
        assert_eq!(status_tail(&rig.state), StatusTail::Hidden);
        rig.ch('n').await;
        rig.esc().await;
        assert_eq!(status_hint(&rig.state), hint);
        assert_eq!(status_tail(&rig.state), StatusTail::Menu);
        for (width, height) in [(59, 24), (80, 15)] {
            rig.state.terminal_width = width;
            rig.state.terminal_height = height;
            assert_eq!(
                route_event(&mut rig.state, &wheel(MouseEventKind::ScrollUp)),
                Some(RouteEffect::Consumed)
            );
        }
    }

    #[tokio::test]
    async fn story21_4_attached_real_compose_preserves_shell_cursor_and_fitted_rows() {
        use crate::adapters::tui::{
            layout::compute_layout,
            presentation::{Shell, geometry},
        };
        use ratatui::widgets::Paragraph;
        use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};
        let row = |buffer: &Buffer, rect: Rect, y| -> String {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for full in [false, true] {
                for route in ["log", "quit", "log-quit", "help-quit"] {
                    let draft = if full {
                        ["draft"; 8].join("\n")
                    } else {
                        String::new()
                    };
                    let mut rig = Rig::at(FocusState::Input, &draft);
                    rig.state.terminal_width = width;
                    rig.state.terminal_height = height;
                    rig.domain_tab.streaming.is_streaming = true;
                    notifications::notify(
                        &mut rig.state,
                        NoticeSource::System,
                        NoticeSeverity::Info,
                        "body-witness",
                    );
                    let draw = |rig: &Rig, title: Option<&str>| {
                        let state = &rig.state;
                        let display =
                            crate::adapters::tui::handlers::recipient_actions::pending_composition(
                                state,
                            )
                            .unwrap_or(&state.input_buffer);
                        let area = Rect::new(0, 0, width, height);
                        let layout = compute_layout(area, &state.theme, display, 1, false).unwrap();
                        assert_eq!(layout.input_area.height, if full { 10 } else { 3 });
                        assert!(layout.sidebar.is_none());
                        let shell = Shell::new(area, &layout);
                        assert_eq!(shell.region.bottom(), layout.chat_pane.bottom());
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal
                            .draw(|frame| {
                                input_box::render(
                                    frame,
                                    layout.input_area,
                                    display,
                                    state.cursor_position,
                                    if state.modal_stack.is_empty()
                                        && state.pending_leader.is_none()
                                    {
                                        state.focus.clone()
                                    } else {
                                        FocusState::Chat
                                    },
                                    &state.theme,
                                    state.multiline_mode,
                                    state.input_scroll_offset,
                                    None,
                                );
                                frame.render_widget(
                                    Paragraph::new("status-row-sentinel"),
                                    layout.status_bar,
                                );
                                presentation::compose(
                                    frame,
                                    state,
                                    &shell,
                                    Host::Attached,
                                    &log_facts(&rig.domain_tab.conversation.id),
                                    &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                                    |frame| {
                                        let buffer = frame.buffer_mut();
                                        assert!(
                                            row(buffer, layout.status_bar, layout.status_bar.y)
                                                .starts_with("status-row-sentinel")
                                        );
                                        if let Some(title) = title {
                                            let rect = geometry::frame_rect(shell.region);
                                            assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
                                            assert_eq!(
                                                buffer[(rect.right() - 1, rect.bottom() - 1)]
                                                    .symbol(),
                                                "┘"
                                            );
                                            assert!(row(buffer, rect, rect.y).contains(title));
                                            let quit = route != "log";
                                            let header = if quit {
                                                "Leave this attached client?"
                                            } else {
                                                "Notifications (1)"
                                            };
                                            let body =
                                                if quit { "Daemon work" } else { "body-witness" };
                                            let footer = if quit { "Stay" } else { "Detail" };
                                            assert!(row(buffer, rect, rect.y + 1).contains(header));
                                            assert!(row(buffer, rect, rect.y + 2).contains(body));
                                            if !quit {
                                                let sheet = (rect.y..rect.bottom())
                                                    .map(|y| row(buffer, rect, y))
                                                    .collect::<Vec<_>>()
                                                    .join("\n");
                                                for action in [
                                                    "[Enter] Detail",
                                                    "[c] Copy",
                                                    "[/] Search",
                                                    "[Esc] Close",
                                                ] {
                                                    assert!(sheet.contains(action), "{sheet}");
                                                }
                                            }
                                            let footer_y = (rect.y + 2..rect.bottom() - 1)
                                                .find(|&y| row(buffer, rect, y).contains(footer))
                                                .expect("fitted footer");
                                            if width == 60 && height == 16 && full {
                                                assert_eq!(rect, Rect::new(0, 0, 60, 5));
                                                assert_eq!(footer_y, rect.y + 3);
                                            } else {
                                                assert!(
                                                    footer_y < rect.bottom() - 2,
                                                    "footer must not bottom-anchor"
                                                );
                                            }
                                        }
                                        frame.render_widget(
                                            ratatui::widgets::Clear,
                                            layout.status_bar,
                                        );
                                        frame.render_widget(
                                            Paragraph::new("Ready/status-sentinel"),
                                            layout.status_bar,
                                        );
                                    },
                                );
                            })
                            .unwrap();
                        assert_eq!(terminal.backend().cursor_visible(), title.is_none());
                        (terminal, layout)
                    };
                    let (bare, before) = draw(&rig, None);
                    if route == "help-quit" {
                        rig.ctrl('x').await;
                        rig.ch('?').await;
                        rig.state.help_overlay.scroll_offset = 7;
                    } else if route != "quit" {
                        rig.ctrl('x').await;
                        rig.ch('n').await;
                    }
                    let legacy = (
                        rig.state.focus.clone(),
                        presentation::is_open(&rig.state, Surface::Help),
                        rig.state.help_overlay.scroll_offset,
                    );
                    let log = rig.state.notification_log.clone();
                    if route != "log" {
                        assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
                    }
                    let title = match route {
                        "log" => "System · Notifications",
                        "log-quit" => "System · Notifications › System · Quit",
                        "help-quit" => "Help — Rustain Keybindings › System · Quit",
                        _ => "System · Quit",
                    };
                    assert!(presentation::consistent(&rig.state));
                    assert_eq!(
                        (
                            rig.state.focus.clone(),
                            presentation::is_open(&rig.state, Surface::Help),
                            rig.state.help_overlay.scroll_offset
                        ),
                        legacy
                    );
                    assert_eq!(rig.state.input_buffer, draft);
                    let (painted, after) = draw(&rig, Some(title));
                    assert_eq!(
                        (before.chat_pane, before.input_area, before.status_bar),
                        (after.chat_pane, after.input_area, after.status_bar)
                    );
                    let buffer = painted.backend().buffer();
                    for x in after.status_bar.x..after.status_bar.right() {
                        assert_eq!(
                            buffer[(x, after.status_bar.y)],
                            bare.backend().buffer()[(x, before.status_bar.y)]
                        );
                    }
                    assert!(
                        row(buffer, after.status_bar, after.status_bar.y)
                            .starts_with("Ready/status-sentinel")
                    );
                    for y in after.input_area.y..after.input_area.bottom() {
                        for x in after.input_area.x..after.input_area.right() {
                            assert!(buffer[(x, y)].modifier.contains(Modifier::DIM));
                        }
                    }
                    if route != "log" {
                        rig.ch('n').await;
                    }
                    assert_eq!(rig.state.notification_log, log);
                    assert_eq!(
                        (
                            rig.state.focus.clone(),
                            presentation::is_open(&rig.state, Surface::Help),
                            rig.state.help_overlay.scroll_offset
                        ),
                        legacy
                    );
                    assert!(
                        !presentation::is_open(&rig.state, Surface::CommandPalette)
                            && rig.state.pending_leader.is_none()
                            && !rig.state.autocomplete.is_open()
                    );
                    assert_eq!(rig.state.input_buffer, draft);
                    assert!(
                        rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                            && rig.frames().is_empty()
                    );
                    assert!(presentation::consistent(&rig.state));
                }
            }
        }
    }

    /// K1: all four migrated surfaces are reached through the attached front door.
    #[tokio::test]
    async fn story21_4_attached_a2a_stack_back_truncation_and_identity() {
        for base in [FocusState::Input, FocusState::Chat] {
            let mut rig = Rig::at(base.clone(), "attached draft 界");
            rig.ctrl('p').await;
            rig.type_text("ml").await;
            let palette = palette_digest(&rig);
            let palette_id = rig.state.modal_stack.top().unwrap().id;
            rig.ctrl('x').await;
            assert_eq!(rig.state.pending_leader, Some(Context::CommandPalette));
            rig.ch('?').await;
            rig.state.help_overlay.scroll_offset = 7;
            let help_id = rig.state.modal_stack.top().unwrap().id;
            rig.ctrl('x').await;
            assert_eq!(rig.state.pending_leader, Some(Context::Help));
            rig.ch('n').await;
            let log_id = rig.state.modal_stack.top().unwrap().id;
            assert!(palette_id < help_id && help_id < log_id);
            assert_eq!(rig.state.modal_stack.len(), 3);
            let frames = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
            let log = rig.state.notification_log.clone();
            for cancel in [Key::Char('n'), Key::Esc, Key::Ctrl('c')] {
                rig.ctrl('q').await;
                assert_eq!(rig.state.modal_stack.len(), 4);
                assert_eq!(
                    rig.state.modal_stack.top().unwrap().surface,
                    Surface::QuitConfirm
                );
                assert!(rig.state.modal_stack.top().unwrap().id > log_id);
                rig.key(cancel).await;
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    frames
                );
                assert_eq!(rig.state.notification_log, log);
                assert_eq!(rig.state.help_overlay.scroll_offset, 7);
                assert!(presentation::consistent(&rig.state));
            }
            rig.ctrl('x').await;
            assert_eq!(rig.state.pending_leader, Some(Context::NotificationLog));
            rig.ch('?').await;
            assert_eq!(
                rig.state.modal_stack.len(),
                2,
                "reopen truncates, never duplicates"
            );
            assert_eq!(rig.state.modal_stack.top().unwrap().id, help_id);
            assert_eq!(rig.state.help_overlay.scroll_offset, 7);
            rig.ctrl('x').await;
            rig.ch('n').await;
            assert!(rig.state.modal_stack.top().unwrap().id > log_id);
            for depth in [2, 1, 0] {
                rig.esc().await;
                assert_eq!(rig.state.modal_stack.len(), depth);
                assert_eq!(rig.state.focus, base);
                assert_eq!(rig.state.input_buffer, "attached draft 界");
                assert!(presentation::consistent(&rig.state));
                if depth == 1 {
                    assert_eq!(
                        palette_digest(&rig),
                        palette,
                        "lent filter and selection restored"
                    );
                }
            }
            assert!(
                rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                    && rig.frames().is_empty()
            );
        }
    }

    /// K2: attached pane absence is registry feedback and never closes the caller.
    #[tokio::test]
    async fn story21_4_attached_a2a_frame_leader_gate_and_quit_retention() {
        let mut rig = Rig::at(FocusState::Input, "draft");
        rig.ctrl('p').await;
        rig.type_text("ml").await;
        let palette = palette_digest(&rig);
        rig.ctrl('x').await;
        rig.ch('?').await;
        let caller = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
        let records = rig.state.notifications.records().len();
        rig.ctrl('x').await;
        let avail = attached_avail_ctx(&rig.state, false);
        assert!(avail.frame_origin);
        assert!(matches!(
            leader_step(Host::Attached, Key::Char('q'), &avail, Context::Help.name()),
            LeaderStep::Run(_)
        ));
        let remedy = "No decisions waiting";
        assert!(!presentation::is_open(&rig.state, Surface::Decision));
        rig.ch('q').await;
        assert_eq!(rig.notice(NoticeSource::System), Some(remedy));
        assert_eq!(rig.state.notifications.records().len(), records + 1);
        assert_eq!(
            rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            caller
        );
        assert_eq!(rig.state.pending_leader, Some(Context::Help));
        let pending = rig.state.pending_leader;
        rig.ctrl('q').await;
        assert_eq!(rig.state.pending_leader, pending);
        assert_eq!(
            rig.state.modal_stack.top().unwrap().surface,
            Surface::QuitConfirm
        );
        rig.ch('?').await;
        assert_eq!(
            rig.state.pending_leader, pending,
            "Quit owns keys over pending leader"
        );
        assert_eq!(rig.state.modal_stack.len(), 3);
        rig.ch('n').await;
        assert_eq!(rig.state.pending_leader, pending);
        assert_eq!(
            rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            caller
        );
        rig.ch('n').await;
        assert_eq!(
            rig.state.modal_stack.top().unwrap().surface,
            Surface::NotificationLog
        );
        rig.ctrl('p').await;
        assert_eq!(rig.state.modal_stack.len(), 1, "palette reopen truncates");
        assert_eq!(palette_digest(&rig), palette);
        assert!(
            rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                && rig.frames().is_empty()
        );
    }

    /// K2: Help/palette/pending leader block paste and wheel without editing Stage.
    #[tokio::test]
    async fn story21_4_attached_a2a_frame_ingress_and_hint_cache() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        for surface in [Surface::Help, Surface::CommandPalette] {
            let mut rig = Rig::at(FocusState::Input, "draft").scrollable();
            rig.state.current_hint = Some("stale base hint".into());
            if surface == Surface::Help {
                rig.ctrl('x').await;
                rig.ch('?').await;
            } else {
                rig.ctrl('p').await;
            }
            assert_eq!(rig.state.current_hint, None);
            let scroll = (
                rig.domain_tab.view_state.scroll_offset,
                matches!(
                    rig.domain_tab.view_state.mode,
                    crate::domain::models::AnchorMode::Following
                ),
            );
            let allocation = (
                rig.state.input_buffer.as_ptr(),
                rig.state.input_buffer.capacity(),
            );
            for pending in [false, true] {
                if pending {
                    rig.ctrl('x').await;
                }
                let before = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
                super::handle_attached_paste(&mut rig.state, "hidden", false);
                for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
                    assert_eq!(
                        route_event(
                            &mut rig.state,
                            &Event::Mouse(MouseEvent {
                                kind,
                                column: 1,
                                row: 2,
                                modifiers: KeyModifiers::NONE,
                            })
                        ),
                        Some(RouteEffect::Consumed)
                    );
                }
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    before
                );
                assert_eq!(
                    (
                        rig.domain_tab.view_state.scroll_offset,
                        matches!(
                            rig.domain_tab.view_state.mode,
                            crate::domain::models::AnchorMode::Following
                        )
                    ),
                    scroll
                );
                assert_eq!(
                    (
                        rig.state.input_buffer.as_ptr(),
                        rig.state.input_buffer.capacity()
                    ),
                    allocation
                );
                assert_eq!(rig.state.input_buffer, "draft");
                if pending {
                    rig.esc().await;
                }
            }
            rig.ctrl('c').await;
            assert!(rig.state.modal_stack.is_empty());
            assert_eq!(
                rig.state.current_hint,
                crate::adapters::tui::hints::contextual_hint(
                    &rig.state.focus,
                    rig.state.session_count,
                    rig.state.theme.timing.status_hint_fade_sessions,
                    rig.state.is_vscode,
                    &rig.state.modal_stack,
                    None,
                    false,
                )
            );
            assert!(rig.frames().is_empty());
        }
    }
    /// K3: real host composition keeps status last and the hardware caret hidden.
    #[tokio::test]
    async fn story21_4_attached_a2a_compose_breadcrumb_leader_and_cursor_matrix() {
        use crate::adapters::tui::{
            layout::compute_layout,
            presentation::{Shell, geometry},
        };
        use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for full in [false, true] {
                for (depth, pending) in [
                    (0, true),
                    (1, false),
                    (2, false),
                    (3, false),
                    (4, false),
                    (3, true),
                    (4, true),
                ] {
                    let draft = if full {
                        ["draft"; 8].join("\n")
                    } else {
                        "draft".into()
                    };
                    let mut rig = Rig::at(FocusState::Input, &draft);
                    rig.state.terminal_width = width;
                    rig.state.terminal_height = height;
                    if depth > 0 {
                        rig.ctrl('p').await;
                    }
                    if depth > 1 {
                        rig.ctrl('x').await;
                        rig.ch('?').await;
                    }
                    if depth > 2 {
                        rig.ctrl('x').await;
                        rig.ch('n').await;
                    }
                    if pending {
                        rig.ctrl('x').await;
                    }
                    if depth > 3 {
                        rig.ctrl('q').await;
                    }
                    assert_eq!(rig.state.modal_stack.len(), depth);
                    assert_eq!(rig.state.pending_leader.is_some(), pending);
                    assert_eq!(rig.state.focus, FocusState::Input);
                    let state = &rig.state;
                    let area = Rect::new(0, 0, width, height);
                    let layout = compute_layout(area, &state.theme, &draft, 1, false).unwrap();
                    let shell = Shell::new(area, &layout);
                    let rect = geometry::frame_rect(shell.region);
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| {
                            input_box::render(
                                frame,
                                layout.input_area,
                                &draft,
                                state.cursor_position,
                                if state.modal_stack.is_empty() && state.pending_leader.is_none() {
                                    state.focus.clone()
                                } else {
                                    FocusState::Chat
                                },
                                &state.theme,
                                false,
                                state.input_scroll_offset,
                                None,
                            );
                            frame.render_widget(Paragraph::new("status-before"), layout.status_bar);
                            presentation::compose(
                                frame,
                                state,
                                &shell,
                                Host::Attached,
                                &log_facts(&rig.domain_tab.conversation.id),
                                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                                |frame| {
                                    let buffer = frame.buffer_mut();
                                    let row = |y| {
                                        (0..width)
                                            .map(|x| buffer[(x, y)].symbol())
                                            .collect::<String>()
                                    };
                                    assert!(row(layout.status_bar.y).starts_with("status-before"));
                                    if !pending && depth > 0 {
                                        let breadcrumb = state
                                            .modal_stack
                                            .iter()
                                            .map(|frame| frame.surface.title())
                                            .collect::<Vec<_>>()
                                            .join(" › ");
                                        let title = row(rect.y);
                                        if width == 130 {
                                            assert!(title.contains(&breadcrumb), "{title}");
                                        } else {
                                            assert!(
                                                title.contains(Surface::CommandPalette.title()),
                                                "{title}"
                                            );
                                        }
                                    }
                                    if pending && depth < 4 {
                                        assert!(
                                            (0..layout.status_bar.y)
                                                .any(|y| row(y).contains("Ctrl+X")),
                                            "which-key paints above frames and outside status"
                                        );
                                    }
                                    frame.render_widget(ratatui::widgets::Clear, layout.status_bar);
                                    frame.render_widget(
                                        Paragraph::new("Ready/status-after"),
                                        layout.status_bar,
                                    );
                                },
                            );
                        })
                        .unwrap();
                    assert!(
                        !terminal.backend().cursor_visible(),
                        "{width}x{height}: depth={depth} pending={pending}"
                    );
                    let buffer = terminal.backend().buffer();
                    let status = (0..width)
                        .map(|x| buffer[(x, layout.status_bar.y)].symbol())
                        .collect::<String>();
                    assert!(status.starts_with("Ready/status-after"));
                    assert_eq!(rig.state.input_buffer, draft);
                    assert!(
                        rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                            && rig.frames().is_empty()
                    );
                    assert!(presentation::consistent(&rig.state));
                }
            }
        }
    }
    #[tokio::test]
    async fn story21_6_attached_a2a_nested_palette_retract_preserves_owner() {
        use crate::adapters::tui::keymap::table::CLOSE_WINDOW_FIRST;
        for nested in [false, true] {
            let mut rig = Rig::at(FocusState::Input, "retained retract draft");
            rig.retractable("peer-response-auto-a", "alpha auto reply");
            rig.retractable("peer-response-auto-b", "beta auto reply");
            if nested {
                rig.ctrl('x').await;
                rig.ch('?').await;
                rig.state.help_overlay.scroll_offset = 7;
            }
            rig.ctrl('p').await;
            rig.type_text("retract").await;
            let target = rig
                .state
                .command_palette
                .filtered_entries
                .iter()
                .position(|entry| {
                    entry.action
                        == PaletteAction::ExecuteCommand(
                            RETRACT_AUTO.into(),
                            Some("peer-response-auto-b".into()),
                        )
                })
                .expect("real attached retract-auto catalogue row");
            for _ in 0..target {
                rig.press(KeyCode::Down, KeyModifiers::NONE).await;
            }
            let owner = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
            let palette = palette_digest(&rig);
            let records = rig.state.notifications.records().len();
            assert_eq!(rig.enter().await, KeyFlow::Continue);
            if nested {
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    owner
                );
                assert_eq!(palette_digest(&rig), palette);
                assert_eq!(rig.state.help_overlay.scroll_offset, 7);
                assert_eq!(rig.state.focus, FocusState::Input);
                assert!(!decision_queue::team_retract_pending(&rig.state));
                assert_eq!(rig.state.notifications.records().len(), records + 1);
                assert_eq!(rig.notice(NoticeSource::Palette), Some(CLOSE_WINDOW_FIRST));
            } else {
                assert!(rig.state.decisions.items().any(|item| matches!(
                    &item.subject, DecisionSubject::TeamRetract(payload)
                        if matches!(&payload.target, decision_queue::RetractTarget::AutoResponse { message_id } if message_id == "peer-response-auto-b")
                )));
                assert_eq!(rig.state.focus, FocusState::Input);
                assert_eq!(rig.state.notifications.records().len(), records);
            }
            assert_eq!(rig.state.input_buffer, "retained retract draft");
            assert!(
                rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                    && rig.frames().is_empty()
            );
        }
    }

    #[tokio::test]
    async fn story21_4_attached_a2a_frame_origin_leader_feedback_is_visible_and_retains_the_menu() {
        for width in [60, 80, 130] {
            for key in ['q', 'z'] {
                let mut rig = Rig::at(FocusState::Input, "leader draft");
                rig.state.terminal_width = width;
                rig.ctrl('p').await;
                rig.type_text("ml").await;
                rig.ctrl('x').await;
                rig.ch('?').await;
                let owner = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
                let palette = palette_digest(&rig);
                rig.ctrl('x').await;
                let records = rig.state.notifications.records().len();
                rig.ch(key).await;
                let expected = if key == 'q' {
                    "No decisions waiting".to_owned()
                } else {
                    crate::adapters::tui::keymap::resolve::no_binding_text(
                        Key::Char(key),
                        Context::Help.name(),
                    )
                };
                assert_eq!(rig.state.pending_leader, Some(Context::Help));
                assert_eq!(
                    rig.notice(if key == 'q' {
                        NoticeSource::System
                    } else {
                        NoticeSource::Chord
                    }),
                    Some(expected.as_str())
                );
                assert_eq!(rig.state.notifications.records().len(), records + 1);
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    owner
                );
                let rows = paint(&rig.state, width);
                let menu_y = rows
                    .iter()
                    .position(|row| row.contains("┌ Ctrl+X "))
                    .expect("WhichKey title");
                let painted = rows[menu_y..]
                    .join(" ")
                    .replace('│', " ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(painted.contains(&expected), "{width}: {painted}");
                for item in leader_menu(&attached_avail_ctx(&rig.state, false)) {
                    assert!(painted.contains(&item.text()), "{width}: {}", item.text());
                }
                if key == 'q' {
                    let buffer = paint_buffer(&rig.state, width);
                    let (y, row) = rows
                        .iter()
                        .enumerate()
                        .find(|(_, row)| row.contains("[Q] Decisions"))
                        .unwrap();
                    let x = row[..row.find("[Q]").unwrap()].chars().count() as u16;
                    assert!(
                        buffer[(x, y as u16)].modifier.intersects(
                            ratatui::style::Modifier::BOLD | ratatui::style::Modifier::REVERSED
                        ),
                        "pressed Decisions row must stand out"
                    );
                }
                rig.ctrl('q').await;
                assert_eq!(rig.state.pending_leader, Some(Context::Help));
                assert!(
                    !paint(&rig.state, width)
                        .join("\n")
                        .contains("[Q] Decisions")
                );
                rig.ch('n').await;
                assert_eq!(rig.state.pending_leader, Some(Context::Help));
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    owner
                );
                assert_eq!(
                    paint(&rig.state, width),
                    rows,
                    "Quit returns exact menu and feedback"
                );
                rig.esc().await;
                assert_eq!(rig.state.pending_leader, None);
                assert_eq!(palette_digest(&rig), palette);
                assert_eq!(rig.state.input_buffer, "leader draft");
                assert_eq!(rig.state.focus, FocusState::Input);
                assert!(rig.frames().is_empty());
                let mut base = Rig::at(FocusState::Input, "base draft");
                base.ctrl('x').await;
                base.ch(key).await;
                assert_eq!(
                    base.state.pending_leader, None,
                    "base-origin behavior unchanged"
                );
                assert!(base.state.modal_stack.is_empty() && base.frames().is_empty());
                assert_eq!(base.state.notifications.records().len(), 1);
                if key == 'q' {
                    assert_eq!(
                        base.notice(NoticeSource::System),
                        Some("No decisions waiting")
                    );
                } else {
                    assert_eq!(
                        base.notice(NoticeSource::Chord),
                        Some("No binding for Ctrl+X z in Input")
                    );
                }
                assert_eq!(base.state.input_buffer, "base draft");
                assert_eq!(base.state.focus, FocusState::Input);
            }
        }
    }

    #[tokio::test]
    async fn story21_6_attached_a2a_nested_palette_refusal_is_visible_without_dispatch() {
        use crate::adapters::tui::keymap::table::CLOSE_WINDOW_FIRST;
        for width in [60, 80, 130] {
            let mut rig = Rig::at(FocusState::Input, "palette draft");
            rig.state.terminal_width = width;
            rig.retractable("peer-response-auto-a", "real auto reply");
            rig.ctrl('x').await;
            rig.ch('?').await;
            rig.ctrl('p').await;
            rig.type_text("retract").await;
            let owner = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
            let palette = palette_digest(&rig);
            let records = rig.state.notifications.records().len();
            rig.enter().await;
            assert_eq!(
                rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                owner
            );
            assert_eq!(palette_digest(&rig), palette);
            assert_eq!(rig.state.notifications.records().len(), records + 1);
            assert_eq!(rig.notice(NoticeSource::Palette), Some(CLOSE_WINDOW_FIRST));
            let rows = paint(&rig.state, width);
            let target_y = rows
                .iter()
                .position(|row| row.contains("retract: real auto reply"))
                .unwrap();
            let header = rows
                .iter()
                .find(|row| row.contains("Command Palette"))
                .unwrap();
            let left = header.chars().position(|c| c == '┌').unwrap();
            let columns = header.chars().skip(left).position(|c| c == '┐').unwrap() + 1;
            // A7 permits a toast outside the centered sheet; inspect only its footer.
            let bottom = rows
                .iter()
                .position(|row| {
                    row.chars().nth(left) == Some('└')
                        && row.chars().nth(left + columns - 1) == Some('┘')
                })
                .expect("palette closing rail at its own columns");
            let footer_text = |rows: &[String]| {
                rows[target_y + 1..bottom]
                    .iter()
                    .map(|row| row.chars().skip(left).take(columns).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let footer = footer_text(&rows);
            assert!(footer.contains(CLOSE_WINDOW_FIRST), "{width}: {rows:?}");
            assert!(!decision_queue::team_retract_pending(&rig.state) && rig.frames().is_empty());
            rig.press(KeyCode::Left, KeyModifiers::NONE).await;
            assert_eq!(
                palette_digest(&rig),
                palette,
                "an inert key retains query and selection"
            );
            let cleared = footer_text(&paint(&rig.state, width));
            assert!(
                !cleared.contains("Close the open window first"),
                "{width}: {cleared}"
            );
            assert_eq!(
                rig.state.notifications.records().len(),
                records + 1,
                "clearing does not erase history"
            );
            assert_eq!(rig.state.input_buffer, "palette draft");
            assert_eq!(rig.state.focus, FocusState::Input);
            assert!(!decision_queue::team_retract_pending(&rig.state) && rig.frames().is_empty());
        }
    }

    /// Controlled selected-entry fixture: unsupported local entries are not an
    /// attached capability, but the shared nested-palette gate must still own Enter.
    #[tokio::test]
    async fn story21_4_attached_a2a_nested_palette_legacy_target_fixture() {
        for nested in [false, true] {
            let mut rig = Rig::at(FocusState::Input, "attached draft");
            if nested {
                rig.ctrl('x').await;
                rig.ch('?').await;
            }
            rig.ctrl('p').await;
            assert!(
                !rig.palette_names().iter().any(|name| name == "/login"),
                "the actual attached catalogue does not offer this local target"
            );
            // The fixture supplies registry action metadata only; dispatch still
            // traverses the production palette/lent-input/attached command doors.
            rig.state.command_palette.filtered_entries = vec![PaletteEntry {
                name: "/login".into(),
                description: "controlled local-target fixture".into(),
                shortcut: None,
                scope: PaletteScope::SlashCommand,
                action: PaletteAction::ExecuteCommand("login".into(), None),
            }];
            rig.state.command_palette.selected_index = 0;
            let before = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
            let palette = palette_digest(&rig);
            let records = rig.state.notifications.records().len();
            assert_eq!(rig.enter().await, KeyFlow::Continue);
            if nested {
                assert_eq!(
                    rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    before
                );
                assert_eq!(palette_digest(&rig), palette);
                assert_eq!(rig.state.notifications.records().len(), records + 1);
                assert!(
                    rig.notice(NoticeSource::Palette)
                        .unwrap()
                        .contains("Close the open window first")
                );
            } else {
                assert!(rig.state.modal_stack.is_empty());
                assert_eq!(
                    rig.notice(NoticeSource::Login),
                    Some(ATTACHED_SIGN_IN),
                    "base palette retains the existing attached auth refusal"
                );
            }
            assert_eq!(rig.state.input_buffer, "attached draft");
            assert!(
                rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                    && rig.frames().is_empty()
            );
        }
    }

    fn shared_query(rig: &Rig, surface: Surface) -> &presentation::text_field::TextField {
        match surface {
            Surface::CommandPalette => &rig.state.command_palette.filter_text,
            Surface::NotificationLog => &rig.state.notification_log.as_ref().unwrap().search.query,
            Surface::Help => &rig.state.help_overlay.search.query,
            _ => unreachable!("only queries offered by both hosts"),
        }
    }

    async fn open_shared_query(rig: &mut Rig, surface: Surface) {
        if surface == Surface::CommandPalette {
            rig.ctrl('p').await;
        } else {
            rig.ctrl('x').await;
            rig.ch(if surface == Surface::Help { '?' } else { 'n' })
                .await;
            rig.ch('/').await;
            assert!(match surface {
                Surface::NotificationLog =>
                    rig.state.notification_log.as_ref().unwrap().search.editing,
                Surface::Help => rig.state.help_overlay.search.editing,
                _ => unreachable!("only inspectors have this sub-context"),
            });
        }
        assert_eq!(rig.state.modal_stack.top().unwrap().surface, surface);
    }

    #[tokio::test]
    async fn story21_4_attached_inspector_paste_caret_cover_and_back_keep_the_owner() {
        use ratatui::style::Modifier;
        let carets = |rig: &Rig, width, height| {
            paint_buffer_at(&rig.state, width, height, &rig.state.input_buffer)
                .content
                .iter()
                .filter(|cell| cell.symbol() == "界" && cell.modifier.contains(Modifier::REVERSED))
                .count()
        };
        for read_only in [false, true] {
            for surface in [Surface::NotificationLog, Surface::Help] {
                for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                    let draft = ["inspector retained draft"; 8].join("\n");
                    let mut rig = Rig::at(FocusState::Input, &draft);
                    rig.read_only = read_only;
                    rig.state.terminal_width = width;
                    rig.state.terminal_height = height;
                    notifications::info(&mut rig.state, NoticeSource::Attached, "info control");
                    notifications::notify(
                        &mut rig.state,
                        NoticeSource::Attached,
                        NoticeSeverity::Error,
                        "error control",
                    );
                    let stage = (
                        rig.domain_tab.view_state.scroll_offset,
                        matches!(
                            rig.domain_tab.view_state.mode,
                            crate::domain::models::AnchorMode::Following
                        ),
                    );
                    let allocation = (
                        rig.state.input_buffer.as_ptr(),
                        rig.state.input_buffer.capacity(),
                    );
                    rig.ctrl('x').await;
                    rig.ch(if surface == Surface::Help { '?' } else { 'n' })
                        .await;
                    let owner = rig.state.modal_stack.top().unwrap().clone();
                    assert_eq!(owner.surface, surface, "actual opener is reachable");
                    assert_eq!(
                        carets(&rig, width, height),
                        0,
                        "independent no-field control"
                    );
                    rig.ch('/').await;
                    rig.chrome
                        .paste(&mut rig.state, "a界", false, rig.workspace.path())
                        .await;
                    rig.key(Key::Home).await;
                    rig.key(Key::Right).await;
                    assert_eq!(
                        carets(&rig, width, height),
                        1,
                        "{surface:?} {width}x{height} readonly={read_only}: actual pasted field caret"
                    );
                    rig.ctrl('x').await;
                    assert!(
                        rig.state.pending_leader.is_some(),
                        "cover must be reachable"
                    );
                    assert_eq!(
                        carets(&rig, width, height),
                        0,
                        "leader hides only the caret"
                    );
                    rig.esc().await;
                    assert_eq!(carets(&rig, width, height), 1, "same caret restored");
                    rig.ch('Z').await;
                    rig.ctrl('k').await;
                    rig.esc().await;
                    assert_eq!(
                        rig.state.modal_stack.top(),
                        Some(&owner),
                        "first Esc leaves field"
                    );
                    rig.esc().await;
                    assert!(
                        rig.state.modal_stack.is_empty(),
                        "second Esc backs the frame"
                    );
                    assert_eq!(rig.state.input_buffer, draft);
                    assert_eq!(
                        (
                            rig.state.input_buffer.as_ptr(),
                            rig.state.input_buffer.capacity()
                        ),
                        allocation
                    );
                    assert_eq!(
                        (
                            rig.domain_tab.view_state.scroll_offset,
                            matches!(
                                rig.domain_tab.view_state.mode,
                                crate::domain::models::AnchorMode::Following
                            )
                        ),
                        stage
                    );
                    assert!(
                        rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                            && rig.frames().is_empty()
                    );
                }
            }
        }
    }

    async fn inspector_rig(_: Key) -> Rig {
        let mut rig = Rig::at(FocusState::Input, "draft");
        open_shared_query(&mut rig, Surface::Help).await;
        rig.type_text("ab").await;
        rig.key(Key::Left).await;
        assert_eq!(shared_query(&rig, Surface::Help).cursor(), 1);
        rig
    }

    fn inspector_digest(rig: &Rig) -> String {
        let search = &rig.state.help_overlay.search;
        format!(
            "q={} cur={} editing={} palette={} leader={:?} {}",
            search.query.as_str(),
            search.query.cursor(),
            search.editing,
            presentation::is_open(&rig.state, Surface::CommandPalette),
            rig.state.pending_leader,
            open_surface(rig),
        )
    }

    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_4_attached_inspector_rows_have_effects_and_other_non_text_keys_are_inert() {
        let at = |query: &str, cursor, editing, palette, leader: &str, quit| format!(
            "Continue | q={query} cur={cursor} editing={editing} palette={palette} leader={leader} quit={quit} focus=Input input=draft"
        );
        let cases = [
            (Key::Enter, at("ab", 1, false, false, "None", false)),
            (Key::Esc, at("", 0, false, false, "None", false)),
            (Key::Ctrl('c'), at("", 0, false, false, "None", false)),
            (Key::Ctrl('q'), at("ab", 1, true, false, "None", true)),
            (Key::Ctrl('p'), at("ab", 1, true, true, "None", false)),
            (Key::Ctrl('x'), at("ab", 1, true, false, "Some(InspectorSearch)", false)),
        ];
        witness_rows(Context::InspectorSearch, inspector_rig, inspector_digest, &cases).await;
        witness_inert(Context::InspectorSearch, inspector_rig, inspector_digest, |key| {
            matches!(key, Key::Char(' '..='~')) ||
                matches!(keymap::resolve(Context::InspectorSearch, Host::Attached, &[key]), Resolved::Hit(row)
                    if matches!(row.command, CommandId::Field(_)))
        }).await;
    }

    #[tokio::test]
    async fn story21_4_attached_accepted_inspector_filters_retain_log_id_and_separate_help_scroll()
    {
        for read_only in [false, true] {
            for surface in [Surface::NotificationLog, Surface::Help] {
                let draft = ["retained inspector draft"; 8].join("\n");
                let mut rig = Rig::at(FocusState::Input, &draft);
                rig.read_only = read_only;
                rig.state.terminal_width = 60;
                rig.state.terminal_height = 16;
                notifications::notify(
                    &mut rig.state,
                    NoticeSource::Attached,
                    NoticeSeverity::Error,
                    "retained failure",
                );
                let retained = rig.state.notifications.records().next_back().unwrap().id;
                notifications::info(&mut rig.state, NoticeSource::Attached, "ordinary arrival");
                rig.ctrl('x').await;
                rig.ch(if surface == Surface::Help { '?' } else { 'n' })
                    .await;
                for _ in 0..if surface == Surface::Help { 3 } else { 1 } {
                    rig.ch('j').await;
                }
                if surface == Surface::Help {
                    rig.state.help_overlay.selected_category = 2;
                }
                let original = paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer);
                if surface == Surface::NotificationLog {
                    assert_eq!(
                        rig.state.notification_log.as_ref().unwrap().selected_id,
                        Some(retained)
                    );
                    let visible: String =
                        original.content.iter().map(|cell| cell.symbol()).collect();
                    assert!(
                        visible.contains("retained failure"),
                        "selected non-newest record must be the fitted visible row: {visible}"
                    );
                    assert!(
                        !visible.contains("ordinary arrival"),
                        "one available body row must not show the unselected newest record"
                    );
                }
                let stage = (
                    rig.domain_tab.view_state.scroll_offset,
                    matches!(
                        rig.domain_tab.view_state.mode,
                        crate::domain::models::AnchorMode::Following
                    ),
                );
                let owner = rig.state.modal_stack.top().unwrap().clone();
                rig.ch('/').await;
                let needle = if surface == Surface::Help {
                    "scroll"
                } else {
                    "eRr"
                };
                rig.chrome
                    .paste(&mut rig.state, needle, false, rig.workspace.path())
                    .await;
                rig.enter().await;
                if surface == Surface::Help {
                    assert!(!rig.state.help_overlay.search.editing);
                    let filtered = paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer);
                    rig.ch('j').await;
                    assert_eq!(rig.state.help_overlay.filtered_scroll_offset, 1);
                    assert_ne!(
                        paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer),
                        filtered
                    );
                    for up in [Key::Char('k'), Key::Up] {
                        rig.ch('G').await;
                        let bottom = paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer);
                        let end = rig.state.help_overlay.filtered_scroll_offset;
                        assert_ne!(bottom, filtered, "end jump changes actual attached rows");
                        rig.key(up).await;
                        assert_ne!(
                            paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer),
                            bottom,
                            "read_only={read_only} {up:?} moves up after the filtered end jump"
                        );
                        assert_eq!(rig.state.help_overlay.filtered_scroll_offset, end - 1);
                        rig.ch('G').await;
                        for _ in 0..8 {
                            rig.ch('j').await;
                        }
                        assert_eq!(rig.state.help_overlay.filtered_scroll_offset, end);
                        assert_eq!(
                            paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer),
                            bottom
                        );
                    }
                    rig.ch('g').await;
                    rig.ch('j').await;
                    assert_eq!(rig.state.help_overlay.scroll_offset, 3);
                    assert_eq!(rig.state.help_overlay.selected_category, 2);
                    rig.ch('/').await;
                    rig.key(Key::Home).await;
                    rig.key(Key::Right).await;
                    assert_eq!(rig.state.help_overlay.filtered_scroll_offset, 1);
                    rig.ch('x').await;
                    assert_eq!(rig.state.help_overlay.filtered_scroll_offset, 0);
                } else {
                    assert_eq!(
                        rig.state.notification_log.as_ref().unwrap().selected_id,
                        Some(retained)
                    );
                    rig.ch('/').await;
                    rig.key(Key::Home).await;
                    rig.ctrl('k').await;
                    rig.chrome
                        .paste(&mut rig.state, "no-match", false, rig.workspace.path())
                        .await;
                    let empty = paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer);
                    let text: String = empty.content.iter().map(|cell| cell.symbol()).collect();
                    assert!(text.contains("No matching notifications."));
                    assert!(
                        !text.contains("retained failure") && !text.contains("ordinary arrival")
                    );
                    rig.enter().await;
                    for key in ['j', 'k'] {
                        rig.ch(key).await;
                    }
                    assert_eq!(
                        rig.state.notification_log.as_ref().unwrap().selected_id,
                        Some(retained)
                    );
                    rig.ch('/').await;
                }
                rig.esc().await;
                assert_eq!(rig.state.modal_stack.top(), Some(&owner));
                assert_eq!(
                    paint_buffer_at(&rig.state, 60, 16, &rig.state.input_buffer),
                    original
                );
                assert_eq!(
                    (
                        rig.domain_tab.view_state.scroll_offset,
                        matches!(
                            rig.domain_tab.view_state.mode,
                            crate::domain::models::AnchorMode::Following
                        )
                    ),
                    stage
                );
                assert_eq!(rig.state.input_buffer, draft);
                assert!(
                    rig.state.cursor_position <= rig.state.input_buffer.chars().count()
                        && rig.frames().is_empty()
                );
            }
        }
    }

    #[tokio::test]
    async fn story21_6_attached_review_preserves_body_rows_in_frame_at_all_viewports() {
        use crate::adapters::tui::decision_queue::{DecisionKind, DecisionSubject};
        use crate::domain::models::SemanticMessageType;

        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for body_len in [12, 24] {
                let mut rig = Rig::new(false);
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                rig.paint();
                let body: Vec<_> = (0..body_len)
                    .map(|row| {
                        if matches!(row, 8 | 9) {
                            "native-review-repeat".to_owned()
                        } else {
                            format!("native-review-{row:02}")
                        }
                    })
                    .collect();
                let conversation = rig.domain_tab.conversation.id.clone();
                super::super::apply_attached_proposal_frame(
                    &mut rig.state,
                    false,
                    &mut rig.team_requests,
                    &conversation,
                    DaemonFrame::TeamProposal {
                        proposal: "native-review".into(),
                        recipients: vec!["lena-po".into(), "jun-dev".into()],
                        message_type: SemanticMessageType::ArchitectureUpdate,
                        body: body.join("\n"),
                    },
                );
                rig.ready();
                rig.ch('r').await;
                assert!(rig.frames().is_empty(), "Review must not send or decide");
                let item = rig
                    .state
                    .decisions
                    .items()
                    .find(|item| item.kind == DecisionKind::TeamProposal)
                    .unwrap();
                assert!(
                    matches!(&item.subject, DecisionSubject::TeamProposal(payload) if payload.reviewing)
                );
                let lines = crate::adapters::tui::widgets::decision_frame::body_lines_for_test(
                    item,
                    &rig.state,
                    usize::from(width - 4),
                );
                let rows: Vec<String> = lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect()
                    })
                    .collect();
                let actual: Vec<_> = rows
                    .iter()
                    .filter(|row| row.starts_with("│ native-review-"))
                    .cloned()
                    .collect();
                let expected: Vec<_> = body.iter().map(|row| format!("│ {row}")).collect();
                assert_eq!(
                    actual, expected,
                    "{width}x{height}: Review rows must remain ordered and uncapped"
                );
                let mut pages = Vec::new();
                for _ in 0..=20 {
                    pages.push(screen(&mut rig));
                    rig.key(Key::PageDown).await;
                }
                for row in &body {
                    assert!(
                        pages.iter().any(|page| page.contains(row)),
                        "{width}x{height}: Review row {row} is inaccessible"
                    );
                }
                assert!(rig.frames().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn story21_4_r4_pending_composition_keeps_the_real_tail_and_caret() {
        use crate::adapters::daemon::protocol::{
            ProtocolError, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
        };
        fn screen(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
            let buffer = terminal.backend().buffer();
            buffer.content.iter().map(|cell| cell.symbol()).collect()
        }
        for width in [60, 130] {
            for refused in [false, true] {
                let mut rig = Rig::new(false);
                rig.state.terminal_width = width;
                let reply = (0..12)
                    .map(|n| format!("row-{n:02} 界e\u{301} 👩‍💻 tail"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let draft = format!("/team reply ri-r4 {reply}");
                rig.paste(&draft).await;
                let mut before = rig.paint();
                let cursor = before.get_cursor_position().unwrap();
                let original = screen(&before);
                assert!(
                    original.contains("row-11") && !original.contains("row-00"),
                    "{original}"
                );
                assert_eq!(cursor.y, rig.state.terminal_height - 2);
                assert!(
                    cursor.x > 10,
                    "wide/grapheme end caret is genuinely reached"
                );
                assert_eq!(rig.enter().await, KeyFlow::Continue);
                assert!(matches!(rig.frames().as_slice(),
                    [ClientFrame::ReplyToRecipientItem { item_id, text, .. }]
                    if item_id == "ri-r4" && text == &reply));
                assert!(rig.state.input_buffer.is_empty());
                assert_eq!(
                    rig.state.cursor_position, 0,
                    "the editable buffer relinquished ownership"
                );
                let mut pending = rig.paint();
                let painted = screen(&pending);
                assert!(
                    painted.contains("row-11") && !painted.contains("row-00"),
                    "{painted}"
                );
                assert_eq!(
                    pending.get_cursor_position().unwrap(),
                    cursor,
                    "pending text owns the display caret"
                );
                rig.ch('x').await;
                assert!(
                    rig.frames().is_empty() && rig.state.input_buffer.is_empty(),
                    "pending edits do not submit or alter the display owner"
                );
                crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                    &mut rig.state,
                    RecipientAction::Reply,
                    RecipientActionTarget::Item {
                        item_id: "ri-r4".into(),
                    },
                    if refused {
                        RecipientActionOutcome::Refused {
                            error: ProtocolError::ReadOnly,
                        }
                    } else {
                        RecipientActionOutcome::Applied { reply: None }
                    },
                );
                let mut settled = rig.paint();
                if refused {
                    assert_eq!(rig.state.input_buffer, draft);
                    assert!(screen(&settled).contains("row-11"));
                    assert_eq!(settled.get_cursor_position().unwrap(), cursor);
                } else {
                    assert!(rig.state.input_buffer.is_empty());
                    assert!(!screen(&settled).contains("row-11"));
                    assert_eq!(settled.get_cursor_position().unwrap().x, 1);
                    rig.enter().await;
                    assert!(
                        rig.frames().is_empty(),
                        "settled empty composer cannot repeat the operation"
                    );
                }
            }
        }
        let mut ordinary = Rig::at(FocusState::Input, "ordinary 界e\u{301} tail");
        ordinary.state.cursor_position = 1;
        assert_eq!(
            ordinary.paint().get_cursor_position().unwrap().x,
            2,
            "ordinary editable drafts retain their non-end caret"
        );
    }

    #[tokio::test]
    async fn story21_4_r5_implicit_follow_retains_navigation_intent() {
        use crate::adapters::tui::keymap::Key;
        use crate::domain::models::AnchorMode;
        fn screen(rig: &mut Rig) -> String {
            let terminal = rig.paint();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }
        let mut reached = 0;
        for command in ["/team log", "/team status", "r5 model"] {
            for streaming in [false, true] {
                for overscroll in [false, true] {
                    for half in [false, true] {
                        let mut rig = Rig::new(false);
                        for row in 0..40 {
                            rig.domain_tab
                                .conversation
                                .messages
                                .push(system_message(format!("r5-prior-{row:02}")));
                        }
                        rig.domain_tab.streaming.is_streaming = streaming;
                        screen(&mut rig);
                        for _ in 0..if overscroll { 40 } else { 2 } {
                            rig.key(Key::PageUp).await;
                            screen(&mut rig);
                        }
                        let retained = rig.domain_tab.view_state.scroll_offset;
                        assert_eq!(retained, if overscroll { 200 } else { 10 });
                        if overscroll {
                            assert!(
                                retained
                                    > rig
                                        .state
                                        .total_content_height
                                        .saturating_sub(rig.state.viewport_height as usize)
                            );
                        }
                        let earlier = screen(&mut rig);
                        assert!(!earlier.contains("r5-prior-39"), "{earlier}");
                        rig.paste(command).await;
                        rig.enter().await;
                        let frames = rig.frames();
                        if command == "/team log" {
                            assert!(frames.is_empty(), "local log never sends");
                            assert_eq!(
                                rig.state.modal_stack.top().unwrap().surface,
                                Surface::MissionControl
                            );
                            assert!(screen(&mut rig).contains("Mission Control · Messages"));
                            rig.esc().await;
                        } else if command == "r5 model" {
                            assert_eq!(
                                frames.iter().filter_map(user_text).collect::<Vec<_>>(),
                                ["r5 model"]
                            );
                        } else {
                            assert_eq!(frames.len(), 1, "the command actually reaches FrameSent");
                            assert!(user_text(&frames[0]).is_none());
                        }
                        let followed = screen(&mut rig);
                        assert_eq!(rig.domain_tab.view_state.mode, AnchorMode::Following);
                        assert!(followed.contains("r5-prior-39"), "{followed}");
                        assert_eq!(
                            rig.domain_tab.view_state.scroll_offset, retained,
                            "{command}: implicit resume is not an explicit Bottom jump"
                        );
                        if half {
                            rig.esc().await;
                            assert_eq!(rig.state.focus, FocusState::Chat);
                        }
                        let rows = rig.state.viewport_height / if half { 2 } else { 1 };
                        let stride = rows
                            .saturating_sub(u16::from(rig.domain_tab.streaming.is_streaming))
                            .clamp(1, 5) as usize;
                        rig.key(if half { Key::Ctrl('u') } else { Key::PageUp })
                            .await;
                        assert_eq!(rig.domain_tab.view_state.scroll_offset, retained + stride);
                        assert_eq!(rig.domain_tab.view_state.mode, AnchorMode::Reading);
                        let resumed = screen(&mut rig);
                        assert!(!resumed.contains("r5-prior-39"), "{resumed}");
                        crate::adapters::tui::handlers::scroll::dispatch_scroll(
                            &mut rig.domain_tab.view_state,
                            &mut rig.state,
                            crate::domain::models::view_state::ScrollDelta::Bottom,
                        );
                        assert_eq!(
                            rig.domain_tab.view_state.scroll_offset, 0,
                            "independent explicit reset control"
                        );
                        assert_eq!(rig.domain_tab.view_state.mode, AnchorMode::Following);
                        reached += 1;
                    }
                }
            }
        }
        assert_eq!(
            reached, 24,
            "all three production paths and control combinations are reached"
        );
    }

    // ── Story 21.8 (slice 8-A): the attached halves of the S-THREAD keystones ────────────
    // Real key events through `handle_attached_key`, a real frame receiver, the production
    // opener over a scratch journal, and cells painted through `paint_attached`.

    #[cfg(all(unix, feature = "a2a"))]
    mod story21_8_thread {
        use super::*;
        use crate::adapters::a2a::exec::{SubmitterKey, mint_inbound_node_id};
        use crate::adapters::daemon::protocol::{
            RecipientAction, RecipientActionOutcome, RecipientActionTarget,
        };
        use crate::adapters::tui::notifications::NoticeSeverity;
        use crate::adapters::tui::state::{ThreadAccess, ThreadTarget};
        use crate::domain::models::{
            ChatMessage, PeerRowControl, PeerRowStage, ReceivedStories, ReceivedStory, RoomEvent,
            SemanticMessageType, StoryHolder, StoryKey,
        };
        use crate::infrastructure::subagent::node_journal::NodeJournal;
        use crossterm::event::KeyEventKind;

        const WRITER_KEYS: [char; 11] = ['r', 'd', 'b', 'a', 'y', 's', 'n', '1', '2', '3', 'u'];

        async fn journal_of(rig: &Rig) -> NodeJournal {
            NodeJournal::open_workspace(rig.workspace())
                .await
                .expect("scratch journal")
        }

        /// One journal-backed received item `ri-1`, opened through the production opener
        /// over Chat. `with_node` pins a settled response row whose node answers the item,
        /// so the Busy key takes its durable join.
        async fn item_rig(
            read_only: bool,
            message_type: SemanticMessageType,
            stories: Option<ReceivedStories>,
            acknowledged: bool,
            with_node: bool,
        ) -> Rig {
            let mut rig = Rig::new(read_only);
            rig.state.focus = FocusState::Chat;
            let submitter = SubmitterKey::loopback();
            let journal = journal_of(&rig).await;
            let address = seed_thread_item(
                &journal,
                &submitter.pseudonymous_peer_id(),
                "ri-1",
                message_type,
                None,
                "please look at this",
                stories,
            )
            .await;
            if acknowledged {
                journal
                    .append_room(RoomEvent::RecipientItemAcknowledged {
                        address: address.clone(),
                        alias: None,
                    })
                    .await
                    .expect("append acknowledgement");
            }
            drop(journal);
            if with_node {
                rig.domain_tab.conversation.messages.push(ChatMessage {
                    id: "row".to_owned(),
                    content: "settled response".to_owned(),
                    ..Default::default()
                });
                rig.domain_tab.conversation.peer_row_controls.insert(
                    "row".to_owned(),
                    PeerRowControl {
                        node_id: mint_inbound_node_id(&submitter, "task-ri-1")
                            .as_str()
                            .to_owned(),
                        item_address: Some(address),
                        stories: None,
                        status: None,
                        retro: None,
                        selected_for: None,
                        message_type,
                        sender_label: "peer".to_owned(),
                        stage: PeerRowStage::Settled,
                        node_resolvable: false,
                    },
                );
            }
            let access = if read_only {
                ThreadAccess::ReadOnly
            } else {
                ThreadAccess::Writer
            };
            rig.open_thread("ri-1", access).await;
            rig
        }

        async fn retract_reply_rig(read_only: bool) -> Rig {
            let mut rig = item_rig(
                read_only,
                SemanticMessageType::Consultation,
                None,
                false,
                true,
            )
            .await;
            rig.state.terminal_width = 130;
            rig.state.terminal_height = 30;
            let template = rig.domain_tab.conversation.peer_row_controls["row"].clone();
            for (node, timestamp) in [("old", 9 * 3600), ("new", 10 * 3600 + 42 * 60)] {
                let id = format!("peer-response-{node}");
                let mut row = system_message(format!("[auto-sent] {node} reply"));
                row.id = id.clone();
                row.created_at = timestamp;
                row.send_authorization = SendAuthorization::Automatic;
                rig.domain_tab.conversation.messages.push(row);
                let mut control = template.clone();
                control.node_id = node.to_owned();
                rig.domain_tab
                    .conversation
                    .peer_row_controls
                    .insert(id, control);
            }
            rig
        }

        fn assert_no_retract_decision(rig: &Rig) {
            assert!(!rig.state.decisions.holds(DecisionKind::TeamRetract));
        }

        fn open_sent_retract(rig: &mut Rig, projection: Option<&str>) {
            let generation = crate::infrastructure::runtime::thread_open::open_sent(
                &mut rig.state,
                ThreadAccess::Writer,
                "peer-b",
                "ri-sent",
                "origin",
            );
            if let Some(projection) = projection {
                assert!(crate::infrastructure::runtime::thread_open::fill_sent(
                    &mut rig.state,
                    "origin",
                    generation,
                    projection,
                ));
            }
        }

        #[tokio::test]
        async fn story21_8_attached_retract_reply_newest_decision_and_outcome() {
            let mut rig = retract_reply_rig(false).await;
            let painted = screen(&mut rig);
            assert!(
                painted.contains("[x] Retract auto-sent reply 10:42"),
                "{painted}"
            );
            assert!(
                !painted.contains("[x] Retract auto-sent reply 09:00"),
                "{painted}"
            );
            rig.ch('x').await;
            assert_eq!(top_surface(&rig), Some(Surface::Decision));
            assert!(rig.state.decisions.items().any(|item| matches!(
                &item.subject, DecisionSubject::TeamRetract(payload)
                    if matches!(&payload.target, decision_queue::RetractTarget::AutoResponse { message_id }
                        if message_id == "peer-response-new")
            )));
            assert!(
                rig.frames().is_empty(),
                "raising the decision sends nothing"
            );
            rig.ready();
            rig.ch('y').await;
            assert!(matches!(rig.frames().as_slice(),
                [ClientFrame::RetractAutoResponse { message_id, target_seq: None }]
                    if message_id == "peer-response-new"));
            let outcomes: Vec<_> = rig.state.thread_outcomes.lines("ri-1").collect();
            assert_eq!(outcomes.len(), 1);
            assert!(outcomes[0].contains("Retracted. Marked in your log — never deleted."));
            assert!(rig.feedback("peer-retract").is_none());
            let painted = screen(&mut rig);
            assert!(
                painted
                    .split_whitespace()
                    .collect::<String>()
                    .contains("Retracted.Markedinyourlog—neverdeleted."),
                "{painted}"
            );
            rig.ch('j').await;
            assert_eq!(rig.state.thread_outcomes.lines("ri-1").count(), 1);
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_retract_node_matched_reply_records_item_outcome_without_control()
         {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, true).await;
            rig.state.terminal_width = 130;
            rig.state.terminal_height = 30;
            let node = rig
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .node_id
                .clone()
                .unwrap();
            let id = format!("peer-response-{node}");
            rig.retractable(&id, "node-matched reply");
            assert!(
                !rig.domain_tab
                    .conversation
                    .peer_row_controls
                    .contains_key(&id)
            );
            assert!(screen(&mut rig).contains("[x] Retract auto-sent reply"));
            rig.ch('x').await;
            assert!(rig.state.decisions.items().any(|item| matches!(
                &item.subject, DecisionSubject::TeamRetract(payload)
                    if matches!(&payload.target, decision_queue::RetractTarget::AutoResponse { message_id }
                        if message_id == &id)
            )));
            assert!(rig.frames().is_empty());
            rig.ready();
            rig.ch('y').await;
            assert!(matches!(rig.frames().as_slice(),
                [ClientFrame::RetractAutoResponse { message_id, .. }] if message_id == &id));
            assert!(
                rig.state
                    .thread_outcomes
                    .lines("ri-1")
                    .any(|line| line.contains("Retracted. Marked in your log — never deleted."))
            );
            let painted = screen(&mut rig);
            assert!(
                painted
                    .split_whitespace()
                    .collect::<String>()
                    .contains("Retracted.Markedinyourlog—neverdeleted."),
                "{painted}"
            );
        }

        #[tokio::test]
        async fn story21_8_attached_retract_reply_stale_at_confirm_sends_nothing() {
            let mut rig = retract_reply_rig(false).await;
            assert!(screen(&mut rig).contains("[x] Retract auto-sent reply 10:42"));
            rig.ch('x').await;
            assert!(decision_queue::team_retract_pending(&rig.state));
            assert!(rig.frames().is_empty());
            rig.domain_tab
                .conversation
                .messages
                .retain(|row| row.id != "peer-response-new");
            rig.ready();
            rig.ch('y').await;
            assert!(rig.frames().is_empty());
            assert_eq!(
                rig.notice(NoticeSource::Attached),
                Some("that message can no longer be retracted — nothing sent")
            );
            assert_eq!(rig.state.thread_outcomes.lines("ri-1").count(), 0);
        }

        #[tokio::test]
        async fn story21_8_attached_retract_reply_badge_human_unsettled_and_read_only_are_inert() {
            let mut control = retract_reply_rig(false).await;
            assert!(screen(&mut control).contains("[x] Retract auto-sent reply 10:42"));
            control.ch('x').await;
            assert!(decision_queue::team_retract_pending(&control.state));
            assert!(control.frames().is_empty());
            for case in ["badge", "human", "unsettled", "read-only"] {
                let mut rig = retract_reply_rig(case == "read-only").await;
                if case != "read-only" {
                    rig.domain_tab
                        .conversation
                        .messages
                        .retain(|row| row.id != "peer-response-old");
                    match case {
                        "badge" | "human" => {
                            let row = rig
                                .domain_tab
                                .conversation
                                .messages
                                .iter_mut()
                                .find(|row| row.id == "peer-response-new")
                                .unwrap();
                            row.send_authorization = if case == "badge" {
                                SendAuthorization::Unknown
                            } else {
                                SendAuthorization::HumanDecided
                            };
                        }
                        "unsettled" => {
                            rig.domain_tab
                                .conversation
                                .peer_row_controls
                                .get_mut("peer-response-new")
                                .unwrap()
                                .stage = PeerRowStage::Ready
                        }
                        _ => unreachable!(),
                    }
                }
                let painted = screen(&mut rig);
                assert!(!painted.contains("[x] Retract"), "{case}: {painted}");
                rig.ch('x').await;
                assert_no_retract_decision(&rig);
                assert!(rig.frames().is_empty(), "{case}");
            }
        }

        #[tokio::test]
        async fn story21_8_attached_sent_retract_preview_requires_own_unretracted_completed_turn() {
            let mut rig = Rig::new(false);
            rig.state.terminal_width = 130;
            open_sent_retract(&mut rig, Some("  you  ri-sent\n      body"));
            let painted = screen(&mut rig);
            assert!(painted.contains("[x] Retract"), "{painted}");
            rig.ch('x').await;
            assert!(matches!(rig.frames().as_slice(),
                [ClientFrame::TeamRetractPreview { peer, item_id, .. }]
                    if peer == "peer-b" && item_id == "ri-sent"));
            assert_no_retract_decision(&rig);
            assert!(
                rig.state
                    .thread_outcomes
                    .lines("sent:peer-b:ri-sent")
                    .next()
                    .is_none()
            );
            for projection in [
                None,
                Some("  another sender  ri-sent\n      body"),
                Some("  you  ri-sent [retracted 10:42]\n      body"),
            ] {
                let mut rig = Rig::new(false);
                rig.state.terminal_width = 130;
                open_sent_retract(&mut rig, projection);
                let painted = screen(&mut rig);
                assert!(
                    !painted.contains("[x] Retract"),
                    "{projection:?}: {painted}"
                );
                rig.ch('x').await;
                assert_no_retract_decision(&rig);
                assert!(rig.frames().is_empty(), "{projection:?}");
            }
        }

        #[tokio::test]
        async fn story21_8_attached_retract_raw_guard_and_floor_rail_truth() {
            let mut rig = retract_reply_rig(false).await;
            assert!(screen(&mut rig).contains("[x] Retract auto-sent reply 10:42"));
            for event in [
                KeyEvent::new_with_kind(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                KeyEvent::new_with_kind(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SHIFT),
                KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
            ] {
                assert_eq!(rig.raw_key(event).await, KeyFlow::Continue);
                assert_no_retract_decision(&rig);
                assert!(rig.frames().is_empty());
            }
            rig.ch('x').await;
            assert!(decision_queue::team_retract_pending(&rig.state));
            assert!(rig.frames().is_empty());
            let mut floor = retract_reply_rig(false).await;
            floor.state.terminal_width = 60;
            floor.state.terminal_height = 16;
            set_composer(&mut floor, &full_composer());
            let painted = screen(&mut floor);
            assert!(!painted.contains("[x] Retract"), "{painted}");
            floor.ch('x').await;
            assert_no_retract_decision(&floor);
            assert!(floor.frames().is_empty());
        }

        /// A thread over the response bound: a root, 128 chained 8 KiB replies and the
        /// opened item, so an earlier page exists and the newest page has no later one. A
        /// queued reply decision gives `Ctrl+X q` a queue to open. Chat focus, `draft`
        /// in the composer, writer access.
        async fn long_thread_rig(draft: &str) -> Rig {
            let mut rig = Rig::at(FocusState::Chat, draft);
            let peer = thread_peer();
            let journal = journal_of(&rig).await;
            seed_thread_item(
                &journal,
                &peer,
                "t-root",
                SemanticMessageType::Consultation,
                None,
                "root body",
                None,
            )
            .await;
            let mut previous = "t-root".to_owned();
            for index in 0..128 {
                let id = format!("t-{index}");
                seed_thread_item(
                    &journal,
                    &peer,
                    &id,
                    SemanticMessageType::Consultation,
                    Some(&previous),
                    &"x".repeat(8 * 1024),
                    None,
                )
                .await;
                previous = id;
            }
            seed_thread_item(
                &journal,
                &peer,
                "t-open",
                SemanticMessageType::Consultation,
                Some(&previous),
                "opened body",
                None,
            )
            .await;
            drop(journal);
            decision_queue::reply::observe(&mut rig.state, "principal", "roster", "ri_1", 1);
            rig.open_thread("t-open", ThreadAccess::Writer).await;
            rig
        }

        fn thread_frames(rig: &Rig) -> usize {
            rig.state
                .modal_stack
                .iter()
                .filter(|frame| frame.surface == Surface::ThreadInspector)
                .count()
        }

        fn top_surface(rig: &Rig) -> Option<Surface> {
            rig.state.modal_stack.top().map(|frame| frame.surface)
        }

        fn viewer_top(rig: &Rig) -> usize {
            rig.state
                .thread_inspector
                .as_ref()
                .expect("thread open")
                .viewer
                .top
        }

        /// Everything a key that must not touch the page leaves alone.
        fn page_state(rig: &Rig) -> String {
            let i = rig.state.thread_inspector.as_ref().expect("thread open");
            let address = i.item_address().unwrap();
            format!(
                "{:?}",
                (
                    i.viewer.top,
                    i.omitted_before,
                    i.omitted_after,
                    i.first_non_pinned_ordinal,
                    i.last_non_pinned_ordinal,
                    &i.root_id,
                    address.item().as_str(),
                    rig.state
                        .thread_outcomes
                        .lines(address.item().as_str())
                        .count(),
                    &rig.state.input_buffer,
                    &rig.state.focus,
                )
            )
        }

        fn notice_with(rig: &Rig, severity: NoticeSeverity, text: &str) -> bool {
            rig.state.notifications.records().any(|record| {
                record.source == NoticeSource::Attached
                    && record.severity == severity
                    && record.message.contains(text)
            })
        }

        fn snapshot(rig: &Rig) -> String {
            let i = rig.state.thread_inspector.as_ref().expect("thread open");
            let address = i.item_address().unwrap();
            format!(
                "{} | {:?} | {} | {:?}",
                rig.state.modal_stack.top().expect("a frame").title(),
                i.access,
                address.item().as_str(),
                i.message_type
            )
        }

        fn set_composer(rig: &mut Rig, text: &str) {
            rig.state.input_buffer = text.to_owned();
            rig.state.cursor_position = text.chars().count();
        }

        async fn seed_two_items(rig: &Rig) {
            let journal = journal_of(rig).await;
            let peer = thread_peer();
            seed_thread_item(
                &journal,
                &peer,
                "ri-a",
                SemanticMessageType::Consultation,
                None,
                "first body",
                None,
            )
            .await;
            seed_thread_item(
                &journal,
                &peer,
                "ri-b",
                SemanticMessageType::ArchitectureUpdate,
                None,
                "second body",
                None,
            )
            .await;
        }

        /// K1: the composer front door `/team view <id>` opens ONE thread frame for the
        /// writer and the read-only mirror alike; an unknown id warns and opens nothing; a
        /// second view retargets the same frame.
        #[tokio::test]
        async fn story21_8_attached_view_command_opens_one_thread_frame_for_writer_and_read_only() {
            for read_only in [false, true] {
                let mut rig = Rig::new(read_only);
                seed_two_items(&rig).await;
                set_composer(&mut rig, "/team view ri-a");
                assert_eq!(rig.enter().await, KeyFlow::Continue);
                assert_eq!(rig.state.modal_stack.len(), 1, "read_only={read_only}");
                assert_eq!(thread_frames(&rig), 1);
                assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
                assert_eq!(
                    rig.state.modal_stack.top().unwrap().title(),
                    "Thread · consultation"
                );
                let inspector = rig.state.thread_inspector.as_ref().expect("the slot");
                assert_eq!(
                    inspector.access,
                    if read_only {
                        ThreadAccess::ReadOnly
                    } else {
                        ThreadAccess::Writer
                    }
                );
                assert!(
                    rig.state.input_buffer.is_empty(),
                    "the command was consumed"
                );
                assert_eq!(
                    rig.state.focus,
                    FocusState::Input,
                    "opening writes no base focus"
                );
                assert!(rig.frames().is_empty(), "a read is never a daemon frame");
                let painted = screen(&mut rig);
                assert!(painted.contains("Thread · consultation"), "{painted}");
                assert!(painted.contains("item actions · ri-a"), "{painted}");
                assert_eq!(
                    painted.contains("read-only — view only"),
                    read_only,
                    "the rail names the access: {painted}"
                );
                assert_eq!(painted.contains("[r] Respond"), !read_only, "{painted}");

                // A second view retargets the one frame (the loop's submit path; a typed key
                // would belong to the open frame).
                set_composer(&mut rig, "/team view ri-b");
                assert_eq!(rig.submit_composer().await, KeyFlow::Continue);
                assert_eq!(rig.state.modal_stack.len(), 1, "retarget keeps one frame");
                assert_eq!(thread_frames(&rig), 1);
                assert_eq!(
                    rig.state.modal_stack.top().unwrap().title(),
                    "Thread · architecture_update"
                );
                let painted = screen(&mut rig);
                assert!(painted.contains("item actions · ri-b"), "{painted}");
                assert!(!painted.contains("item actions · ri-a"), "{painted}");
                assert!(rig.frames().is_empty());

                assert_eq!(rig.esc().await, KeyFlow::Continue);
                assert!(rig.state.modal_stack.is_empty() && rig.state.thread_inspector.is_none());
                assert_eq!(rig.state.focus, FocusState::Input);
            }

            // An unknown id: Warning notice, empty stack, empty slot.
            let mut rig = Rig::new(false);
            seed_two_items(&rig).await;
            set_composer(&mut rig, "/team view nope");
            assert_eq!(rig.enter().await, KeyFlow::Continue);
            assert!(rig.state.modal_stack.is_empty() && rig.state.thread_inspector.is_none());
            assert!(
                notice_with(
                    &rig,
                    NoticeSeverity::Warning,
                    "recipient item 'nope' not found"
                ),
                "{:?}",
                rig.state
                    .notifications
                    .records()
                    .map(|r| r.message.clone())
                    .collect::<Vec<_>>()
            );
            assert!(rig.frames().is_empty());
            // Positive control: the same rig opens a known id.
            set_composer(&mut rig, "/team view ri-a");
            rig.enter().await;
            assert_eq!(thread_frames(&rig), 1);
        }

        /// K1: the 21-6 `[v] View item` defers the decision (it stays queued) and opens the
        /// same frame the command opens; a node without an item address has nothing to view.
        #[tokio::test]
        async fn story21_8_attached_item_view_key_defers_the_decision_and_opens_the_same_frame() {
            let mut rig = Rig::new(false);
            let submitter = SubmitterKey::loopback();
            let journal = journal_of(&rig).await;
            let address = seed_thread_item(
                &journal,
                &submitter.pseudonymous_peer_id(),
                "ri-v",
                SemanticMessageType::Consultation,
                None,
                "viewed body",
                None,
            )
            .await;
            drop(journal);
            rig.domain_tab.conversation.messages.push(ChatMessage {
                id: "row".to_owned(),
                content: "original draft".to_owned(),
                ..Default::default()
            });
            rig.domain_tab.conversation.peer_row_controls.insert(
                "row".to_owned(),
                PeerRowControl {
                    node_id: mint_inbound_node_id(&submitter, "task-ri-v")
                        .as_str()
                        .to_owned(),
                    item_address: Some(address),
                    stories: None,
                    status: None,
                    retro: None,
                    selected_for: None,
                    message_type: SemanticMessageType::Consultation,
                    sender_label: "peer".to_owned(),
                    stage: PeerRowStage::Ready,
                    node_resolvable: true,
                },
            );
            decision_queue::sync_recipient_drafts(
                &mut rig.state,
                &rig.domain_tab.conversation,
                false,
            );
            rig.open_kind(DecisionKind::RecipientDraftApproval);
            assert!(presentation::is_open(&rig.state, Surface::Decision));
            assert_eq!(rig.ch('v').await, KeyFlow::Continue);
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert_eq!(thread_frames(&rig), 1);
            assert!(
                !presentation::is_open(&rig.state, Surface::Decision),
                "the decision was deferred"
            );
            assert_eq!(decision_queue::waiting(&rig.state), 1, "still queued");
            assert!(
                rig.state
                    .decisions
                    .holds(DecisionKind::RecipientDraftApproval)
            );
            assert!(rig.frames().is_empty(), "viewing decides nothing");
            let from_key = snapshot(&rig);
            assert_eq!(
                rig.state.modal_stack.top().unwrap().title(),
                "Thread · consultation"
            );

            // The command opens the same frame on a fresh rig over the same journal shape.
            let mut other = Rig::new(false);
            let journal = journal_of(&other).await;
            seed_thread_item(
                &journal,
                &submitter.pseudonymous_peer_id(),
                "ri-v",
                SemanticMessageType::Consultation,
                None,
                "viewed body",
                None,
            )
            .await;
            drop(journal);
            set_composer(&mut other, "/team view ri-v");
            other.enter().await;
            assert_eq!(snapshot(&other), from_key);

            // Esc closes the frame; the deferred decision is still there.
            rig.esc().await;
            assert!(rig.state.thread_inspector.is_none());
            assert_eq!(decision_queue::waiting(&rig.state), 1);

            // Address-less drafts now open the response view without deciding the draft.
            let mut bare = story21_6_attached_recipient_rig();
            assert_eq!(bare.ch('v').await, KeyFlow::Continue);
            assert_eq!(thread_frames(&bare), 1);
            assert!(screen(&mut bare).contains("response actions · node-a"));
            assert!(bare.frames().is_empty());
            assert_eq!(decision_queue::waiting(&bare.state), 1);
        }

        /// K2: with the frame open every key acts by value. Scroll keys move the viewer, `h 0 $
        /// v` and an unavailable `l` are consumed no-ops, a Repeat/Release/modified writer
        /// letter and an unbound key send nothing, Chat `Tab`/`Enter` are consumed, paste is
        /// consumed, and the stack chords reach their frames and return to the same page.
        #[tokio::test]
        async fn story21_8_attached_thread_frame_keys_act_by_value() {
            let mut rig = long_thread_rig("draft").await;
            let (rows, viewport) = rig.state.thread_inspector.as_ref().unwrap().fit.get();
            let max = rows.saturating_sub(viewport);
            assert!(
                viewport >= 4 && max > 4 * viewport,
                "rows={rows} viewport={viewport}"
            );
            {
                let i = rig.state.thread_inspector.as_ref().unwrap();
                assert!(
                    i.omitted_before > 0 && i.omitted_after == 0,
                    "the fixture has an earlier page only"
                );
            }
            let (half, page) = ((viewport / 2).max(1), viewport - 1);
            let at = |rig: &mut Rig, top: usize| {
                assert_eq!(viewer_top(rig), top);
                assert!(rig.frames().is_empty());
            };

            // Scroll keys, by value.
            assert_eq!(viewer_top(&rig), 0);
            for (key, top) in [
                (Key::Char('j'), 1),
                (Key::Char('k'), 0),
                (Key::Down, 1),
                (Key::Up, 0),
                (Key::PageDown, page),
                (Key::PageUp, 0),
                (Key::Ctrl('d'), half),
                (Key::Ctrl('u'), 0),
                (Key::Char('G'), max),
                (Key::Char('g'), 0),
            ] {
                assert_eq!(rig.key(key).await, KeyFlow::Continue, "{key:?}");
                at(&mut rig, top);
            }

            // Keys with no applicable row preserve the page, composer and focus.
            rig.key(Key::PageDown).await;
            let before = page_state(&rig);
            assert_ne!(viewer_top(&rig), 0, "positive control: the viewer scrolled");
            let mode = rig.state.thread_inspector.as_ref().unwrap().mode;
            rig.ch('v').await;
            assert_ne!(rig.state.thread_inspector.as_ref().unwrap().mode, mode);
            rig.ch('v').await;
            assert_eq!(rig.state.thread_inspector.as_ref().unwrap().mode, mode);
            screen(&mut rig);
            for key in [
                Key::Char('h'),
                Key::Char('0'),
                Key::Char('$'),
                Key::Char('l'),
                Key::Char('z'),
                Key::Alt('b'),
                Key::Tab,
                Key::Enter,
            ] {
                assert_eq!(rig.key(key).await, KeyFlow::Continue, "{key:?}");
                assert_eq!(page_state(&rig), before, "{key:?} must not touch the page");
                assert_eq!(rig.state.focus, FocusState::Chat);
                assert!(rig.frames().is_empty(), "{key:?} sends nothing");
            }
            for (code, modifiers, kind) in [
                (KeyCode::Char('b'), KeyModifiers::NONE, KeyEventKind::Repeat),
                (
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                (KeyCode::Char('N'), KeyModifiers::SHIFT, KeyEventKind::Press),
            ] {
                rig.raw_key(KeyEvent::new_with_kind(code, modifiers, kind))
                    .await;
                assert_eq!(page_state(&rig), before, "{code:?} {kind:?}");
                assert!(rig.frames().is_empty(), "{code:?} {kind:?} sends nothing");
            }
            rig.paste("pasted text").await;
            assert_eq!(page_state(&rig), before, "paste is consumed by the frame");
            assert_eq!(rig.state.input_buffer, "draft");
            assert!(rig.frames().is_empty());

            // The stack chords reach their frames and Esc returns to the same page/viewer.
            rig.ctrl('x').await;
            rig.ch('n').await;
            assert_eq!(top_surface(&rig), Some(Surface::NotificationLog));
            assert!(
                rig.state.thread_inspector.is_some(),
                "S-LOG opened over the thread"
            );
            rig.esc().await;
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert_eq!(page_state(&rig), before);
            rig.ctrl('x').await;
            rig.ch('?').await;
            assert_eq!(top_surface(&rig), Some(Surface::Help));
            rig.esc().await;
            assert_eq!(page_state(&rig), before);
            rig.ctrl('x').await;
            rig.ch('q').await;
            assert_eq!(top_surface(&rig), Some(Surface::DecisionQueue));
            rig.esc().await;
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert_eq!(page_state(&rig), before);
            rig.ctrl('p').await;
            assert_eq!(top_surface(&rig), Some(Surface::CommandPalette));
            rig.esc().await;
            assert_eq!(page_state(&rig), before);
            rig.ctrl('q').await;
            assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
            assert_eq!(top_surface(&rig), Some(Surface::QuitConfirm));
            rig.ch('n').await;
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert_eq!(page_state(&rig), before);
            assert!(rig.frames().is_empty());

            // `l` acts once an omitted later page exists.
            rig.ch('p').await;
            assert!(rig.state.thread_inspector.as_ref().unwrap().omitted_after > 0);
            let earlier_first = rig
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .first_non_pinned_ordinal;
            rig.ch('l').await;
            assert_ne!(
                rig.state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .first_non_pinned_ordinal,
                earlier_first,
                "`l` loads the newer page"
            );

            // Esc and Ctrl+C go back exactly one frame and never touch the base focus.
            for key in [Key::Esc, Key::Ctrl('c')] {
                let mut rig =
                    item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
                set_composer(&mut rig, "draft");
                rig.ctrl('x').await;
                rig.ch('n').await;
                assert_eq!(top_surface(&rig), Some(Surface::NotificationLog));
                assert_eq!(rig.key(key).await, KeyFlow::Continue);
                assert_eq!(
                    top_surface(&rig),
                    Some(Surface::ThreadInspector),
                    "{key:?} closes the log only"
                );
                assert_eq!(rig.key(key).await, KeyFlow::Continue);
                assert!(
                    rig.state.modal_stack.is_empty() && rig.state.thread_inspector.is_none(),
                    "{key:?}"
                );
                assert_eq!(rig.state.focus, FocusState::Chat, "{key:?}");
                assert_eq!(rig.state.input_buffer, "draft");
                assert!(
                    rig.frames().is_empty(),
                    "{key:?} neither interrupts nor sends"
                );
            }
        }

        /// What a writer key did: open its reply field, send one frame, or nothing.
        #[derive(Debug, PartialEq)]
        enum Effect {
            Nothing,
            ReplyField(String),
            Frame(String),
        }

        fn frame_effect(frame: ClientFrame) -> Effect {
            Effect::Frame(format!("{frame:?}"))
        }

        async fn press_effect(rig: &mut Rig, key: char) -> Effect {
            let composer = rig.state.input_buffer.clone();
            assert_eq!(rig.key(Key::Char(key)).await, KeyFlow::Continue);
            let frames = rig.frames();
            assert!(
                !frames
                    .iter()
                    .any(|frame| matches!(frame, ClientFrame::ResolvePeerDraft { .. })),
                "S-THREAD never resolves a peer draft: {frames:?}"
            );
            assert_eq!(
                rig.state.input_buffer, composer,
                "writer keys never alter the composer"
            );
            match frames.as_slice() {
                [] if rig
                    .state
                    .thread_inspector
                    .as_ref()
                    .is_some_and(|i| i.reply.is_some()) =>
                {
                    let i = rig.state.thread_inspector.as_ref().unwrap();
                    Effect::ReplyField(
                        i.reply
                            .as_ref()
                            .unwrap()
                            .prompt(i.item_address().unwrap().item().as_str()),
                    )
                }
                [] => Effect::Nothing,
                [one] => Effect::Frame(format!("{one:?}")),
                many => panic!("`{key}` sent more than one frame: {many:?}"),
            }
        }

        const RAIL: [(char, &str); 9] = [
            ('r', "[r] Respond"),
            ('d', "[d] Discuss"),
            ('b', "[b] Busy"),
            ('a', "[a] Acknowledge"),
            ('y', "[y] Accept"),
            ('s', "[s] Share"),
            ('n', "[n] Decline"),
            ('1', "[1-3] Leave out"),
            ('u', "[u] "),
        ];

        fn painted_keys(painted: &str) -> BTreeSet<char> {
            RAIL.iter()
                .filter(|(_, label)| painted.contains(label))
                .map(|(key, _)| *key)
                .collect()
        }

        /// K5: the painted rail keys equal the applicable rows; each painted key does exactly
        /// its effect once; each unpainted writer key does nothing and leaves the frame open.
        async fn assert_rail<F, Fut>(
            name: &str,
            make: F,
            painted: &[char],
            effects: &[(char, Effect)],
        ) where
            F: Fn() -> Fut,
            Fut: Future<Output = Rig>,
        {
            let mut rig = make().await;
            let shown = painted_keys(&screen(&mut rig));
            assert_eq!(
                shown,
                painted.iter().copied().collect::<BTreeSet<_>>(),
                "{name}: painted rail keys"
            );
            assert_eq!(
                effects.iter().map(|(key, _)| *key).collect::<BTreeSet<_>>(),
                painted.iter().copied().collect::<BTreeSet<_>>(),
                "{name}: every painted key has an expected effect"
            );
            for key in WRITER_KEYS {
                let mut rig = make().await;
                screen(&mut rig);
                let effect = press_effect(&mut rig, key).await;
                match effects.iter().find(|(painted_key, _)| *painted_key == key) {
                    Some((_, expected)) => {
                        assert_eq!(&effect, expected, "{name}: painted `{key}`");
                        if matches!(effect, Effect::Frame(_)) {
                            assert!(
                                rig.state.thread_inspector.is_some(),
                                "{name}: `{key}` keeps the frame open"
                            );
                            assert_eq!(
                                press_effect(&mut rig, key).await,
                                Effect::Nothing,
                                "{name}: `{key}` is sent once while in flight"
                            );
                        }
                    }
                    None => {
                        assert_eq!(
                            effect,
                            Effect::Nothing,
                            "{name}: unpainted `{key}` must do nothing"
                        );
                        assert!(
                            rig.state.thread_inspector.is_some(),
                            "{name}: `{key}` keeps the frame open"
                        );
                    }
                }
            }
        }

        const REPLY: &str = "/team reply ri-1 ";

        fn defer() -> Effect {
            frame_effect(ClientFrame::DeferRecipientItem {
                item_id: "ri-1".to_owned(),
            })
        }

        fn acknowledge() -> Effect {
            frame_effect(ClientFrame::AcknowledgeRecipientItem {
                item_id: "ri-1".to_owned(),
            })
        }

        fn context(selected: bool) -> Effect {
            frame_effect(ClientFrame::SetRecipientContext {
                item_id: "ri-1".to_owned(),
                selected,
            })
        }

        async fn busy_in_flight_rig() -> Rig {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, true).await;
            screen(&mut rig);
            assert_eq!(rig.ch('b').await, KeyFlow::Continue);
            assert_eq!(rig.frames().len(), 1, "the Busy key queued its one frame");
            rig
        }

        /// K5: rail truth for a consultation writer, an architecture update, an acknowledged
        /// item, a story list offering Accept, the read-only mirror and an in-flight Busy.
        #[tokio::test]
        async fn story21_8_attached_rail_keys_equal_the_applicable_rows_and_send_their_frames() {
            use crate::adapters::daemon::protocol::ReplyVerdict;
            use SemanticMessageType::{ArchitectureUpdate, Consultation, StoryAssignment};
            assert_rail(
                "consultation writer",
                || item_rig(false, Consultation, None, false, true),
                &['r', 'b', 'u'],
                &[
                    ('r', Effect::ReplyField(REPLY.to_owned())),
                    ('b', defer()),
                    ('u', context(true)),
                ],
            )
            .await;
            assert_rail(
                "architecture update",
                || item_rig(false, ArchitectureUpdate, None, false, false),
                &['d', 'a', 'u'],
                &[
                    ('d', Effect::ReplyField(REPLY.to_owned())),
                    ('a', acknowledge()),
                    ('u', context(true)),
                ],
            )
            .await;
            assert_rail(
                "acknowledged item",
                || item_rig(false, ArchitectureUpdate, None, true, false),
                &['d', 'u'],
                &[
                    ('d', Effect::ReplyField(REPLY.to_owned())),
                    ('u', context(true)),
                ],
            )
            .await;
            let list = || {
                Some(ReceivedStories::Listed {
                    stories: vec![ReceivedStory {
                        key: StoryKey::parse("S1").unwrap(),
                        holder: StoryHolder::You,
                    }],
                })
            };
            assert_rail(
                "story assignment offering Accept",
                move || item_rig(false, StoryAssignment, list(), false, false),
                &['d', 'a', 'y', 'u'],
                &[
                    (
                        'd',
                        Effect::ReplyField("/team reply --discuss ri-1 ".to_owned()),
                    ),
                    ('a', acknowledge()),
                    (
                        'y',
                        frame_effect(ClientFrame::ReplyToRecipientItem {
                            item_id: "ri-1".to_owned(),
                            text: String::new(),
                            verdict: Some(ReplyVerdict::Accept),
                        }),
                    ),
                    ('u', context(true)),
                ],
            )
            .await;
            assert_rail(
                "read-only mirror",
                || item_rig(true, Consultation, None, false, false),
                &[],
                &[],
            )
            .await;
            let mut mirror = item_rig(true, Consultation, None, false, false).await;
            let painted = screen(&mut mirror);
            assert!(painted.contains("read-only — view only"), "{painted}");
            assert_rail(
                "Busy in flight",
                busy_in_flight_rig,
                &['r', 'u'],
                &[
                    ('r', Effect::ReplyField(REPLY.to_owned())),
                    ('u', context(true)),
                ],
            )
            .await;
        }

        /// K6: `u` queues the context frame; the rail and the identity row change only after
        /// the correlated outcome; `u` again removes; `c` has no row.
        #[tokio::test]
        async fn story21_8_attached_context_key_toggles_on_the_correlated_outcome() {
            let mut rig = item_rig(
                false,
                SemanticMessageType::ArchitectureUpdate,
                None,
                false,
                false,
            )
            .await;
            let target = || RecipientActionTarget::Item {
                item_id: "ri-1".to_owned(),
            };
            let before = screen(&mut rig);
            assert!(before.contains("[u] Use as context"), "{before}");
            assert!(!before.contains("peer-tainted"), "{before}");

            assert_eq!(rig.ch('u').await, KeyFlow::Continue);
            let frames = rig.frames();
            assert!(
                matches!(frames.as_slice(), [ClientFrame::SetRecipientContext { item_id, selected: true }] if item_id == "ri-1"),
                "{frames:?}"
            );
            let during = screen(&mut rig);
            assert!(
                !during.contains("[u] Use as context") && !during.contains("Stop using as context"),
                "no context key while the daemon answers: {during}"
            );
            assert!(
                !during.contains("peer-tainted"),
                "the mark waits for the outcome: {during}"
            );
            rig.ch('u').await;
            assert!(rig.frames().is_empty(), "in flight: the key is withheld");

            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                RecipientAction::ContextSelect,
                target(),
                RecipientActionOutcome::ContextSelection {
                    selected: true,
                    changed: true,
                },
            );
            let after = screen(&mut rig);
            assert!(after.contains("[u] Stop using as context"), "{after}");
            assert!(after.contains(" · peer-tainted"), "{after}");
            assert!(rig.state.recipient_context_ids.contains("ri-1"));

            let state = page_state(&rig);
            rig.state.clipboard_copier = Some(std::sync::Arc::new(|_| {
                crate::adapters::tui::clipboard::ClipboardResult::Osc52Success
            }));
            assert_eq!(rig.ch('c').await, KeyFlow::Continue);
            assert!(rig.frames().is_empty(), "`c` sends nothing");
            assert_eq!(page_state(&rig), state);

            assert_eq!(rig.ch('u').await, KeyFlow::Continue);
            let frames = rig.frames();
            assert!(
                matches!(frames.as_slice(), [ClientFrame::SetRecipientContext { item_id, selected: false }] if item_id == "ri-1"),
                "{frames:?}"
            );
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                RecipientAction::ContextRemove,
                target(),
                RecipientActionOutcome::ContextSelection {
                    selected: false,
                    changed: true,
                },
            );
            let removed = screen(&mut rig);
            assert!(removed.contains("[u] Use as context"), "{removed}");
            assert!(!removed.contains("peer-tainted"), "{removed}");
            assert!(!rig.state.recipient_context_ids.contains("ri-1"));
        }

        /// K7: `p`/`l` retain root and item and put the viewer back at the top; the pager is
        /// painted, and acts, only when an omitted page exists.
        #[tokio::test]
        async fn story21_8_attached_paging_retains_root_and_item_and_paints_only_with_an_omitted_page()
         {
            // No omitted page: nothing painted, both keys are consumed no-ops.
            let mut short =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            let painted = screen(&mut short);
            assert!(
                !painted.contains("[p] Earlier") && !painted.contains("[l] Newer"),
                "{painted}"
            );
            let state = page_state(&short);
            for key in ['p', 'l'] {
                assert_eq!(short.ch(key).await, KeyFlow::Continue);
                assert_eq!(page_state(&short), state, "`{key}` without a page");
                assert!(short.frames().is_empty());
            }

            let mut rig = long_thread_rig("").await;
            let painted = screen(&mut rig);
            assert!(painted.contains("[p] Earlier"), "{painted}");
            assert!(!painted.contains("[l] Newer"), "{painted}");
            assert!(
                painted.contains("item actions · t-open")
                    && painted.contains("thread root · t-root"),
                "{painted}"
            );
            let first = {
                let i = rig.state.thread_inspector.as_ref().unwrap();
                (
                    i.first_non_pinned_ordinal.expect("a linked turn"),
                    i.omitted_before,
                )
            };
            rig.key(Key::PageDown).await;
            assert_ne!(
                viewer_top(&rig),
                0,
                "positive control: scrolled before paging"
            );

            assert_eq!(rig.ch('p').await, KeyFlow::Continue);
            assert!(rig.frames().is_empty(), "paging is a read, never a frame");
            let earlier = {
                let i = rig.state.thread_inspector.as_ref().unwrap();
                assert_eq!(i.root_id.as_deref(), Some("t-root"), "the root is retained");
                let address = i.item_address().unwrap();
                assert_eq!(
                    address.item().as_str(),
                    "t-open",
                    "the opened item is retained"
                );
                assert_eq!(i.viewer.top, 0, "back at the top");
                assert!(i.omitted_after > 0, "a later page now exists");
                let ordinal = i.first_non_pinned_ordinal.expect("a linked turn");
                assert!(
                    ordinal < first.0 && i.omitted_before < first.1,
                    "a real earlier page"
                );
                i.last_non_pinned_ordinal.expect("a linked turn")
            };
            assert_eq!(thread_frames(&rig), 1);
            let painted = screen(&mut rig);
            assert!(painted.contains("[l] Newer"), "{painted}");
            assert!(
                painted.contains("item actions · t-open")
                    && painted.contains("thread root · t-root"),
                "{painted}"
            );

            rig.key(Key::PageDown).await;
            assert_ne!(viewer_top(&rig), 0);
            assert_eq!(rig.ch('l').await, KeyFlow::Continue);
            assert!(rig.frames().is_empty());
            let i = rig.state.thread_inspector.as_ref().unwrap();
            assert_eq!(i.root_id.as_deref(), Some("t-root"));
            let address = i.item_address().unwrap();
            assert_eq!(address.item().as_str(), "t-open");
            assert_eq!(i.viewer.top, 0, "back at the top");
            assert!(
                i.first_non_pinned_ordinal
                    .is_some_and(|ordinal| ordinal > earlier),
                "the newer page follows the earlier one"
            );
            assert!(i.omitted_before > 0);
        }

        fn full_composer() -> String {
            (0..12)
                .map(|n| format!("composer line {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// 8A-L1-01 (attached half), one item kind: with the terminal at the 60x16 floor and
        /// a grown composer, a writer key sends a frame only if its hint is PAINTED (read
        /// from the painted cells); every other writer key sends ZERO frames and changes
        /// nothing. The positive control is the same item on the roomy 80x24 terminal.
        async fn assert_floor_rail<F, Fut>(
            name: &str,
            make: F,
            roomy: &[(char, Effect)],
            floor_painted: Option<&[char]>,
        ) where
            F: Fn() -> Fut,
            Fut: Future<Output = Rig>,
        {
            assert!(!roomy.is_empty(), "{name}: the control must send frames");
            for (key, expected) in roomy {
                let mut rig = make().await;
                let painted = screen(&mut rig);
                assert!(
                    painted_keys(&painted).contains(key),
                    "{name}: roomy control paints `{key}`: {painted}"
                );
                assert_eq!(
                    &press_effect(&mut rig, *key).await,
                    expected,
                    "{name}: roomy `{key}`"
                );
            }
            let composer = full_composer();
            for key in WRITER_KEYS {
                let mut rig = make().await;
                rig.state.terminal_width = 60;
                rig.state.terminal_height = 16;
                set_composer(&mut rig, &composer);
                let painted = screen(&mut rig);
                let shown = painted_keys(&painted);
                assert!(painted.contains("Thread · "), "{name}: the title row");
                assert!(
                    painted.contains("Close"),
                    "{name}: the reader rail: {painted}"
                );
                let inspector = rig.state.thread_inspector.as_ref().unwrap();
                assert!(inspector.fit.get().1 >= 1, "{name}: a body row is painted");
                assert_eq!(
                    inspector.rail_live.get().0,
                    !shown.is_empty(),
                    "{name}: the writer rail is live exactly when it is painted: {painted}"
                );
                if let Some(expected) = floor_painted {
                    assert_eq!(
                        shown,
                        expected.iter().copied().collect::<BTreeSet<_>>(),
                        "{name}: painted writer keys at the floor: {painted}"
                    );
                }
                let effect = press_effect(&mut rig, key).await;
                match roomy.iter().find(|(frame_key, _)| *frame_key == key) {
                    Some((_, expected)) if shown.contains(&key) => {
                        assert_eq!(&effect, expected, "{name}: painted `{key}` at the floor");
                    }
                    _ => assert_eq!(
                        effect,
                        Effect::Nothing,
                        "{name}: `{key}` is not a painted frame key at the floor: zero frames"
                    ),
                }
                assert!(rig.state.thread_inspector.is_some(), "{name}: `{key}`");
            }
        }

        #[tokio::test]
        async fn story21_8_attached_unpainted_writer_keys_send_no_frame_at_the_floor_with_a_full_composer()
         {
            use crate::adapters::daemon::protocol::ReplyVerdict;
            use SemanticMessageType::{ArchitectureUpdate, Consultation, StoryAssignment};
            assert_floor_rail(
                "consultation writer",
                || item_rig(false, Consultation, None, false, true),
                &[('b', defer()), ('u', context(true))],
                None,
            )
            .await;
            assert_floor_rail(
                "architecture update",
                || item_rig(false, ArchitectureUpdate, None, false, false),
                &[('a', acknowledge()), ('u', context(true))],
                None,
            )
            .await;
            let list = || {
                Some(ReceivedStories::Listed {
                    stories: vec![ReceivedStory {
                        key: StoryKey::parse("S1").unwrap(),
                        holder: StoryHolder::You,
                    }],
                })
            };
            // `[d] .. [a] .. [y] .. [u]` wraps to two rows: with the reader rail it does not fit
            // the three inner rows, so none of its keys is painted and none may send.
            assert_floor_rail(
                "story assignment offering Accept",
                move || item_rig(false, StoryAssignment, list(), false, false),
                &[
                    ('a', acknowledge()),
                    (
                        'y',
                        frame_effect(ClientFrame::ReplyToRecipientItem {
                            item_id: "ri-1".to_owned(),
                            text: String::new(),
                            verdict: Some(ReplyVerdict::Accept),
                        }),
                    ),
                    ('u', context(true)),
                ],
                Some(&[]),
            )
            .await;

            // The pager obeys the same truth: `p` pages exactly when `[p] Earlier` is painted.
            {
                let mut rig = long_thread_rig("").await;
                let painted = screen(&mut rig);
                assert!(painted.contains("[p] Earlier"), "{painted}");
                let before = page_state(&rig);
                assert_eq!(rig.ch('p').await, KeyFlow::Continue);
                assert_ne!(page_state(&rig), before, "positive control: p pages");
            }
            let mut rig = long_thread_rig("").await;
            rig.state.terminal_width = 60;
            rig.state.terminal_height = 16;
            set_composer(&mut rig, &full_composer());
            let painted = screen(&mut rig);
            assert!(painted.contains("Close"), "{painted}");
            let hint = painted.contains("[p] Earlier");
            assert_eq!(
                rig.state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .rail_live
                    .get()
                    .1,
                hint,
                "the pager is live exactly when its hint is painted"
            );
            let before = page_state(&rig);
            assert_eq!(rig.ch('p').await, KeyFlow::Continue);
            assert_eq!(
                page_state(&rig) != before,
                hint,
                "p pages iff [p] is painted at the floor: {painted}"
            );
            assert!(rig.frames().is_empty(), "paging is a read, never a frame");
        }

        /// 8A-L2-03(iii), attached host: a wheel event through the shared route boundary
        /// scrolls the thread body by the clamp and reaches neither the composer, the Stage
        /// view nor the daemon; with no frame open the same wheel scrolls the Stage.
        #[tokio::test]
        async fn story21_8_attached_wheel_scrolls_the_thread_body_and_reaches_nothing_else() {
            use crossterm::event::{MouseEvent, MouseEventKind};
            let wheel = |kind| {
                Event::Mouse(MouseEvent {
                    kind,
                    column: 1,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                })
            };
            let mut rig = long_thread_rig("draft").await;
            for n in 0..60 {
                rig.domain_tab.conversation.messages.push(ChatMessage {
                    id: format!("stage-{n}"),
                    content: format!("stage row {n}"),
                    ..Default::default()
                });
            }
            screen(&mut rig);
            let (rows, viewport) = rig.state.thread_inspector.as_ref().unwrap().fit.get();
            let max = rows.saturating_sub(viewport);
            assert!(max > 1, "the thread spans several pages");
            let notch = usize::from(MouseConfig::default().wheel_lines.max(1));
            let untouched = |rig: &Rig| {
                format!(
                    "{:?}",
                    (
                        &rig.state.input_buffer,
                        rig.state.cursor_position,
                        &rig.state.focus,
                        rig.domain_tab.view_state.scroll_offset,
                        rig.domain_tab.view_state.mode.clone(),
                    )
                )
            };
            let before = untouched(&rig);
            let step = |rig: &mut Rig, kind| {
                assert_eq!(
                    handle_attached_mouse(
                        &mut rig.state,
                        &mut rig.domain_tab.view_state,
                        &wheel(kind)
                    ),
                    Some(RouteEffect::Consumed),
                    "the frame owns the wheel"
                );
            };
            step(&mut rig, MouseEventKind::ScrollDown);
            assert_eq!(
                viewer_top(&rig),
                notch.min(max),
                "one notch scrolls by wheel_lines"
            );
            for _ in 0..max.div_ceil(notch) + 2 {
                step(&mut rig, MouseEventKind::ScrollDown);
            }
            assert_eq!(
                viewer_top(&rig),
                max,
                "the wheel clamps at the last painted row"
            );
            step(&mut rig, MouseEventKind::ScrollUp);
            assert_eq!(viewer_top(&rig), max.saturating_sub(notch));
            for _ in 0..max.div_ceil(notch) + 2 {
                step(&mut rig, MouseEventKind::ScrollUp);
            }
            assert_eq!(viewer_top(&rig), 0, "the wheel clamps at the first row");
            assert_eq!(
                untouched(&rig),
                before,
                "composer, focus and Stage view untouched"
            );
            assert!(rig.frames().is_empty(), "a wheel never reaches the daemon");
            assert_eq!(thread_frames(&rig), 1);

            // Positive control: with no frame open the same wheel scrolls the Stage.
            assert_eq!(rig.esc().await, KeyFlow::Continue);
            assert!(rig.state.modal_stack.is_empty());
            screen(&mut rig);
            let stage_before = untouched(&rig);
            assert_eq!(
                handle_attached_mouse(
                    &mut rig.state,
                    &mut rig.domain_tab.view_state,
                    &wheel(MouseEventKind::ScrollUp)
                ),
                None,
                "with no frame open the route releases the wheel"
            );
            assert_ne!(untouched(&rig), stage_before, "the Stage view scrolled");
        }

        /// A Busy whose node no longer joins the item (the journal no longer maps that node to
        /// this item) sends nothing and says why in the frame; the real join sends the Defer.
        #[tokio::test]
        async fn story21_8_attached_busy_with_a_stale_node_join_sends_nothing_and_flashes_not_found()
         {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, true).await;
            screen(&mut rig);
            assert_eq!(
                press_effect(&mut rig, 'b').await,
                defer(),
                "positive control: the real join sends the Defer"
            );

            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, true).await;
            screen(&mut rig);
            let stale = mint_inbound_node_id(&SubmitterKey::loopback(), "task-gone")
                .as_str()
                .to_owned();
            rig.state
                .thread_inspector
                .as_mut()
                .expect("thread open")
                .node_id = Some(stale);
            assert_eq!(
                press_effect(&mut rig, 'b').await,
                Effect::Nothing,
                "a stale join queues no frame"
            );
            assert!(rig.state.thread_inspector.is_some(), "the frame stays open");
            let painted = screen(&mut rig);
            assert!(
                painted.contains("recipient item 'ri-1' not found"),
                "the refusal is shown in the frame: {painted}"
            );
        }

        fn reply_text(rig: &Rig) -> Option<&str> {
            rig.state
                .thread_inspector
                .as_ref()?
                .reply
                .as_ref()
                .map(|r| r.field.as_str())
        }

        #[tokio::test]
        async fn story21_8_attached_node_view_tab_toggles_item_and_response() {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, true).await;
            rig.state.terminal_width = 220;
            set_composer(&mut rig, "saved draft");
            let node = rig
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .node_id
                .clone()
                .unwrap();
            assert!(screen(&mut rig).contains("item actions · ri-1"));
            rig.press(KeyCode::Tab, KeyModifiers::NONE).await;
            let painted = screen(&mut rig);
            assert!(painted.contains("Thread · response"), "{painted}");
            assert!(
                painted.contains(&format!("response actions · {node}")),
                "{painted}"
            );
            rig.press(KeyCode::Tab, KeyModifiers::NONE).await;
            assert!(screen(&mut rig).contains("item actions · ri-1"));
            assert_eq!(rig.state.input_buffer, "saved draft");
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_v_on_an_addressless_draft_opens_the_node_view() {
            let mut rig = story21_6_attached_recipient_rig();
            set_composer(&mut rig, "saved draft");
            assert_eq!(top_surface(&rig), Some(Surface::Decision));
            rig.ch('v').await;
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            let painted = screen(&mut rig);
            assert!(
                painted.contains("Thread · response")
                    && painted.contains("response actions · node-a"),
                "{painted}"
            );
            assert_eq!(decision_queue::waiting(&rig.state), 1);
            assert_eq!(rig.state.input_buffer, "saved draft");
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_o_opens_the_queued_draft_decision_and_esc_returns_exactly() {
            let mut rig = story21_6_attached_recipient_rig();
            rig.ch('v').await;
            let before = screen(&mut rig);
            assert!(before.contains("[o] Open decision"), "{before}");
            let top = rig.state.thread_inspector.as_ref().unwrap().viewer.top;
            rig.ch('o').await;
            assert_eq!(top_surface(&rig), Some(Surface::Decision));
            assert!(
                rig.state
                    .decisions
                    .holds(DecisionKind::RecipientDraftApproval)
            );
            rig.ready();
            rig.esc().await;
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert_eq!(rig.state.thread_inspector.as_ref().unwrap().viewer.top, top);
            let after = screen(&mut rig);
            assert!(
                after.contains("response actions · node-a") && after.contains("[o] Open decision"),
                "{after}"
            );
            assert!(rig.frames().is_empty());

            // The same route on a response linked to a journal item opens that
            // node's queued decision, not an unrelated draft.
            let mut linked =
                item_rig(false, SemanticMessageType::Consultation, None, false, true).await;
            let node = linked
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .node_id
                .clone()
                .unwrap();
            let control = linked
                .domain_tab
                .conversation
                .peer_row_controls
                .get_mut("row")
                .unwrap();
            control.stage = PeerRowStage::Ready;
            control.node_resolvable = true;
            decision_queue::sync_recipient_drafts(
                &mut linked.state,
                &linked.domain_tab.conversation,
                false,
            );
            if top_surface(&linked) == Some(Surface::Decision) {
                linked.ready();
                linked.esc().await;
            }
            linked.press(KeyCode::Tab, KeyModifiers::NONE).await;
            assert!(screen(&mut linked).contains("[o] Open decision"));
            let page = linked
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .content
                .clone();
            let top = linked.state.thread_inspector.as_ref().unwrap().viewer.top;
            linked.ch('o').await;
            assert_eq!(
                decision_queue::focused_key(&linked.state).unwrap().source,
                node
            );
            linked.ready();
            linked.esc().await;
            let inspector = linked.state.thread_inspector.as_ref().unwrap();
            assert_eq!(inspector.content, page);
            assert_eq!(inspector.viewer.top, top);
            assert!(linked.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_o_absent_and_inert_when_stale_other_tab_or_none_queued() {
            let mut control = story21_6_attached_recipient_rig();
            control.ch('v').await;
            assert!(screen(&mut control).contains("[o] Open decision"));
            control.ch('o').await;
            assert_eq!(top_surface(&control), Some(Surface::Decision));

            for case in 0..3 {
                let mut rig = story21_6_attached_recipient_rig();
                rig.ch('v').await;
                match case {
                    0 => {
                        rig.state.thread_inspector.as_mut().unwrap().target =
                            ThreadTarget::Node("stale-node".into());
                    }
                    1 => {
                        rig.domain_tab.conversation.id = "other-tab".into();
                    }
                    _ => {
                        rig.domain_tab.conversation.peer_row_controls.clear();
                        decision_queue::sync_recipient_drafts(
                            &mut rig.state,
                            &rig.domain_tab.conversation,
                            false,
                        );
                    }
                }
                let painted = screen(&mut rig);
                assert!(
                    !painted.contains("[o] Open decision"),
                    "case {case}: {painted}"
                );
                rig.ch('o').await;
                assert_eq!(
                    top_surface(&rig),
                    Some(Surface::ThreadInspector),
                    "case {case}"
                );
                assert!(rig.frames().is_empty());
            }

            // Retain a genuinely stale queue item (normally kept while its decision
            // is being read), then expose the node view beneath it.
            let mut stale = story21_6_attached_recipient_rig();
            let controls = stale.domain_tab.conversation.peer_row_controls.clone();
            stale.domain_tab.conversation.peer_row_controls.clear();
            decision_queue::sync_recipient_drafts(
                &mut stale.state,
                &stale.domain_tab.conversation,
                false,
            );
            assert!(stale.state.decisions.items().any(|item| item.is_stale()));
            stale.domain_tab.conversation.peer_row_controls = controls;
            presentation::reducer::close(&mut stale.state, Surface::Decision);
            crate::infrastructure::runtime::thread_open::open_node(
                &stale.domain_tab.conversation,
                "node-a",
                &mut stale.state,
                ThreadAccess::Writer,
            )
            .unwrap();
            let painted = screen(&mut stale);
            assert!(!painted.contains("[o] Open decision"), "{painted}");
            stale.ch('o').await;
            assert_eq!(top_surface(&stale), Some(Surface::ThreadInspector));
            assert!(stale.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_node_view_offers_no_draft_rows_and_never_touches_the_composer()
        {
            let mut rig = story21_6_attached_recipient_rig();
            set_composer(&mut rig, "saved draft");
            rig.state.cursor_position = 3;
            rig.ch('v').await;
            let painted = screen(&mut rig);
            assert!(painted.contains("[o] Open decision"), "{painted}");
            for key in ['y', 'e', 'n', 'r', 'd'] {
                assert!(!painted.contains(&format!("[{key}]")), "{painted}");
                rig.ch(key).await;
                assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
                assert_eq!(
                    (&*rig.state.input_buffer, rig.state.cursor_position),
                    ("saved draft", 3)
                );
                assert!(rig.frames().is_empty());
            }
        }

        #[tokio::test]
        async fn story21_8_attached_reply_field_prompt_and_enter_send_through_team_reply() {
            use crate::adapters::daemon::protocol::ReplyVerdict;
            let stories = || {
                Some(ReceivedStories::Listed {
                    stories: vec![ReceivedStory {
                        key: StoryKey::parse("S1").unwrap(),
                        holder: StoryHolder::You,
                    }],
                })
            };
            for (kind, list, key, prompt, verdict) in [
                (
                    SemanticMessageType::StoryAssignment,
                    stories(),
                    'd',
                    "/team reply --discuss ri-1",
                    Some(ReplyVerdict::Discuss),
                ),
                (
                    SemanticMessageType::Consultation,
                    None,
                    'r',
                    "/team reply ri-1",
                    None,
                ),
                (
                    SemanticMessageType::ArchitectureUpdate,
                    None,
                    'd',
                    "/team reply ri-1",
                    None,
                ),
            ] {
                let mut rig = item_rig(false, kind, list, false, false).await;
                set_composer(&mut rig, "saved draft");
                rig.state.cursor_position = 3;
                rig.ch(key).await;
                let painted = screen(&mut rig);
                assert!(painted.contains(prompt), "{painted}");
                assert!(
                    !painted.contains("input already contains text"),
                    "{painted}"
                );
                super::super::handle_attached_paste(&mut rig.state, "x\ny", false);
                rig.type_text("z").await;
                assert_eq!(reply_text(&rig), Some("x\nyz"));
                rig.enter().await;
                let frames = rig.frames();
                assert!(
                    matches!(frames.as_slice(), [ClientFrame::ReplyToRecipientItem { item_id, text, verdict: actual }] if item_id == "ri-1" && text == "x\nyz" && *actual == verdict),
                    "{frames:?}"
                );
                assert!(rig.state.thread_inspector.is_none());
                assert_eq!(rig.state.input_buffer, "saved draft");
                assert_eq!(rig.state.cursor_position, 3);
            }
        }

        #[tokio::test]
        async fn story21_8_attached_empty_enter_does_nothing_and_esc_clears_then_backs() {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            set_composer(&mut rig, "saved draft");
            rig.ch('r').await;
            rig.enter().await;
            assert_eq!(reply_text(&rig), Some(""));
            assert!(rig.frames().is_empty());
            rig.type_text("unsent").await;
            rig.esc().await;
            assert_eq!(reply_text(&rig), None);
            rig.ch('r').await;
            assert_eq!(reply_text(&rig), Some(""));
            rig.esc().await;
            rig.esc().await;
            assert!(rig.state.thread_inspector.is_none());
            assert_eq!(rig.state.input_buffer, "saved draft");
        }

        #[tokio::test]
        async fn story21_8_attached_reply_over_64_kib_is_refused_before_anything_is_sent() {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            set_composer(&mut rig, "saved draft");
            rig.ch('r').await;
            super::super::handle_attached_paste(&mut rig.state, &"x".repeat(64 * 1024 + 1), false);
            rig.enter().await;
            assert!(rig.frames().is_empty());
            let key = rig.state.thread_inspector.as_ref().unwrap().outcome_key();
            assert!(
                rig.state
                    .thread_outcomes
                    .lines(&key)
                    .any(|line| line.contains("reply is longer than 64 KiB")),
                "the refusal is an outcome line of the open thread"
            );
            assert_eq!(reply_text(&rig).unwrap().len(), 64 * 1024 + 1);
            assert_eq!(rig.state.input_buffer, "saved draft");
            rig.esc().await;
            rig.ch('r').await;
            rig.type_text("short").await;
            rig.enter().await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::ReplyToRecipientItem { text, .. }] if text == "short")
            );
        }

        #[tokio::test]
        async fn story21_8_attached_refused_reply_keeps_its_text_in_the_thread_not_the_composer() {
            for outcome in [
                RecipientActionOutcome::Refused {
                    error: crate::adapters::daemon::protocol::ProtocolError::ReadOnly,
                },
                RecipientActionOutcome::Idempotent,
            ] {
                let mut rig = item_rig(
                    false,
                    SemanticMessageType::ArchitectureUpdate,
                    None,
                    false,
                    false,
                )
                .await;
                set_composer(&mut rig, "saved draft");
                rig.ch('d').await;
                rig.type_text("retained reply").await;
                rig.enter().await;
                assert!(
                    matches!(rig.frames().as_slice(), [ClientFrame::ReplyToRecipientItem { text, .. }] if text == "retained reply")
                );
                crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                    &mut rig.state,
                    RecipientAction::Reply,
                    RecipientActionTarget::Item {
                        item_id: "ri-1".into(),
                    },
                    outcome,
                );
                assert_eq!(rig.state.input_buffer, "saved draft");
                rig.open_thread("ri-1", ThreadAccess::Writer).await;
                rig.ch('d').await;
                assert_eq!(reply_text(&rig), Some("retained reply"));
                assert!(screen(&mut rig).contains("retained reply"));
                assert!(rig.frames().is_empty());
            }
        }

        #[tokio::test]
        async fn story21_8_attached_shift_text_and_readline_chords_reach_the_reply_field() {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            set_composer(&mut rig, "saved draft");
            rig.press(KeyCode::Char('R'), KeyModifiers::SHIFT).await;
            assert_eq!(reply_text(&rig), None);
            assert!(rig.frames().is_empty());
            rig.ch('r').await;
            for c in ['R', 'D', 'Y'] {
                rig.press(KeyCode::Char(c), KeyModifiers::SHIFT).await;
            }
            assert_eq!(reply_text(&rig), Some("RDY"));
            rig.type_text(" word").await;
            rig.ctrl('a').await;
            rig.ch('X').await;
            assert_eq!(reply_text(&rig), Some("XRDY word"));
            rig.press(KeyCode::End, KeyModifiers::NONE).await;
            rig.ctrl('w').await;
            assert_eq!(reply_text(&rig), Some("XRDY "));
            assert_eq!(rig.state.input_buffer, "saved draft");
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_read_only_never_opens_a_reply_field() {
            let mut control =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            control.ch('r').await;
            assert_eq!(reply_text(&control), Some(""));
            let mut rig =
                item_rig(true, SemanticMessageType::Consultation, None, false, false).await;
            set_composer(&mut rig, "saved draft");
            for key in ['r', 'd'] {
                rig.ch(key).await;
                assert_eq!(reply_text(&rig), None);
                assert!(rig.frames().is_empty());
            }
            assert_eq!(rig.state.input_buffer, "saved draft");
        }

        #[tokio::test]
        async fn story21_8_attached_c_copies_original_body_and_sends_no_frame() {
            let mut rig = Rig::at(FocusState::Chat, "saved draft");
            let journal = journal_of(&rig).await;
            seed_thread_item(
                &journal,
                &thread_peer(),
                "ri-copy",
                SemanticMessageType::Consultation,
                None,
                "original \x1b[31m body",
                None,
            )
            .await;
            drop(journal);
            rig.open_thread("ri-copy", ThreadAccess::Writer).await;
            let copied = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
            let sink = copied.clone();
            rig.state.clipboard_copier = Some(std::sync::Arc::new(move |text| {
                sink.lock().push(text.to_owned());
                crate::adapters::tui::clipboard::ClipboardResult::Osc52Success
            }));
            rig.ch('c').await;
            assert_eq!(&*copied.lock(), &["original \x1b[31m body"]);
            assert!(rig.frames().is_empty());
            assert_eq!(rig.state.input_buffer, "saved draft");
            rig.state.thread_inspector.as_mut().unwrap().original = None;
            rig.ch('c').await;
            assert_eq!(copied.lock().len(), 1);
            assert!(
                rig.state
                    .notifications
                    .records()
                    .any(|n| n.message.contains("Nothing to copy"))
            );
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_search_filter_attached_twin() {
            let mut rig = Rig::at(FocusState::Chat, "saved draft");
            let journal = journal_of(&rig).await;
            let peer = thread_peer();
            for (id, parent, body) in [
                ("root", None, "root canary"),
                ("middle", Some("root"), "unique needle"),
                ("other", Some("middle"), "discard canary"),
                ("opened", Some("other"), "opened canary"),
            ] {
                seed_thread_item(
                    &journal,
                    &peer,
                    id,
                    SemanticMessageType::Consultation,
                    parent,
                    body,
                    None,
                )
                .await;
            }
            drop(journal);
            rig.open_thread("opened", ThreadAccess::Writer).await;
            let unfiltered = screen(&mut rig);
            assert!(unfiltered.contains("[/] Search"), "{unfiltered}");
            let kept = |rig: &Rig, needle: &str| {
                crate::adapters::tui::thread_body::visible_segments(
                    rig.state.thread_inspector.as_ref().unwrap(),
                )
                .iter()
                .any(|segment| segment.text().contains(needle))
            };
            assert!(
                kept(&rig, "discard canary"),
                "positive control: nothing filtered yet"
            );
            rig.ch('/').await;
            rig.type_text("NEEDLE").await;
            decision_queue::reply::observe(&mut rig.state, "principal", "roster", "ri-arrived", 1);
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert!(rig.state.thread_inspector.as_ref().unwrap().search.editing);
            rig.enter().await;
            let filtered = screen(&mut rig);
            assert!(
                filtered.contains("unique needle")
                    && filtered.contains("root canary")
                    && filtered.contains("opened canary"),
                "{filtered}"
            );
            assert!(
                !kept(&rig, "discard canary"),
                "the non-matching turn is filtered out"
            );
            assert!(!filtered.contains("discard canary"), "{filtered}");
            assert_eq!(rig.state.input_buffer, "saved draft");
            assert!(rig.frames().is_empty());
            rig.esc().await;
            assert!(
                kept(&rig, "discard canary"),
                "the first Esc clears the filter"
            );
            let cleared = screen(&mut rig);
            assert!(
                cleared.contains("opened canary") && !cleared.contains("NEEDLE"),
                "{cleared}"
            );
            rig.ch('/').await;
            rig.type_text("not-present-anywhere").await;
            rig.enter().await;
            assert!(
                rig.state
                    .notifications
                    .records()
                    .any(|n| n.message.contains("Search: no match"))
            );
            assert!(kept(&rig, "discard canary"));
            let unchanged = screen(&mut rig);
            assert!(
                unchanged.contains("opened canary") && unchanged.contains("[c] Copy"),
                "{unchanged}"
            );
            assert!(
                !unchanged.contains("not-present-anywhere"),
                "the no-match query is cleared: {unchanged}"
            );
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_l_never_pages_inside_the_viewport() {
            use crate::adapters::tui::thread_body::ThreadSegment;
            let mut rig = long_thread_rig("saved draft").await;
            rig.ch('p').await;
            let before = rig
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .last_non_pinned_ordinal;
            assert!(rig.state.thread_inspector.as_ref().unwrap().omitted_after > 0);
            {
                let inspector = rig.state.thread_inspector.as_mut().unwrap();
                let body = inspector
                    .segments
                    .iter_mut()
                    .find_map(|segment| match segment {
                        ThreadSegment::Body(body) => Some(body),
                        _ => None,
                    })
                    .unwrap();
                *body = format!("```\n{}\n```", "0123456789".repeat(20));
                inspector.invalidate();
            }
            let painted = screen(&mut rig);
            assert!(
                rig.state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .wide_fit
                    .get()
                    .0
                    > 0,
                "{painted}"
            );
            rig.enter().await;
            assert!(rig.state.thread_inspector.as_ref().unwrap().viewport);
            let before_scroll = screen(&mut rig);
            rig.ch('l').await;
            rig.ch('l').await;
            let inspector = rig.state.thread_inspector.as_ref().unwrap();
            assert_eq!(inspector.last_non_pinned_ordinal, before);
            assert!(inspector.viewer.h_off > 0);
            let after_scroll = screen(&mut rig);
            assert_ne!(before_scroll, after_scroll, "the visible wide block moves");
            assert!(rig.frames().is_empty());
            rig.esc().await;
            rig.ch('l').await;
            assert_ne!(
                rig.state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .last_non_pinned_ordinal,
                before
            );
            assert_eq!(rig.state.input_buffer, "saved draft");
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_enter_not_painted_without_a_wide_block() {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            let painted = screen(&mut rig);
            assert!(!painted.contains("[Enter] Block"), "{painted}");
            rig.enter().await;
            assert!(!rig.state.thread_inspector.as_ref().unwrap().viewport);
            assert!(rig.frames().is_empty());
            rig.ch('r').await;
            assert_eq!(reply_text(&rig), Some(""));
        }

        #[tokio::test]
        async fn story21_8_attached_ack_outcome_lives_in_the_thread_and_survives_reopen() {
            let mut rig = item_rig(
                false,
                SemanticMessageType::ArchitectureUpdate,
                None,
                false,
                false,
            )
            .await;
            set_composer(&mut rig, "saved draft");
            rig.ch('a').await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::AcknowledgeRecipientItem { item_id }] if item_id == "ri-1")
            );
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                RecipientAction::Acknowledge,
                RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                RecipientActionOutcome::Applied { reply: None },
            );
            assert!(screen(&mut rig).contains("acknowledgement recorded"));
            rig.esc().await;
            rig.open_thread("ri-1", ThreadAccess::Writer).await;
            assert!(screen(&mut rig).contains("acknowledgement recorded"));
            assert_eq!(rig.state.input_buffer, "saved draft");
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_typed_team_ack_without_a_frame_posts_one_info_notice_and_no_feedback_block()
         {
            let mut rig = item_rig(
                false,
                SemanticMessageType::ArchitectureUpdate,
                None,
                false,
                false,
            )
            .await;
            rig.esc().await;
            let blocks = rig.state.feedback_blocks.len();
            let notices = rig.state.notifications.records().count();
            set_composer(&mut rig, "/team ack ri-1");
            rig.submit_composer().await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::AcknowledgeRecipientItem { item_id }] if item_id == "ri-1")
            );
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                RecipientAction::Acknowledge,
                RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                RecipientActionOutcome::Idempotent,
            );
            assert_eq!(rig.state.feedback_blocks.len(), blocks);
            let new: Vec<_> = rig.state.notifications.records().skip(notices).collect();
            assert_eq!(new.len(), 1);
            assert_eq!(new[0].severity, NoticeSeverity::Info);
            assert_eq!(new[0].source, NoticeSource::Team);
            assert_eq!(new[0].count, 1);
            assert!(new[0].action.is_none());
            assert!(new[0].message.contains("no new action applied"));
            assert!(rig.frames().is_empty());
        }

        #[test]
        fn story21_8_attached_outcome_log_evicts_the_oldest_target_whole_at_33() {
            let mut rig = Rig::new(false);
            for target in 0..32 {
                for line in 0..2 {
                    rig.state
                        .thread_outcomes
                        .push(&format!("item-{target}"), format!("line-{line}"));
                }
            }
            for line in 0..17 {
                rig.state
                    .thread_outcomes
                    .push("item-1", format!("extra-{line}"));
            }
            let lines: Vec<_> = rig.state.thread_outcomes.lines("item-1").collect();
            assert_eq!(lines.len(), 16);
            assert_eq!(lines[0], "extra-1");
            rig.state.thread_outcomes.push("item-32", "new".into());
            assert_eq!(rig.state.thread_outcomes.len(), 32);
            assert_eq!(rig.state.thread_outcomes.lines("item-0").count(), 0);
            assert_eq!(rig.state.thread_outcomes.lines("item-1").count(), 16);
            assert_eq!(
                rig.state
                    .thread_outcomes
                    .lines("item-32")
                    .collect::<Vec<_>>(),
                ["new"]
            );
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_context_inventory_stays_a_transcript_row() {
            let mut rig = Rig::at(FocusState::Input, "/team context");
            rig.submit_composer().await;
            assert!(matches!(
                rig.frames().as_slice(),
                [ClientFrame::InspectRecipientContext]
            ));
            crate::adapters::tui::handlers::recipient_actions::apply_context_state(
                &mut rig.state,
                vec!["ri-1".into()],
                "inventory canary".into(),
                true,
            );
            assert_eq!(rig.feedback("recipient-context"), Some("inventory canary"));
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_8_attached_deleted_block_ids_are_absent() {
            let mut rig = item_rig(
                false,
                SemanticMessageType::ArchitectureUpdate,
                None,
                false,
                false,
            )
            .await;
            rig.ch('a').await;
            assert_eq!(rig.frames().len(), 1);
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                RecipientAction::Acknowledge,
                RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                RecipientActionOutcome::Applied { reply: None },
            );
            assert!(screen(&mut rig).contains("acknowledgement recorded"));
            for id in ["recipient-actions", "recipient-view"] {
                assert!(rig.feedback(id).is_none(), "{id}");
            }
        }

        async fn reply_witness_rig(_: Key) -> Rig {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            set_composer(&mut rig, "draft");
            rig.ch('r').await;
            rig
        }

        fn reply_witness_digest(rig: &Rig) -> String {
            format!("reply={} {}", reply_text(rig).is_some(), open_surface(rig))
        }

        #[tokio::test]
        async fn story21_3_attached_witness_thread_reply() {
            let at = |reply, quit| {
                format!("Continue | reply={reply} quit={quit} focus=Chat input=draft")
            };
            let cases = [
                (Key::Enter, at(true, false)),
                (Key::Esc, at(false, false)),
                (Key::Ctrl('c'), at(false, false)),
                (Key::Ctrl('x'), at(true, false)),
                (Key::Ctrl('p'), at(true, false)),
                (Key::Ctrl('q'), at(true, true)),
            ];
            witness_rows(
                Context::ThreadReply,
                reply_witness_rig,
                reply_witness_digest,
                &cases,
            )
            .await;
            let mut rig = reply_witness_rig(Key::Char('R')).await;
            rig.press(KeyCode::Char('R'), KeyModifiers::SHIFT).await;
            rig.type_text(" word").await;
            rig.ctrl('w').await;
            assert_eq!(reply_text(&rig), Some("R "));
            assert_eq!(rig.state.input_buffer, "draft");
            assert!(rig.frames().is_empty());
        }

        async fn viewport_witness_rig(_: Key) -> Rig {
            use crate::adapters::tui::thread_body::ThreadSegment;
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            set_composer(&mut rig, "draft");
            let inspector = rig.state.thread_inspector.as_mut().unwrap();
            let body = inspector
                .segments
                .iter_mut()
                .find_map(|segment| match segment {
                    ThreadSegment::Body(body) => Some(body),
                    _ => None,
                })
                .unwrap();
            *body = format!("```\n{}\n```", "x".repeat(200));
            inspector.invalidate();
            screen(&mut rig);
            rig.enter().await;
            rig.state.thread_inspector.as_mut().unwrap().viewer.h_off = 10;
            rig
        }

        fn viewport_witness_digest(rig: &Rig) -> String {
            let inspector = rig.state.thread_inspector.as_ref().unwrap();
            format!(
                "viewport={} off={} {}",
                inspector.viewport,
                inspector.viewer.h_off,
                open_surface(rig)
            )
        }

        #[tokio::test]
        async fn story21_3_attached_witness_body_viewport() {
            let probe = viewport_witness_rig(Key::Esc).await;
            let (widest, cols) = probe
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .wide_fit
                .get();
            let maximum = widest.saturating_sub(cols);
            let step = crate::adapters::tui::text_geometry::TAB_STOP
                .min(cols.saturating_sub(2))
                .max(1);
            assert!(maximum > 10 + step);
            let at = |viewport, off, quit| {
                format!(
                    "Continue | viewport={viewport} off={off} quit={quit} focus=Chat input=draft"
                )
            };
            let own = [
                (Key::Esc, at(false, 0, false)),
                (Key::Ctrl('c'), at(false, 0, false)),
                (Key::Ctrl('x'), at(true, 10, false)),
                (Key::Ctrl('p'), at(true, 10, false)),
                (Key::Ctrl('q'), at(true, 10, true)),
            ];
            witness_rows(
                Context::BodyViewport,
                viewport_witness_rig,
                viewport_witness_digest,
                &own,
            )
            .await;
            let inherited = [
                (Key::Char('h'), at(true, 10 - step, false)),
                (Key::Char('l'), at(true, 10 + step, false)),
                (Key::Char('0'), at(true, 0, false)),
                (Key::Char('$'), at(true, maximum, false)),
                (Key::Char('v'), at(true, 10, false)),
            ];
            witness_rows(
                Context::MarkdownBody,
                viewport_witness_rig,
                viewport_witness_digest,
                &inherited,
            )
            .await;
        }

        #[tokio::test]
        async fn story21_8_attached_nonempty_reply_is_protected_by_quit_confirmation() {
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            assert!(rig.state.input_buffer.is_empty());
            rig.ch('r').await;
            assert!(!crate::adapters::tui::thread_inspector::reply_field_has_text(&rig.state));
            rig.type_text("unsent reply").await;
            assert!(crate::adapters::tui::thread_inspector::reply_field_has_text(&rig.state));
            rig.ctrl('q').await;
            assert_eq!(top_surface(&rig), Some(Surface::QuitConfirm));
            rig.ch('n').await;
            assert_eq!(top_surface(&rig), Some(Surface::ThreadInspector));
            assert_eq!(reply_text(&rig), Some("unsent reply"));
            assert!(rig.frames().is_empty());
        }

        /// 8B-03: a reply sent from the field and still awaiting its result protects quit.
        #[tokio::test]
        async fn story21_8_attached_pending_field_reply_is_protected_by_quit_confirmation() {
            let mut idle =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            idle.ctrl('q').await;
            assert_ne!(
                top_surface(&idle),
                Some(Surface::QuitConfirm),
                "positive control: an idle client with nothing pending quits freely"
            );
            let mut rig =
                item_rig(false, SemanticMessageType::Consultation, None, false, false).await;
            rig.ch('r').await;
            rig.type_text("awaiting result").await;
            rig.enter().await;
            assert!(matches!(
                rig.frames().as_slice(),
                [ClientFrame::ReplyToRecipientItem { .. }]
            ));
            assert!(rig.state.thread_inspector.is_none());
            assert!(rig.state.input_buffer.is_empty());
            rig.ctrl('q').await;
            assert_eq!(top_surface(&rig), Some(Surface::QuitConfirm));
        }
    }
}
