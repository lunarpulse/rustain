//! Attached host key front door (Story 21.2 Task 5: AC6 / AC8 / AC12 / AC13).
//!
//! [`handle_attached_key`] is the one place `run_attached` sends EVERY key
//! event. The shared presentation boundary owns S-QUIT, S-LOG and the minimum
//! terminal size before the legacy overlays, team-retract card, approvals,
//! proposals, recipient surface, peer card Tab and inline decision cards.
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
//! The attached composer is a plain `String` owned by `run_attached`. The
//! shared palette/popup code edits `TuiState::input_buffer`, so the composer is
//! *lent* to the state for exactly one shared call ([`with_lent_input`]) and
//! handed back — one owner at a time, never a per-draw copy.
//!
//! This is a child module of `attach_loop` (declared there with `#[path]`) so
//! it can reach that module's private team/approval/surface machinery without
//! widening its visibility.

use std::collections::VecDeque;
use std::path::Path;

use crate::adapters::tui::app::refuse_leader;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use super::{
    AttachedApprovals, AttachedTeamRequests, ComposerOutcome, PendingConsentApproval,
    SocketTurnDriver, attached_composer_key_with_team_requests, attached_display_input,
    attached_proposal_higher_confirmation, attached_proposal_key, attached_proposal_pending,
    attached_team_retract_key, dispatch_recipient_surface_key, open_response_surface,
    pending_peer_response, preview_line, recipient_surface_flash, retractable_rows,
    scroll_attached_transcript, system_message, take_current_peer_compose,
};
use crate::adapters::builtin_commands::{self, CommandHost};
use crate::adapters::command_registry::{CommandRegistry, CommandSource};
use crate::adapters::daemon::protocol::{ClientFrame, ProposalToken};
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
use crate::adapters::tui::keymap::text_edit;
use crate::adapters::tui::keymap::{
    self, AvailCtx, CommandId, Context, Host, Key, LeaderStep, Resolved, leader_step,
};
use crate::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
use crate::adapters::tui::palette_gate::PaletteGate;
use crate::adapters::tui::presentation::{self, RouteEffect, Surface};
use crate::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
use crate::adapters::tui::state::{PendingConsolidationCard, TabRenderState, TuiState};
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
pub(super) enum KeyFlow {
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
        input: &mut String,
        text: &str,
        card_pending: bool,
        workspace: &Path,
    ) {
        if handle_attached_paste(state, input, text, card_pending) {
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
    pub(super) input: &'a mut String,
    pub(super) frame_tx: &'a mpsc::UnboundedSender<ClientFrame>,
    pub(super) team_requests: &'a mut AttachedTeamRequests,
    pub(super) approvals: &'a mut AttachedApprovals,
    pub(super) consent: &'a mut VecDeque<PendingConsentApproval>,
    pub(super) consolidation_card: &'a mut Option<PendingConsolidationCard>,
    pub(super) consolidation_token: &'a mut Option<ProposalToken>,
    pub(super) retract_confirm: &'a mut Option<(String, String)>,
    pub(super) tab: &'a mut TabRenderState,
    pub(super) scroll_offset: &'a mut usize,
    pub(super) auto_scroll: &'a mut bool,
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

    /// A class-V view block (the peer-retract receipt): not a notice, so it
    /// keeps its `feedback_blocks` row until the next key.
    fn view_block(&mut self, id: &str, message: &str) {
        crate::adapters::tui::views::show_view(
            &mut self.state.feedback_blocks,
            crate::adapters::tui::views::ViewKind::PeerRetract,
            id,
            FeedbackLevel::Info,
            message,
        );
        self.state.needs_redraw = true;
    }

    /// A peer reply composition (or an open surface) owns the composer text.
    fn composer_locked(&self) -> bool {
        crate::adapters::tui::handlers::recipient_actions::pending_composition(self.state).is_some()
            || self.state.recipient_surface.is_some()
    }

    fn decision_cards_pending(&self) -> bool {
        !self.consent.is_empty() || self.consolidation_token.is_some()
    }
}

/// The attached decision card that owns `Esc` / `Ctrl+C` now: sender consent, the
/// consolidation card or the retract confirmation (A8; the permission approvals and the
/// team retract card answer in `attach_loop`).
fn attached_card_context(ctx: &KeyCtx<'_>) -> Option<Context> {
    if !ctx.consent.is_empty() {
        Some(Context::Consent)
    } else if ctx.consolidation_token.is_some() || ctx.retract_confirm.is_some() {
        Some(Context::Confirm)
    } else {
        None
    }
}

/// Lend the attached composer to the shared composer state for one call and
/// hand it back. The cursor is always at the end of the attached buffer.
fn with_lent_input<R>(
    state: &mut TuiState,
    input: &mut String,
    call: impl FnOnce(&mut TuiState) -> R,
) -> R {
    std::mem::swap(&mut state.input_buffer, input);
    state.cursor_position = state.input_buffer.chars().count();
    let result = call(state);
    std::mem::swap(&mut state.input_buffer, input);
    result
}

fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char(c)
}

/// The status-row hint for the attached client, generated from the registry
/// (Story 21.3 A10): the Detach leader row, `i` to type while Chat has focus
/// and `[^X Enter] Send` while multi-line. The `^X menu` tail is painted by the
/// status row itself, so it is not repeated here.
pub(super) fn status_hint(state: &TuiState) -> Option<String> {
    if !state.modal_stack.is_empty()
        || state.pending_leader.is_some()
        || state.recipient_surface.is_some()
    {
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

/// Borrowed paste edits an offered field before considering the base composer.
pub(super) fn handle_attached_paste(
    state: &mut TuiState,
    input: &mut String,
    text: &str,
    card_pending: bool,
) -> bool {
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
        && state.recipient_surface.is_none()
        && crate::adapters::tui::handlers::recipient_actions::pending_composition(state).is_none()
        && state.pending_team_retract.is_none()
    {
        input.push_str(text);
        state.autocomplete.dismiss();
        state.needs_redraw = true;
    }
    false
}

/// The attached key front door. Called by `run_attached` for every key event.
pub(super) async fn handle_attached_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    // Reconcile only an overlay-close transition, never a frame or deliberate `i`.
    let overlay_was_open = crate::adapters::tui::decision_pane::card_focus_blocked(ctx.state);
    let owner_before = (
        ctx.state.modal_stack.top().map(|frame| frame.id),
        ctx.state.pending_leader,
        crate::adapters::tui::app::focus_context(ctx.state),
    );
    let routed = route_event(ctx.state, &Event::Key(key));
    let flow = match routed {
        Some(RouteEffect::Consumed) => KeyFlow::Continue,
        Some(RouteEffect::QuitRequest) => request_quit(ctx),
        Some(RouteEffect::QuitConfirmed) => KeyFlow::Quit,
        Some(RouteEffect::Log(outcome)) => apply_log_outcome(ctx, outcome),
        Some(RouteEffect::OpenPalette) => {
            open_palette(ctx).await;
            KeyFlow::Continue
        }
        Some(RouteEffect::Frame(Context::Help)) => help_key(ctx, key).await,
        Some(RouteEffect::Frame(Context::CommandPalette)) => palette_key(ctx, key).await,
        Some(RouteEffect::Leader) => leader_key(ctx, key).await,
        Some(RouteEffect::Frame(context)) => unreachable!("attached frame context: {context:?}"),
        Some(RouteEffect::Action(_)) => unreachable!("local-only frame action in attached host"),
        // Guarded quit is universal, including cards that consume every key.
        None if command_in(Context::Global, overlay_key(&key))
            == Some(CommandId::Global(GlobalCmd::QuitRequest)) =>
        {
            request_quit(ctx)
        }
        None => handle_attached_key_inner(ctx, key).await,
    };
    if overlay_was_open
        && ctx.state.focus == FocusState::Input
        && !crate::adapters::tui::decision_pane::card_focus_blocked(ctx.state)
    {
        reconcile_pending_card_after_overlay_close(ctx);
    }
    super::settle_attached_card_focus(
        ctx.state,
        ctx.read_only,
        ctx.approvals,
        ctx.consent,
        ctx.consolidation_token,
        ctx.retract_confirm,
    );
    let owner_after = (
        ctx.state.modal_stack.top().map(|frame| frame.id),
        ctx.state.pending_leader,
        crate::adapters::tui::app::focus_context(ctx.state),
    );
    if owner_before != owner_after {
        ctx.state.current_hint = crate::adapters::tui::hints::contextual_hint(
            &ctx.state.focus,
            ctx.state.session_count,
            ctx.state.theme.timing.status_hint_fade_sessions,
            ctx.state.is_vscode,
            &ctx.state.modal_stack,
            ctx.state.pending_leader,
        );
    }
    flow
}

fn reconcile_pending_card_after_overlay_close(ctx: &mut KeyCtx<'_>) {
    if super::attached_card_pending(
        ctx.approvals,
        ctx.consent,
        ctx.consolidation_token,
        ctx.retract_confirm,
    ) {
        super::attached_card_arrived(ctx.state, ctx.input, ctx.read_only);
    }
}

async fn handle_attached_key_inner(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crossterm::event::KeyEventKind;

    if ctx.state.autocomplete.is_open()
        && ctx.state.focus == FocusState::Input
        && let Some(flow) = popup_key(ctx, key).await
    {
        return flow;
    }

    // Story 19.21c: every retract-card key before the composer.
    if attached_team_retract_key(ctx.state, ctx.team_requests, key, ctx.frame_tx) {
        return KeyFlow::Continue;
    }
    let approval_higher = attached_proposal_higher_confirmation(
        ctx.state,
        !ctx.consent.is_empty(),
        ctx.consolidation_card.is_some(),
        ctx.retract_confirm.is_some(),
    );
    if ctx.approvals.key(
        key,
        ctx.read_only || approval_higher || ctx.state.focus != FocusState::Chat,
        attached_display_input(ctx.state, ctx.input),
        ctx.frame_tx,
    ) {
        if std::mem::take(&mut ctx.approvals.cue) {
            ctx.info(crate::adapters::tui::keymap::render::card_cue(
                Context::Permission,
                Host::Attached,
            ));
        }
        ctx.state.needs_redraw = true;
        return KeyFlow::Continue;
    }
    let proposal_higher = approval_higher || (!ctx.read_only && !ctx.approvals.pending.is_empty());
    if attached_proposal_key(
        key,
        proposal_higher,
        ctx.read_only,
        ctx.input,
        ctx.state,
        ctx.tab,
        ctx.team_requests,
        ctx.frame_tx,
        ctx.scroll_offset,
        ctx.auto_scroll,
    ) {
        return KeyFlow::Continue;
    }
    let higher_confirmation = proposal_higher || attached_proposal_pending(ctx.state).is_some();
    if key.kind != KeyEventKind::Press
        && (ctx.state.recipient_surface.is_some() || higher_confirmation)
    {
        return KeyFlow::Continue;
    }
    if dispatch_recipient_surface_key(
        key,
        higher_confirmation,
        ctx.workspace,
        ctx.conversation,
        ctx.input,
        ctx.state,
        (!ctx.read_only).then_some(ctx.frame_tx),
    )
    .await
    {
        return KeyFlow::Continue;
    }
    if key.code == KeyCode::Tab
        && key.modifiers == KeyModifiers::NONE
        && !higher_confirmation
        && key.kind == KeyEventKind::Press
        && let Some(pending) = pending_peer_response(ctx.conversation)
    {
        let node = pending.node.to_owned();
        if let Err(error) = open_response_surface(
            ctx.workspace,
            ctx.conversation,
            &node,
            ctx.state,
            ctx.read_only,
            true,
        )
        .await
        {
            recipient_surface_flash(ctx.state, error.to_string());
        }
        return KeyFlow::Continue;
    }
    // Any further keypress dismisses the retract confirmation.
    ctx.state.feedback_blocks.remove("peer-retract");

    // `Esc` and `Ctrl+C` never decide a card (A8): one cue naming the keys that do. A card
    // answers only in Chat focus (AC17); in Input both keys keep their usual meaning.
    let chat = ctx.state.focus == FocusState::Chat;
    if chat
        && key.kind == KeyEventKind::Press
        && (key.code == KeyCode::Esc && key.modifiers.is_empty() || is_ctrl(&key, 'c'))
        && let Some(card) = attached_card_context(ctx)
    {
        ctx.info(crate::adapters::tui::keymap::render::card_cue(
            card,
            Host::Attached,
        ));
        ctx.state.needs_redraw = true;
        return KeyFlow::Continue;
    }

    match (key.code, key.modifiers) {
        // Story 18.3d AC1 — sender-consent Approval Pattern. The guard is the
        // daemon-derived pending FIFO, so these single-letter keys are inert
        // everywhere else.
        (KeyCode::Char('y'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && !ctx.consent.is_empty() =>
        {
            if let Some(pending) = ctx.consent.pop_front() {
                let _ = ctx.frame_tx.send(ClientFrame::ApprovalResponse {
                    request_id: pending.request_id,
                    outcome: crate::domain::models::ApprovalOutcome::Once,
                });
            }
            ctx.state.needs_redraw = true;
        }
        (KeyCode::Char('a'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && !ctx.consent.is_empty() =>
        {
            if let Some(pending) = ctx.consent.pop_front() {
                let _ = ctx.frame_tx.send(ClientFrame::ApprovalResponse {
                    request_id: pending.request_id,
                    outcome: crate::domain::models::ApprovalOutcome::AlwaysAndSave {
                        scope: crate::domain::models::ApprovalScope::Tool(pending.tool),
                    },
                });
            }
            ctx.state.needs_redraw = true;
        }
        (KeyCode::Char('n'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && !ctx.consent.is_empty() =>
        {
            if let Some(pending) = ctx.consent.pop_front() {
                let _ = ctx.frame_tx.send(ClientFrame::ApprovalResponse {
                    request_id: pending.request_id,
                    outcome: crate::domain::models::ApprovalOutcome::Reject { feedback: None },
                });
            }
            ctx.state.needs_redraw = true;
        }
        // Story 12.2d AC4/AC5 — consolidation card intercept.
        (KeyCode::Char('y'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && ctx.consolidation_token.is_some() =>
        {
            if let Some(token) = ctx.consolidation_token.take() {
                let _ = ctx.frame_tx.send(ClientFrame::ConsolidationResolve {
                    token,
                    accept: true,
                });
            }
            *ctx.consolidation_card = None;
            ctx.state.needs_redraw = true;
        }
        (KeyCode::Char('n'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && ctx.consolidation_token.is_some() =>
        {
            // n while the card is shown → decline.
            if let Some(token) = ctx.consolidation_token.take() {
                let _ = ctx.frame_tx.send(ClientFrame::ConsolidationResolve {
                    token,
                    accept: false,
                });
            }
            *ctx.consolidation_card = None;
            ctx.state.needs_redraw = true;
        }
        // Story 18.3c AC5 — the retract confirmation owns y/n/Esc while open
        // (Approval grammar, never a bare key on a transient surface). It is
        // opened only by the palette's retract route.
        (KeyCode::Char('y'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && ctx.retract_confirm.is_some() =>
        {
            if ctx.read_only {
                ctx.info("read-only — can't retract here");
            } else if let Some((message_id, _)) = ctx.retract_confirm.take() {
                // Stale-target gate: the row must still be retractable now.
                if retractable_rows(ctx.conversation).any(|row| row.id == message_id) {
                    let _ = ctx.frame_tx.send(ClientFrame::RetractAutoResponse {
                        message_id,
                        target_seq: None,
                    });
                    ctx.view_block(
                        "peer-retract",
                        "Retracted. Marked in your log — never deleted.\nWhat was already read, was read.",
                    );
                } else {
                    ctx.info("that message can no longer be retracted — nothing sent");
                }
            }
            ctx.state.needs_redraw = true;
        }
        (KeyCode::Char('n'), m)
            if !m.contains(KeyModifiers::CONTROL) && chat && ctx.retract_confirm.is_some() =>
        {
            *ctx.retract_confirm = None;
            ctx.state.needs_redraw = true;
        }
        _ => {
            let pressed = Key::from_crossterm(&key);
            // An open peer surface owns every printable key.
            if matches!(pressed, Key::Char(_)) && ctx.state.recipient_surface.is_some() {
                return KeyFlow::Continue;
            }
            let context = if ctx.state.focus == FocusState::Input {
                Context::Input
            } else {
                Context::Chat
            };
            return match command_in(context, pressed) {
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
            };
        }
    }
    KeyFlow::Continue
}

/// Edit the tail of the attached composer with a shared `text_edit` function.
fn edit_composer(ctx: &mut KeyCtx<'_>, edit: fn(&mut String, usize) -> usize) {
    if ctx.state.focus != FocusState::Input || ctx.composer_locked() {
        return;
    }
    let end = ctx.input.chars().count();
    edit(ctx.input, end);
    if ctx.input.is_empty() {
        ctx.state.pinned_peer_compose = None;
    }
    ctx.state.needs_redraw = true;
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
    let _ = attached_composer_key_with_team_requests(
        KeyCode::Char(c),
        ctx.read_only,
        ctx.input,
        ctx.state,
        ctx.frame_tx,
        ctx.team_requests,
    );
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
            ctx.state.focus = FocusState::Input;
            ctx.state.needs_redraw = true;
        }
        CommandId::Chat(ChatCmd::OpenHelp) => {
            presentation::reducer::open(ctx.state, Surface::Help);
        }
        // Esc never detaches, quits or submits: with no popup/overlay open it
        // moves focus between Input and Chat and preserves the unsent text.
        CommandId::Input(InputCmd::ReturnToChat) | CommandId::Chat(ChatCmd::FocusToggle) => {
            match ctx.state.focus {
                FocusState::Input => ctx.state.focus = FocusState::Chat,
                FocusState::Chat => ctx.state.focus = FocusState::Input,
                _ => {}
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
        CommandId::Input(InputCmd::DeleteBack) => {
            if ctx.state.recipient_surface.is_none() {
                let _ = attached_composer_key_with_team_requests(
                    KeyCode::Backspace,
                    ctx.read_only,
                    ctx.input,
                    ctx.state,
                    ctx.frame_tx,
                    ctx.team_requests,
                );
            }
        }
        // TextEntry (Story 21.3 AC11): the attached composer is append-only (its cursor
        // stays at the end), so Ctrl+W / Ctrl+U edit its tail and Ctrl+H is Backspace;
        // Ctrl+A / Ctrl+E / Ctrl+K are consumed no-ops.
        CommandId::Input(InputCmd::DeleteWordBack) => edit_composer(ctx, text_edit::kill_word_back),
        CommandId::Input(InputCmd::KillToLineStart) => {
            edit_composer(ctx, text_edit::kill_to_line_start);
        }
        CommandId::Input(InputCmd::OpenSlashPopup) => {
            if ctx.input.is_empty() && !ctx.composer_locked() {
                // The shared composer owns the slash trigger: it saves the pre-trigger
                // composer so Esc can restore it exactly.
                let event = crate::domain::events::DomainInputEvent::KeyPress('/');
                with_lent_input(ctx.state, ctx.input, |state| {
                    let _ = handle_input(state, &event);
                });
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
            let rows = ctx.state.viewport_height / if half { 2 } else { 1 };
            scroll_attached_transcript(
                earlier,
                rows,
                ctx.streaming.is_streaming,
                ctx.scroll_offset,
                ctx.auto_scroll,
            );
            ctx.state.needs_redraw |= half;
        }
        CommandId::Overlay(
            cmd @ (OverlayCmd::HelpDown
            | OverlayCmd::HelpUp
            | OverlayCmd::HelpBottom
            | OverlayCmd::HelpTop),
        ) => {
            let help = &mut ctx.state.help_overlay;
            help.scroll_offset = match cmd {
                OverlayCmd::HelpDown => help.scroll_offset.saturating_add(1),
                OverlayCmd::HelpUp => help.scroll_offset.saturating_sub(1),
                OverlayCmd::HelpBottom => usize::MAX / 2,
                _ => 0,
            };
            ctx.state.needs_redraw = true;
        }
        CommandId::Overlay(OverlayCmd::HelpClose) => {
            presentation::reducer::close(ctx.state, Surface::Help);
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
            with_lent_input(ctx.state, ctx.input, cancel_autocomplete_popup);
            ctx.chrome.sync_slash(ctx.state, ctx.workspace);
            ctx.state.needs_redraw = true;
        }
        CommandId::Overlay(
            OverlayCmd::AutoPrev
            | OverlayCmd::AutoNext
            | OverlayCmd::AutoAccept
            | OverlayCmd::AutoBackspace,
        ) => popup_delegate(ctx, key),
        // Ctrl+A / Ctrl+E / Ctrl+K have no cursor to move; local-only rows never resolve here.
        _ => {}
    }
    KeyFlow::Continue
}

/// Enter and its modified forms. Plain Enter is never mistaken for Ctrl+Enter.
async fn enter_key(ctx: &mut KeyCtx<'_>, command: InputCmd) -> KeyFlow {
    if ctx.state.recipient_surface.is_some() {
        return KeyFlow::Continue;
    }
    match command {
        InputCmd::InsertNewline => {
            if !ctx.composer_locked() {
                ctx.input.push('\n');
                ctx.state.needs_redraw = true;
            }
            KeyFlow::Continue
        }
        // Ctrl+Enter (where the terminal distinguishes it) sends.
        InputCmd::Send if ctx.input.is_empty() => KeyFlow::Continue,
        InputCmd::Send => submit_enter(ctx).await,
        _ if ctx.state.multiline_mode => {
            if !ctx.input.is_empty() && !ctx.composer_locked() {
                ctx.input.push('\n');
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
            notifications::error(
                ctx.state,
                NoticeSource::Attached,
                "attach socket closed — stop request not sent",
            );
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
    let facts = QuitFacts {
        host: QuitHost::Attached,
        streams: usize::from(ctx.streaming.is_streaming),
        live_tools: ctx.streaming.active_tool_calls.len(),
        agents: 0,
        queued_turns: 0,
        composer_draft: !attached_display_input(ctx.state, ctx.input).is_empty(),
        pending_images: 0,
        peer_compose: ctx.state.pinned_peer_compose.is_some()
            || crate::adapters::tui::handlers::recipient_actions::pending_composition(ctx.state)
                .is_some(),
        pending_decisions: ctx.consent.len()
            + usize::from(ctx.consolidation_token.is_some())
            + usize::from(attached_proposal_pending(ctx.state).is_some())
            + if ctx.read_only {
                0
            } else {
                ctx.approvals.pending.len()
            },
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
            run_team_log(ctx, &args).await;
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
            if ctx.approvals.pending.is_empty() {
                ctx.info("No decisions waiting in this client");
            } else {
                ctx.info(format!(
                    "Approval pending: clear the draft, {}then {} / {}",
                    if ctx.state.focus == FocusState::Input {
                        format!(
                            "{} to Chat, ",
                            crate::adapters::tui::keymap::render::key_text(InputCmd::ReturnToChat)
                        )
                    } else {
                        String::new()
                    },
                    crate::adapters::tui::keymap::render::rail(
                        Context::Permission,
                        crate::adapters::tui::keymap::table_permission::PermissionCmd::AllowOnce
                    ),
                    crate::adapters::tui::keymap::render::rail(
                        Context::Permission,
                        crate::adapters::tui::keymap::table_permission::PermissionCmd::Deny
                    ),
                ));
            }
        }
        LeaderCmd::Detach => return KeyFlow::Detach,
        // The shared boundary intercepts Ctrl+Q before dismissing a legacy menu.
        LeaderCmd::QuitRequest => return request_quit(ctx),
        // Portable multiline send: the same path as Enter, exactly once.
        LeaderCmd::Send if origin == Context::Input => {
            if !ctx.input.is_empty() {
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

async fn run_team_log(
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
    *ctx.auto_scroll = true;
}

// ── help overlay ─────────────────────────────────────────────────────────────

/// Help is passive: Esc / `?` / Ctrl+C close it and restore the origin (Input
/// text is untouched); scroll keys move it. Everything else is consumed.
async fn help_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    match command_in(Context::Help, overlay_key(&key)) {
        Some(command) => execute_attached(ctx, command, key).await,
        None => KeyFlow::Continue,
    }
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

/// Hand a popup key to the shared autocomplete handler (the composer is lent to it).
fn popup_delegate(ctx: &mut KeyCtx<'_>, key: KeyEvent) {
    if let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default()) {
        with_lent_input(ctx.state, ctx.input, |state| {
            let _ = handle_input(state, &event);
        });
    }
    ctx.chrome.sync_slash(ctx.state, ctx.workspace);
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

/// Hand a palette key to the shared palette handler (the composer is lent to
/// it) and run the entry it selected, if any.
async fn palette_delegate(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crate::adapters::tui::app::InputAction;
    let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default()) else {
        return KeyFlow::Continue;
    };
    let action = with_lent_input(ctx.state, ctx.input, |state| {
        crate::adapters::tui::app::with_card_focus_reconcile(state, |state| {
            crate::adapters::tui::app::dispatch_frame_key(
                state,
                Context::CommandPalette,
                Key::from_domain(&event).unwrap_or(Key::Other),
                Host::Attached,
            )
        })
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
        // Ask with the REAL composer: its unsent draft (and a pinned reply) is
        // exactly what the decision must count, and nothing is swapped or cleared.
        return request_quit(ctx);
    }
    if ctx.state.pinned_peer_compose.is_some() {
        ctx.info("finish or cancel your peer reply before running a command from the palette");
        return KeyFlow::Continue;
    }
    let text = match args {
        Some(args) => format!("/{name} {args}"),
        None => format!("/{name}"),
    };
    let draft = std::mem::replace(ctx.input, text);
    let flow = submit_enter(ctx).await;
    *ctx.input = draft;
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
        Some(target) => {
            *ctx.retract_confirm = Some(target);
            // FR-21-3-c: the operator opened it; the confirm answers only in Chat focus.
            crate::adapters::tui::decision_pane::take_chat_focus_for_card(
                &mut ctx.state.focus,
                &mut ctx.state.card_focus_borrowed_from,
                !ctx.read_only,
                false,
            );
            ctx.state.focus = FocusState::Chat;
        }
        None => ctx.info("that message can no longer be retracted — nothing sent"),
    }
    ctx.state.needs_redraw = true;
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
        // leave every OTHER unsent fact (a pinned peer reply) untouched so the S-QUIT
        // decision still counts it, and so cancelling loses nothing (RU-14, AC7).
        ctx.input.clear();
        ctx.state.needs_redraw = true;
        return request_quit(ctx);
    }
    // A command is consumed like the existing refusals: the draft is cleared.
    ctx.input.clear();
    ctx.state.pinned_peer_compose = None;
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
    if let Some(local) = local_slash(ctx.input) {
        return run_local_slash(ctx, local);
    }
    let text = match attached_composer_key_with_team_requests(
        KeyCode::Enter,
        ctx.read_only,
        ctx.input,
        ctx.state,
        ctx.frame_tx,
        ctx.team_requests,
    ) {
        ComposerOutcome::Handled => return KeyFlow::Continue,
        ComposerOutcome::FrameSent => {
            *ctx.auto_scroll = true;
            return KeyFlow::Continue;
        }
        // Story 19.16g — a user-initiated log may scroll its own result into view.
        ComposerOutcome::LocalLog(args) => {
            run_team_log(ctx, &args).await;
            return KeyFlow::Continue;
        }
        ComposerOutcome::LocalView(item_id) => {
            let access = if ctx.read_only {
                crate::adapters::tui::state::RecipientSurfaceAccess::ReadOnly
            } else {
                crate::adapters::tui::state::RecipientSurfaceAccess::Writer
            };
            if let Err(error) =
                crate::infrastructure::runtime::recipient_surface::open_received_surface(
                    ctx.workspace,
                    Some(ctx.conversation),
                    &item_id,
                    ctx.state,
                    access,
                    crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
                )
                .await
            {
                recipient_surface_flash(ctx.state, error.to_string());
            }
            return KeyFlow::Continue;
        }
        ComposerOutcome::Submit(text) => text,
    };
    let compose = match take_current_peer_compose(ctx.state, ctx.conversation) {
        Ok(compose) => compose,
        Err(()) => {
            *ctx.input = text;
            recipient_surface_flash(
                ctx.state,
                "response already settled — your text was not sent".to_owned(),
            );
            return KeyFlow::Continue;
        }
    };
    if let Some(compose) = compose {
        ctx.input.clone_from(&text);
        let action = match compose.action {
            crate::adapters::tui::state::PeerComposeAction::Respond => {
                crate::adapters::daemon::protocol::PeerDraftAction::WriteOwn { content: text }
            }
            crate::adapters::tui::state::PeerComposeAction::Edit => {
                crate::adapters::daemon::protocol::PeerDraftAction::Edit { content: text }
            }
        };
        let node = compose.node_id.clone();
        ctx.state.pinned_peer_compose = Some(compose);
        let frame = ClientFrame::ResolvePeerDraft { node, action };
        super::queue_recipient_action(ctx.state, ctx.input, frame, ctx.frame_tx, true);
    } else {
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
        *ctx.auto_scroll = true;
    }
    KeyFlow::Continue
}

#[cfg(test)]
mod tests {
    use super::super::{apply_attached_approval_frame, attached_card_arrived};
    use super::*;
    use crate::adapters::daemon::protocol::{DaemonFrame, InterruptRefusal, TurnGeneration};
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
    struct Rig {
        workspace: tempfile::TempDir,
        read_only: bool,
        state: TuiState,
        conversation: Conversation,
        streaming: StreamingState,
        input: String,
        tx: mpsc::UnboundedSender<ClientFrame>,
        rx: mpsc::UnboundedReceiver<ClientFrame>,
        team_requests: AttachedTeamRequests,
        approvals: AttachedApprovals,
        consent: VecDeque<PendingConsentApproval>,
        consolidation_card: Option<PendingConsolidationCard>,
        consolidation_token: Option<ProposalToken>,
        retract_confirm: Option<(String, String)>,
        tab: TabRenderState,
        scroll_offset: usize,
        auto_scroll: bool,
        interrupt: AttachedInterrupt,
        driver: SocketTurnDriver,
        lifecycle: TuiTurnLifecycle,
        session_manager: SessionManager,
        log_service: crate::infrastructure::transparency::TransparencyService,
        chrome: AttachedChrome,
    }

    impl Rig {
        fn new(read_only: bool) -> Self {
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
            state.focus = FocusState::Input;
            Self {
                workspace,
                read_only,
                state,
                conversation: Conversation::default(),
                streaming: StreamingState::default(),
                input: String::new(),
                driver: SocketTurnDriver::new(tx.clone()),
                tx,
                rx,
                team_requests: AttachedTeamRequests::default(),
                approvals: AttachedApprovals::default(),
                consent: VecDeque::new(),
                consolidation_card: None,
                consolidation_token: None,
                retract_confirm: None,
                tab: TabRenderState::default(),
                scroll_offset: 0,
                auto_scroll: true,
                interrupt: AttachedInterrupt::new(read_only),
                lifecycle: TuiTurnLifecycle::default(),
                session_manager: SessionManager::new(SessionState::Empty),
                log_service,
                chrome: AttachedChrome::new(),
            }
        }

        async fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) -> KeyFlow {
            let mut ctx = KeyCtx {
                workspace: self.workspace.path(),
                read_only: self.read_only,
                state: &mut self.state,
                conversation: &mut self.conversation,
                streaming: &mut self.streaming,
                input: &mut self.input,
                frame_tx: &self.tx,
                team_requests: &mut self.team_requests,
                approvals: &mut self.approvals,
                consent: &mut self.consent,
                consolidation_card: &mut self.consolidation_card,
                consolidation_token: &mut self.consolidation_token,
                retract_confirm: &mut self.retract_confirm,
                tab: &mut self.tab,
                scroll_offset: &mut self.scroll_offset,
                auto_scroll: &mut self.auto_scroll,
                interrupt: &self.interrupt,
                driver: &self.driver,
                lifecycle: &mut self.lifecycle,
                session_manager: &mut self.session_manager,
                log_service: &self.log_service,
                chrome: &mut self.chrome,
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                handle_attached_key(&mut ctx, KeyEvent::new(code, modifiers)),
            )
            .await
            .expect("attached key deadline")
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

        async fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                assert_eq!(self.ch(c).await, KeyFlow::Continue, "typing {c:?}");
            }
        }

        fn frames(&mut self) -> Vec<ClientFrame> {
            let mut frames = Vec::new();
            while let Ok(frame) = self.rx.try_recv() {
                frames.push(frame);
            }
            frames
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
            self.conversation.messages.push(row);
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
            ('a', NoticeSource::Chord, "/team status"),
            ('s', NoticeSource::Chord, "/team status"),
            ('r', NoticeSource::Chord, "/team status"),
            ('e', NoticeSource::Chord, "/team status"),
            ('t', NoticeSource::Chord, "/team status"),
            ('u', NoticeSource::Chord, "/team status"),
            ('w', NoticeSource::Chord, "single layout"),
            ('h', NoticeSource::Chord, "/team status"),
        ];
        for (key, source, remedy) in unavailable {
            let mut rig = Rig::new(false);
            rig.type_text("draft").await;
            assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
            assert!(rig.state.pending_leader.is_some(), "{key}: leader open");
            assert_eq!(rig.ch(key).await, KeyFlow::Continue, "{key}");
            assert!(rig.state.pending_leader.is_none(), "{key}: leader consumed");
            assert_eq!(rig.state.focus, FocusState::Input, "{key}: origin restored");
            assert_eq!(rig.input, "draft", "{key}: draft untouched");
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
        // `/team log` paints its view as the stable `team-log` feedback block.
        let log_view = |rig: &Rig| rig.feedback("team-log").map(str::to_owned);
        assert!(log_view(&leader).is_some(), "`l` produced the log read");
        assert_eq!(log_view(&leader), log_view(&typed));
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
        assert_eq!(rig.input, "keep me");
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
        assert!(rig.input.is_empty());
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
        assert_eq!(rig.input, "keep me");
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
        assert_eq!(rig.input, "keep");

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
        assert_eq!(rig.input, "unsent");
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert_eq!(rig.state.focus, FocusState::Input);
        // Ctrl+D is inert in Input.
        assert_eq!(rig.ctrl('d').await, KeyFlow::Continue);
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.input, "unsent");
        // Ctrl+D in Chat is a half-page scroll toward newer rows.
        rig.esc().await;
        rig.state.viewport_height = 20;
        rig.scroll_offset = 12;
        rig.auto_scroll = false;
        assert_eq!(rig.ctrl('d').await, KeyFlow::Continue);
        assert_eq!(rig.scroll_offset, 7);
        // Ctrl+C with a draft asks first; it never detaches and keeps the draft.
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(rig.state.quit_confirm.is_some());
        assert_eq!(rig.input, "unsent");
        assert!(rig.frames().is_empty(), "no frame: nothing is detached");
    }

    #[tokio::test]
    async fn story21_2_attached_q_is_text_in_input_and_a_quit_request_in_chat() {
        let mut rig = Rig::new(false);
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        assert_eq!(rig.input, "q", "q in Input is plain text");
        assert!(rig.state.quit_confirm.is_none());
        // Chat: with a draft the guard opens; the draft survives.
        rig.esc().await;
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        assert!(rig.state.quit_confirm.is_some());
        assert_eq!(rig.input, "q");
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
        rig.consent.push_back(PendingConsentApproval {
            request_id: crate::domain::models::RequestId::new(),
            tool: "a2a/sender-consent".to_owned(),
            input_preview: "consent".to_owned(),
        });
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
        assert_eq!(rig.input, "draft");
        assert_eq!(rig.consent.len(), 1);
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
            assert_eq!(rig.input, "draft");
            assert_eq!(rig.consent.len(), 1, "the card was never answered");
            assert!(rig.frames().is_empty());
            rig.ctrl('q').await;
        }
        // Explicit `y` is the only exit, and it answers no card.
        assert_eq!(rig.ch('y').await, KeyFlow::Quit);
        assert!(rig.state.quit_confirm.is_none());
        assert_eq!(rig.consent.len(), 1);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_2_attached_ctrl_c_interrupts_quits_idle_or_refuses_read_only() {
        // A projected live turn: Ctrl+C sends the interrupt and stays attached.
        let mut rig = Rig::new(false);
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.streaming.is_streaming = true;
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
        viewer.streaming.is_streaming = true;
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
        assert!(viewer.conversation.messages.is_empty());
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
        assert!(rig.input.is_empty(), "Alt+M keeps its modifier");
        rig.type_text("a").await;
        assert_eq!(rig.ctrl('e').await, KeyFlow::Continue);
        assert!(rig.state.multiline_mode, "Ctrl+E is end-of-line, no toggle");
        assert_eq!(rig.input, "a");
        // Plain Enter in multiline mode inserts a newline and never sends.
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert_eq!(rig.input, "a\n");
        assert!(rig.frames().is_empty());
        rig.type_text("b").await;
        // Esc never submits, even with multiline text.
        rig.esc().await;
        rig.esc().await;
        assert!(rig.frames().is_empty());
        assert_eq!(rig.input, "a\nb");
        // Ctrl+Enter sends exactly once through the submit path.
        assert_eq!(
            rig.press(KeyCode::Enter, KeyModifiers::CONTROL).await,
            KeyFlow::Continue
        );
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("a\nb"));
        assert!(rig.input.is_empty());
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
        assert_eq!(rig.input, "x\n\n");
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
        assert!(rig.input.is_empty());

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
            assert!(rig.input.is_empty(), "the refused command is consumed");
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
                rig.input = ["retained draft"; 8].join("\n");
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                let draft = rig.input.clone();
                let allocation = (rig.input.as_ptr(), rig.input.capacity());
                let stage = (rig.scroll_offset, rig.auto_scroll);
                rig.ctrl('p').await;
                let frame = rig.state.modal_stack.top().unwrap().clone();
                let all = rig.palette_names();
                assert!(all.len() > 1 && all.iter().any(|name| name == "/ml"));
                let refreshes = rig.chrome.gate.refresh_count();
                rig.chrome
                    .paste(
                        &mut rig.state,
                        &mut rig.input,
                        "/ml",
                        false,
                        rig.workspace.path(),
                    )
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
                    .paste(
                        &mut rig.state,
                        &mut rig.input,
                        "界",
                        false,
                        rig.workspace.path(),
                    )
                    .await;
                assert_eq!(rig.state.command_palette.filter_text.as_str(), "/界ml");
                assert!(
                    rig.palette_names().is_empty(),
                    "pasted query updates the rows, not just text"
                );
                rig.key(Key::Left).await;
                let snapshot = rig.state.command_palette.filter_text.clone();
                let caret_count = |rig: &Rig| {
                    paint_buffer_at(&rig.state, width, height, &rig.input)
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
                    .paste(
                        &mut rig.state,
                        &mut rig.input,
                        "blocked",
                        false,
                        rig.workspace.path(),
                    )
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
                assert_eq!(rig.input, draft);
                assert_eq!((rig.input.as_ptr(), rig.input.capacity()), allocation);
                assert_eq!((rig.scroll_offset, rig.auto_scroll), stage);
                assert!(
                    rig.frames().is_empty(),
                    "no pasted key commands or submissions"
                );
            }
        }
    }

    #[tokio::test]
    async fn story21_4_bb2a_attached_every_printable_inserts_at_the_palette_caret() {
        for read_only in [false, true] {
            let mut rig = Rig::new(read_only);
            rig.input = "draft".into();
            rig.ctrl('p').await;
            for ch in (0x20u8..0x7f).map(char::from) {
                rig.chrome
                    .paste(
                        &mut rig.state,
                        &mut rig.input,
                        "ab",
                        false,
                        rig.workspace.path(),
                    )
                    .await;
                rig.key(Key::Home).await;
                rig.key(Key::Right).await;
                let frame = rig.state.modal_stack.top().unwrap().clone();
                rig.ch(ch).await;
                assert_eq!(
                    rig.state.command_palette.filter_text.as_str(),
                    format!("a{ch}b"),
                    "{read_only}: {ch:?}"
                );
                assert_eq!(rig.state.modal_stack.top(), Some(&frame));
                assert_eq!(rig.input, "draft");
                assert!(rig.frames().is_empty());
                rig.key(Key::Home).await;
                rig.ctrl('k').await;
                assert!(rig.state.command_palette.filter_text.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn story21_4_bb2a_attached_readline_keys_edit_graphemes_at_the_caret() {
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
        for read_only in [false, true] {
            for &(edit, seed, prefix, expected, cursor) in cases {
                let mut rig = Rig::new(read_only);
                rig.input = "untouched draft".into();
                rig.ctrl('p').await;
                let frame = rig.state.modal_stack.top().unwrap().clone();
                let stage = (rig.scroll_offset, rig.auto_scroll);
                rig.chrome
                    .paste(
                        &mut rig.state,
                        &mut rig.input,
                        seed,
                        false,
                        rig.workspace.path(),
                    )
                    .await;
                assert_eq!(
                    rig.state.command_palette.filter_text.as_str(),
                    seed,
                    "actual paste control"
                );
                for &key in prefix {
                    rig.key(key).await;
                }
                assert_eq!(rig.key(edit).await, KeyFlow::Continue);
                assert_eq!(
                    rig.state.command_palette.filter_text.as_str(),
                    expected,
                    "{read_only}: {edit:?} {seed}"
                );
                assert_eq!(
                    rig.state.command_palette.filter_text.cursor(),
                    cursor,
                    "{read_only}: {edit:?} {seed}"
                );
                assert_eq!(rig.state.modal_stack.top(), Some(&frame));
                assert_eq!((rig.scroll_offset, rig.auto_scroll), stage);
                assert_eq!(rig.input, "untouched draft");
                assert!(rig.frames().is_empty());
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
        assert_eq!(rig.input, "half-written", "the draft is never the command");
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
        assert_eq!(quitting.input, "half written", "the draft survives the ask");
        assert!(quitting.frames().is_empty());

        rig.ctrl('p').await;
        rig.type_text("detach").await;
        assert_eq!(rig.enter().await, KeyFlow::Detach);
        assert!(rig.frames().is_empty());
    }

    /// B-2: a typed `/quit` over a pinned peer reply asks first and keeps the reply.
    #[tokio::test]
    async fn story21_2_attached_typed_quit_counts_a_pinned_peer_reply() {
        use crate::adapters::tui::state::{PeerComposeAction, PinnedPeerCompose};
        let mut rig = Rig::new(false);
        rig.state.pinned_peer_compose = Some(PinnedPeerCompose::new(
            "node-a".to_owned(),
            PeerComposeAction::Respond,
        ));
        rig.type_text("/quit").await;
        rig.state.autocomplete.dismiss();
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert!(
            rig.state.quit_confirm.is_some(),
            "a pinned reply is unsent work"
        );
        assert!(
            rig.state.pinned_peer_compose.is_some(),
            "asking never drops the pinned reply"
        );
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_2_attached_palette_retract_selects_a_real_row_and_keeps_every_gate() {
        let mut rig = Rig::new(false);
        rig.retractable("peer-response-node-a", "first auto reply");
        rig.retractable("peer-response-node-b", "second auto reply");
        // Ctrl+X is the leader now — never an immediate retract.
        rig.ctrl('x').await;
        assert!(rig.state.pending_leader.is_some());
        assert!(rig.retract_confirm.is_none());
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
        assert_eq!(
            rig.retract_confirm.as_ref().map(|(id, _)| id.as_str()),
            Some("peer-response-node-a")
        );
        assert!(rig.frames().is_empty(), "no immediate effect");
        // `n` declines; reopen and confirm with `y`.
        assert_eq!(
            rig.state.focus,
            FocusState::Chat,
            "the palette route hands the confirm Chat focus, where it answers (AC17)"
        );
        rig.ch('n').await;
        assert!(rig.retract_confirm.is_none() && rig.frames().is_empty());
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
        rig.ch('y').await;
        let frames = rig.frames();
        assert!(matches!(
            frames.as_slice(),
            [ClientFrame::RetractAutoResponse { message_id, target_seq: None }]
                if message_id == "peer-response-node-a"
        ));

        // Stale target: the row was retracted meanwhile — `y` sends nothing. The confirm
        // opens through the real palette route, which hands it Chat focus (FR-21-3-c).
        let mut rig = Rig::new(false);
        rig.retractable("peer-response-node-a", "auto reply");
        assert_eq!(rig.state.focus, FocusState::Input);
        rig.input = "saved retract draft".into();
        rig.ctrl('p').await;
        rig.type_text("retract").await;
        rig.enter().await;
        assert!(rig.retract_confirm.is_some());
        assert_eq!(rig.state.focus, FocusState::Chat);
        rig.ch('i').await;
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.input, "saved retract draft");
        rig.esc().await;
        rig.conversation.messages[0].retracted_at_ms = Some(1);
        rig.ch('y').await;
        assert!(rig.frames().is_empty(), "stale target never retracts");
        assert!(rig.retract_confirm.is_none());

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
        assert!(rig.retract_confirm.is_none());
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
        assert_eq!(rig.input, "/");
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
        assert_eq!(rig.input, "");
        assert_eq!(
            rig.state.focus,
            FocusState::Input,
            "Esc closed only the popup"
        );

        // Tab completes; the completed command then runs through the router.
        rig.type_text("/cl").await;
        rig.press(KeyCode::Tab, KeyModifiers::NONE).await;
        assert_eq!(rig.input, "/clear");
        assert!(!rig.state.autocomplete.is_open());
        rig.enter().await;
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("/clear"));

        // A slash after other text is plain text — no popup.
        rig.type_text("a/").await;
        assert!(!rig.state.autocomplete.is_open());
        assert_eq!(rig.input, "a/");
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

    // ── the pre-existing owners keep their keys ──────────────────────────────

    /// 21-3 3-C (AC16, AC17; was "keeps its keys and Ctrl+C declines it"): a card answers only
    /// in Chat focus, and `Esc` / `Ctrl+C` post one cue instead of declining it.
    #[tokio::test]
    async fn story21_3_attached_consent_and_retract_cards_answer_in_chat_and_never_on_esc() {
        fn consent() -> PendingConsentApproval {
            PendingConsentApproval {
                request_id: crate::domain::models::RequestId::new(),
                tool: "a2a/sender-consent".to_owned(),
                input_preview: "consent".to_owned(),
            }
        }
        let mut rig = Rig::new(false);
        rig.consent.push_back(consent());
        // Input focus: `y` is text and the card stays pending.
        rig.ch('y').await;
        assert!(rig.frames().is_empty() && rig.consent.len() == 1);
        assert_eq!(rig.input, "y");
        rig.input.clear();
        attached_card_arrived(&mut rig.state, &rig.input, false);
        assert_eq!(rig.state.focus, FocusState::Chat);
        let cue = crate::adapters::tui::keymap::render::card_cue(Context::Consent, Host::Attached);
        for pressed_ctrl_c in [false, true] {
            if pressed_ctrl_c {
                rig.ctrl('c').await;
            } else {
                rig.esc().await;
            }
            assert!(
                rig.frames().is_empty() && rig.consent.len() == 1,
                "no decision"
            );
            assert_eq!(rig.notice(NoticeSource::Attached), Some(cue.as_str()));
        }
        assert!(
            rig.state.quit_confirm.is_none(),
            "Ctrl+C is never a quit here"
        );
        // Positive control: the explicit keys decide, once each.
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse {
                outcome: crate::domain::models::ApprovalOutcome::Once,
                ..
            }]
        ));
        assert_eq!(
            rig.state.focus,
            FocusState::Input,
            "consent returns borrowed focus"
        );
        rig.retained_arrives(false);
        rig.ch('n').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse {
                outcome: crate::domain::models::ApprovalOutcome::Reject { .. },
                ..
            }]
        ));
        // The retract confirmation: same grammar, same cue.
        rig.retract_confirm = Some(("peer-response-node-a".to_owned(), "first".to_owned()));
        assert_eq!(rig.state.focus, FocusState::Input);
        attached_card_arrived(&mut rig.state, &rig.input, false);
        rig.esc().await;
        assert!(rig.retract_confirm.is_some() && rig.frames().is_empty());
        assert_eq!(
            rig.notice(NoticeSource::Attached),
            Some(
                crate::adapters::tui::keymap::render::card_cue(Context::Confirm, Host::Attached)
                    .as_str()
            )
        );
        rig.ch('n').await;
        assert!(rig.retract_confirm.is_none());
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
                apply_attached_approval_frame(
                    &mut self.state,
                    &mut self.approvals,
                    frame,
                    &self.input,
                    self.read_only,
                );
            }
            self.paint_approvals();
        }

        /// The loop's own paint rule: a writer in Chat or Input with an empty composer.
        fn paint_approvals(&mut self) {
            let paints = !self.read_only
                && matches!(self.state.focus, FocusState::Chat | FocusState::Input)
                && attached_display_input(&self.state, &self.input).is_empty();
            let theme = crate::adapters::tui::theme::Theme::dark();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            let mut painted = None;
            terminal
                .draw(|f| {
                    if paints {
                        painted = self.approvals.paint(f, f.area(), &theme);
                    }
                })
                .unwrap();
            self.approvals.painted = painted;
        }
    }

    impl Rig {
        fn retained_arrives(&mut self, consolidation: bool) {
            if consolidation {
                super::super::apply_attached_consolidation_frame(
                    &mut self.state,
                    &mut self.consolidation_card,
                    &mut self.consolidation_token,
                    DaemonFrame::ConsolidationProposed {
                        token: ProposalToken(7),
                        proposals: vec![],
                    },
                    &self.input,
                    self.read_only,
                );
            } else {
                super::super::apply_attached_consent_frame(
                    &mut self.state,
                    &mut self.consent,
                    DaemonFrame::ApprovalRequest {
                        request_id: crate::domain::models::RequestId::new(),
                        tool: "a2a/sender-consent".into(),
                        source: crate::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                            conversation_id: "c".into(),
                        },
                        risk: crate::domain::models::ToolRisk::Standard,
                        summary: "consent".into(),
                        input_bytes: None,
                    },
                    &self.input,
                    self.read_only,
                );
            }
        }
    }

    async fn retained_attached_arrival(consolidation: bool) {
        for (focus, draft, help) in [
            (FocusState::Input, "", false),
            (FocusState::Input, "draft\n界", false),
            (FocusState::Input, "", true),
        ] {
            let mut rig = Rig::new(false);
            rig.state.focus = focus.clone();
            rig.input = draft.into();
            if help {
                rig.ctrl('x').await;
                rig.ch('?').await;
            }
            rig.retained_arrives(consolidation);
            let takes = !help && focus == FocusState::Input && draft.is_empty();
            assert_eq!(
                rig.state.focus,
                if takes {
                    FocusState::Chat
                } else {
                    focus.clone()
                }
            );
            assert_eq!(rig.input, draft);
            assert!(rig.frames().is_empty());
            if takes {
                rig.ch('i').await;
                assert_eq!(rig.state.focus, FocusState::Input);
                rig.retained_arrives(consolidation);
                assert_eq!(
                    rig.state.focus,
                    FocusState::Chat,
                    "burst retakes empty Input"
                );
            } else if !help && focus == FocusState::Input {
                rig.esc().await;
                rig.ch('i').await;
                assert_eq!(rig.state.focus, FocusState::Input);
                assert_eq!(rig.input, draft);
            }
            assert_eq!(rig.consolidation_token.is_some(), consolidation);
            assert_eq!(rig.consent.is_empty(), consolidation);
        }
    }

    #[tokio::test]
    async fn story21_3_retained_card_arrivals_preserve_focus_and_drafts_attached_consent() {
        retained_attached_arrival(false).await;
    }

    #[tokio::test]
    async fn story21_3_retained_card_arrivals_preserve_focus_and_drafts_attached_consolidation() {
        retained_attached_arrival(true).await;
    }

    #[tokio::test]
    async fn story21_3_borrowed_consent_focus_restores_composition() {
        let mut rig = Rig::new(false);
        rig.retained_arrives(false);
        assert_eq!(rig.state.focus, FocusState::Chat);
        rig.ch('y').await;
        assert_eq!(rig.state.focus, FocusState::Input);
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse { .. }]
        ));
        let pending = super::super::attached_card_pending(
            &rig.approvals,
            &rig.consent,
            &rig.consolidation_token,
            &rig.retract_confirm,
        );
        super::handle_attached_paste(
            &mut rig.state,
            &mut rig.input,
            "first line\nsecond line",
            pending,
        );
        rig.enter().await;
        assert_eq!(
            rig.frames().iter().find_map(user_text),
            Some("first line\nsecond line")
        );
        // Tab saves this restored focus, not the card's Chat.
        let mut row = system_message("pending response");
        row.id = "peer".into();
        rig.conversation.messages.push(row);
        rig.conversation.peer_row_controls.insert(
            "peer".into(),
            crate::domain::models::PeerRowControl {
                node_id: "node".into(),
                item_address: None,
                stories: None,
                status: None,
                selected_for: None,
                retro: None,
                message_type: crate::domain::models::SemanticMessageType::Unknown,
                sender_label: "sender".into(),
                stage: crate::domain::models::PeerRowStage::AwaitingOperator,
                node_resolvable: true,
            },
        );
        rig.press(KeyCode::Tab, KeyModifiers::NONE).await;
        assert!(rig.state.recipient_surface.is_some());
        rig.esc().await;
        assert_eq!(rig.state.focus, FocusState::Input);
    }

    #[tokio::test]
    async fn story21_3_borrowed_focus_respects_explicit_user_control() {
        let mut rig = Rig::new(false);
        rig.retained_arrives(false);
        rig.ch('i').await;
        assert_eq!(rig.state.focus, FocusState::Input);
        rig.esc().await;
        assert_eq!(rig.state.focus, FocusState::Chat);
        rig.ch('y').await;
        assert_eq!(rig.state.focus, FocusState::Chat);
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse { .. }]
        ));
    }

    #[tokio::test]
    async fn story21_3_borrowed_focus_waits_for_last_attached_card() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("queued");
        rig.retained_arrives(false);
        rig.ch('y').await;
        assert_eq!(rig.state.focus, FocusState::Chat);
        assert_eq!(rig.approvals.pending.len(), 1);
        rig.paint_approvals();
        rig.ch('y').await;
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.frames().len(), 2);
    }

    #[tokio::test]
    async fn story21_3_borrowed_focus_settles_after_frame_without_key() {
        let mut rig = Rig::new(false);
        rig.approval_arrives("withdrawn");
        apply_attached_approval_frame(
            &mut rig.state,
            &mut rig.approvals,
            DaemonFrame::ApprovalClosed {
                request_id: crate::domain::models::RequestId("withdrawn".into()),
            },
            &rig.input,
            false,
        );
        super::super::settle_attached_card_focus(
            &mut rig.state,
            false,
            &rig.approvals,
            &rig.consent,
            &rig.consolidation_token,
            &rig.retract_confirm,
        );
        assert!(rig.approvals.pending.is_empty());
        assert_eq!(rig.state.focus, FocusState::Input);
        assert!(rig.frames().is_empty());
    }

    #[tokio::test]
    async fn story21_3_borrowed_focus_does_not_restore_unborrowed_focus() {
        for (read_only, draft) in [(true, ""), (false, "draft")] {
            let mut rig = Rig::new(read_only);
            rig.input = draft.into();
            rig.retained_arrives(false);
            assert_eq!(rig.state.focus, FocusState::Input);
            rig.esc().await;
            rig.ch('y').await;
            assert_eq!(rig.state.focus, FocusState::Chat);
            assert_eq!(rig.input, draft);
        }
    }

    async fn arrival_behind_surface(surface: char) {
        for draft in ["", "saved draft"] {
            let mut rig = Rig::new(false);
            rig.input = draft.into();
            rig.streaming.is_streaming = true;
            if surface == 'q' {
                rig.ctrl('q').await;
            } else {
                rig.ctrl('x').await;
                rig.ch(surface).await;
            }
            assert_eq!(rig.state.focus, FocusState::Input);
            assert!(rig.state.notification_log.is_some() || rig.state.quit_confirm.is_some());
            rig.approval_arrives("behind-surface");
            assert_eq!(rig.state.focus, FocusState::Input, "surface keeps focus");
            rig.ch('d').await;
            assert_eq!(rig.input, draft, "surface keeps its keys");
            rig.esc().await;
            assert_eq!(
                rig.state.focus,
                if draft.is_empty() {
                    FocusState::Chat
                } else {
                    FocusState::Input
                }
            );
            assert_eq!(rig.approvals.pending.len(), 1);
            assert_eq!(rig.input, draft);
            assert!(
                rig.frames().is_empty(),
                "dismissal never decides or interrupts"
            );
        }
    }

    #[tokio::test]
    async fn story21_3_arrival_behind_log_attached() {
        arrival_behind_surface('n').await;
    }
    #[tokio::test]
    async fn story21_3_arrival_behind_quit_attached() {
        arrival_behind_surface('q').await;
    }

    #[tokio::test]
    async fn story21_3_attached_approval_arriving_behind_help_remains_discoverable() {
        for draft in ["", "saved draft"] {
            let mut rig = Rig::new(false);
            rig.input = draft.into();
            rig.ctrl('x').await;
            rig.ch('?').await;
            assert!(presentation::is_open(&rig.state, Surface::Help));
            assert_eq!(rig.state.focus, FocusState::Input);
            rig.approval_arrives("behind-help");
            assert!(presentation::is_open(&rig.state, Surface::Help));
            assert_eq!(rig.state.focus, FocusState::Input);
            rig.esc().await;
            assert_eq!(
                rig.state.focus,
                if draft.is_empty() {
                    FocusState::Chat
                } else {
                    FocusState::Input
                }
            );
            rig.paint_approvals();
            if !draft.is_empty() {
                assert_eq!(
                    super::super::attached_notice_badges(&rig.state, &rig.approvals).decisions,
                    1
                );
                assert!(rig.approvals.painted.is_none(), "draft hides card");
            }
            rig.ch('y').await;
            if draft.is_empty() {
                assert!(matches!(
                    rig.frames().as_slice(),
                    [ClientFrame::ApprovalResponse {
                        outcome: crate::domain::models::ApprovalOutcome::Once,
                        ..
                    }]
                ));
                rig.ch('y').await;
                assert!(rig.frames().is_empty());
            } else {
                assert_eq!(rig.input, "saved drafty");
                assert!(rig.frames().is_empty());
                assert_eq!(rig.approvals.pending.len(), 1);
                rig.ctrl('x').await;
                rig.ch('q').await;
                let route = rig.notice(NoticeSource::Attached).unwrap();
                assert!(
                    route.contains("clear the draft")
                        && route.contains(&crate::adapters::tui::keymap::render::key_text(
                            InputCmd::ReturnToChat
                        ))
                );
                assert!(route.contains(&crate::adapters::tui::keymap::render::rail(
                    Context::Permission,
                    crate::adapters::tui::keymap::table_permission::PermissionCmd::Deny
                )));
                rig.esc().await;
                rig.paint_approvals();
                assert!(
                    rig.approvals.painted.is_none(),
                    "Esc does not expose a drafted approval"
                );
            }
        }
    }

    #[tokio::test]
    async fn story21_3_attached_proposal_ctrl_c_cues_without_interrupting_a_live_turn() {
        let mut rig = Rig::new(false);
        rig.state.focus = FocusState::Chat;
        super::super::apply_attached_proposal_frame(
            &mut rig.state,
            &mut rig.tab,
            &mut rig.team_requests,
            "c",
            DaemonFrame::TeamProposal {
                proposal: "p".into(),
                recipients: vec!["peer".into()],
                message_type: crate::domain::models::SemanticMessageType::ArchitectureUpdate,
                body: "review".into(),
            },
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let mut layout = crate::adapters::tui::layout::compute_layout(
                    frame.area(),
                    &rig.state.theme,
                    "",
                    1,
                    false,
                    crate::domain::models::visual::DensityMode::Focus,
                )
                .unwrap();
                let card = super::super::prepare_attached_proposal(
                    &rig.state,
                    &rig.tab,
                    "",
                    false,
                    false,
                    &mut layout,
                    frame.area(),
                );
                rig.state.team_proposal_visible =
                    crate::adapters::tui::widgets::team_proposal_card::render(
                        card,
                        frame.buffer_mut(),
                        &rig.state.theme,
                    );
            })
            .unwrap();
        assert!(rig.state.team_proposal_visible.is_some());
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.streaming.is_streaming = true;
        rig.ctrl('c').await;
        assert!(rig.frames().is_empty(), "no interrupt or decision");
        assert_eq!(
            rig.notice(NoticeSource::Approval),
            Some(
                crate::adapters::tui::keymap::render::card_cue(
                    Context::TeamProposal,
                    Host::Attached
                )
                .as_str()
            )
        );
        assert!(rig.state.pending_team_proposals.contains_key("c"));
        rig.input = "saved draft".into();
        rig.ctrl('c').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::InterruptTurn { .. }]
        ));
        assert_eq!(rig.input, "saved draft");
        assert!(rig.state.pending_team_proposals.contains_key("c"));
    }

    /// 21-3 3-C patch round (R1, FR-21-3-c): an approval that arrives while the writer sits in
    /// an empty Input hands the card Chat focus; `y` then decides once. Esc / Ctrl+C post the
    /// registry cue through the host (R4c) and a card never answers outside Chat (R4b).
    #[tokio::test]
    async fn story21_3_attached_approval_arrival_takes_chat_focus_for_an_empty_composer() {
        let mut rig = Rig::new(false);
        assert_eq!(rig.state.focus, FocusState::Input);
        rig.approval_arrives("a");
        assert_eq!(rig.state.focus, FocusState::Chat, "the arrival takes focus");
        let cue =
            crate::adapters::tui::keymap::render::card_cue(Context::Permission, Host::Attached);
        for ctrl_c in [false, true] {
            if ctrl_c {
                rig.ctrl('c').await;
            } else {
                rig.esc().await;
            }
            assert!(rig.frames().is_empty(), "no decision");
            assert_eq!(rig.approvals.pending.len(), 1, "the card stays pending");
            assert_eq!(rig.notice(NoticeSource::Attached), Some(cue.as_str()));
        }
        assert!(
            rig.state.quit_confirm.is_none(),
            "Ctrl+C is never a quit here"
        );
        // Outside Chat the painted card is blocked: `y` is composer text.
        rig.state.focus = FocusState::Input;
        rig.ch('y').await;
        assert_eq!(rig.input, "y");
        assert!(rig.frames().is_empty() && rig.approvals.pending.len() == 1);
        rig.input.clear();
        rig.state.focus = FocusState::Chat;
        rig.paint_approvals();
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse {
                outcome: crate::domain::models::ApprovalOutcome::Once,
                ..
            }]
        ));
        assert!(rig.approvals.pending.is_empty(), "decided once");
        rig.ch('y').await;
        assert!(rig.frames().is_empty(), "a second `y` decides nothing");
    }

    /// A draft keeps focus: the user is mid-sentence, so `y` stays text and no frame goes out
    /// (the card is hidden by the paint rule and still pending). A read-only client keeps focus.
    #[tokio::test]
    async fn story21_3_attached_approval_arrival_never_takes_focus_from_a_draft_or_a_reader() {
        let mut rig = Rig::new(false);
        rig.input = "half a sentence ".to_owned();
        rig.approval_arrives("a");
        assert_eq!(rig.state.focus, FocusState::Input);
        rig.ch('y').await;
        assert_eq!(rig.input, "half a sentence y");
        assert!(rig.frames().is_empty());
        assert_eq!(rig.approvals.pending.len(), 1, "the card waits");

        let mut reader = Rig::new(true);
        reader.approval_arrives("a");
        assert_eq!(reader.state.focus, FocusState::Input);
        reader.ch('y').await;
        assert_eq!(reader.input, "y");
        assert!(reader.frames().is_empty());
    }

    /// R4a: the consolidation card answers `y` / `n` in Chat (a token is sent once), they are
    /// text in Input, and `Esc` / `Ctrl+C` post the registry cue and leave it pending. The
    /// arrival takes the Chat focus through the loop's own function.
    #[tokio::test]
    async fn story21_3_attached_consolidation_card_answers_in_chat_and_cues_on_esc() {
        fn card() -> Option<PendingConsolidationCard> {
            Some(PendingConsolidationCard {
                conversation_id: String::new(),
                proposals: vec![],
            })
        }
        let mut rig = Rig::new(false);
        rig.consolidation_card = card();
        rig.consolidation_token = Some(ProposalToken(7));
        // Input focus: `y` / `n` are text.
        rig.ch('y').await;
        rig.ch('n').await;
        assert_eq!(rig.input, "yn");
        assert!(rig.frames().is_empty() && rig.consolidation_token.is_some());
        rig.input.clear();
        attached_card_arrived(&mut rig.state, &rig.input, false);
        assert_eq!(rig.state.focus, FocusState::Chat);
        let cue = crate::adapters::tui::keymap::render::card_cue(Context::Confirm, Host::Attached);
        for ctrl_c in [false, true] {
            if ctrl_c {
                rig.ctrl('c').await;
            } else {
                rig.esc().await;
            }
            assert!(rig.frames().is_empty() && rig.consolidation_token.is_some());
            assert_eq!(rig.notice(NoticeSource::Attached), Some(cue.as_str()));
        }
        rig.ch('n').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ConsolidationResolve {
                token: ProposalToken(7),
                accept: false
            }]
        ));
        assert!(rig.consolidation_token.is_none() && rig.consolidation_card.is_none());
        assert_eq!(
            rig.state.focus,
            FocusState::Input,
            "consolidation returns borrowed focus"
        );
        rig.consolidation_card = card();
        rig.consolidation_token = Some(ProposalToken(8));
        attached_card_arrived(&mut rig.state, &rig.input, false);
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ConsolidationResolve {
                token: ProposalToken(8),
                accept: true
            }]
        ));
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
                    crate::domain::models::visual::DensityMode::Focus,
                )
                .unwrap();
                let shell = crate::adapters::tui::presentation::Shell::new(area, &layout);
                crate::adapters::tui::presentation::compose(
                    f,
                    state,
                    &shell,
                    Host::Attached,
                    &log_facts("conversation-1"),
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
            rig.streaming.is_streaming = true;
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
        assert!(rig.input.is_empty(), "the command is consumed");
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
        assert!(rig.input.is_empty());
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
        notifications::error(&mut rig.state, NoticeSource::Attached, "an error");
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

    /// S2 AC10 — the attached client has no decision producers: `Ctrl+X q` is a
    /// painted, routed chord that answers honestly (an Info notice) and sends
    /// nothing to the daemon; the status row never claims a waiting decision.
    #[tokio::test]
    async fn story21_1_attached_ctrl_x_q_answers_that_no_decision_waits_in_this_client() {
        let mut rig = Rig::new(false);
        assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
        assert_eq!(rig.ch('q').await, KeyFlow::Continue);
        let notice = rig.state.notifications.visible().next().expect("a toast");
        assert_eq!(notice.message, "No decisions waiting in this client");
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
        assert!(viewer.conversation.messages.is_empty());

        let mut rig = Rig::new(false);
        rig.interrupt.apply(&DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(7)),
        });
        rig.streaming.is_streaming = true;
        rig.rx.close();
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        let dead = rig.state.notifications.visible().next().expect("a toast");
        assert_eq!(dead.message, "attach socket closed — stop request not sent");
        assert_eq!(dead.severity, NoticeSeverity::Error);
        assert_eq!(rig.state.status, StatusState::Idle, "no sticky status");
        assert!(rig.conversation.messages.is_empty());
    }

    // ── Story 21.3 (3-A): the attached leader and KG-2 truth from the registry ──

    /// The crossterm event a terminal sends for one registry `Key`.
    fn key_event(key: Key) -> (KeyCode, KeyModifiers) {
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
            rig.input = draft.to_owned();
            rig
        }

        async fn key(&mut self, key: Key) -> KeyFlow {
            let (code, modifiers) = key_event(key);
            self.press(code, modifiers).await
        }

        /// A transcript with room to scroll both ways, so a scroll key moves it.
        fn scrollable(mut self) -> Self {
            self.state.viewport_height = 20;
            self.scroll_offset = 12;
            self.auto_scroll = false;
            self
        }
    }

    /// Everything a key with no row must leave alone.
    #[derive(Debug, PartialEq)]
    struct Salient {
        focus: FocusState,
        input: String,
        multiline: bool,
        /// which-key, help, palette, S-LOG, S-QUIT, popup, recipient surface.
        surfaces: [bool; 7],
        scroll: (usize, bool),
        notices: usize,
        feedback: usize,
    }

    fn salient(rig: &Rig) -> Salient {
        let state = &rig.state;
        Salient {
            focus: state.focus.clone(),
            input: rig.input.clone(),
            multiline: state.multiline_mode,
            surfaces: [
                state.pending_leader.is_some(),
                presentation::is_open(state, Surface::Help),
                presentation::is_open(state, Surface::CommandPalette),
                presentation::is_open(state, Surface::NotificationLog),
                presentation::is_open(state, Surface::QuitConfirm),
                state.autocomplete.is_open(),
                state.recipient_surface.is_some(),
            ],
            scroll: (rig.scroll_offset, rig.auto_scroll),
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
            assert_eq!(rig.input, "draft", "{label}: composer unchanged");
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
            assert_eq!(rig.input, "draftx", "{label}: not a second follow-up");
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
        assert_eq!(rig.input, "draft");
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
            rig.streaming.is_streaming = true;
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
                    assert_eq!(rig.input, "draft", "{label}");
                    assert_eq!(rig.state.focus, FocusState::Input, "{label}");
                }
                LeaderCmd::A2aLog => {
                    assert!(rig.feedback("team-log").is_some(), "{label}");
                    assert!(rig.frames().is_empty(), "{label}: a local read");
                    assert_eq!(rig.input, "draft", "{label}");
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
                    assert_eq!(rig.input, "draft", "{label}");
                }
                LeaderCmd::ClearToasts => {
                    assert_eq!(rig.state.notifications.visible().count(), 0, "{label}");
                    assert_eq!(rig.state.notifications.records().len(), 1, "log keeps it");
                }
                LeaderCmd::Decisions => {
                    let toast = rig.state.notifications.visible().next().expect("a toast");
                    assert_eq!(toast.message, "No decisions waiting in this client");
                    assert_eq!(toast.severity, NoticeSeverity::Info);
                    assert_eq!(posted, 1, "{label}");
                    assert!(rig.frames().is_empty(), "{label}: a local answer");
                }
                LeaderCmd::Send => {
                    let frames = rig.frames();
                    assert_eq!(frames.len(), 1, "{label}: one user frame");
                    assert_eq!(user_text(&frames[0]), Some("draft"));
                    assert!(rig.input.is_empty(), "{label}: composer sent");
                    assert_eq!(posted, 0, "{label}");
                }
                LeaderCmd::Cancel => {
                    assert_eq!(posted, 0, "{label}: cancels silently");
                    assert!(rig.frames().is_empty(), "{label}");
                    assert_eq!(rig.input, "draft", "{label}");
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
                        assert_eq!(rig.input, "draft");
                    }
                    GlobalCmd::QuitRequest => {
                        let mut rig = Rig::at(focus.clone(), "draft");
                        assert_eq!(rig.key(key).await, KeyFlow::Continue);
                        assert!(rig.state.quit_confirm.is_some(), "{focus:?}: asks first");
                        assert_eq!(rig.input, "draft");
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
                    assert!(rig.input.is_empty());
                    let mut multiline = Rig::at(FocusState::Input, "hi");
                    multiline.state.multiline_mode = true;
                    multiline.key(key).await;
                    assert_eq!(multiline.input, "hi\n", "multi-line: a newline");
                    assert!(multiline.frames().is_empty());
                }
                InputCmd::Send => {
                    assert_eq!(frames.len(), 1, "{row:?}");
                    assert_eq!(user_text(&frames[0]), Some("hi"));
                    assert!(rig.input.is_empty());
                }
                InputCmd::InsertNewline => {
                    no_frames("newline");
                    assert_eq!(rig.input, "hi\n", "{row:?}");
                }
                InputCmd::NoCursor => {
                    no_frames("Ctrl+A/E/K");
                    assert_eq!(
                        rig.input, "hi",
                        "no cursor: nothing moves, nothing is deleted"
                    );
                    assert!(!rig.state.multiline_mode);
                }
                InputCmd::DeleteWordBack | InputCmd::KillToLineStart => {
                    no_frames("Ctrl+W/U");
                    assert_eq!(rig.input, "", "the tail of the one-line draft goes");
                }
                InputCmd::ToggleMultiline => {
                    no_frames("Alt+M");
                    assert!(rig.state.multiline_mode);
                    assert_eq!(rig.input, "hi", "Alt+M is not typed");
                }
                InputCmd::ReturnToChat => {
                    no_frames("Esc");
                    assert_eq!(rig.state.focus, FocusState::Chat);
                    assert_eq!(rig.input, "hi", "unsent text kept");
                }
                InputCmd::OpenPalette => {
                    assert!(presentation::is_open(&rig.state, Surface::CommandPalette));
                    assert_eq!(rig.input, "hi");
                }
                InputCmd::CancelOrQuit => {
                    no_frames("Ctrl+C");
                    assert!(rig.state.quit_confirm.is_some(), "a draft asks first");
                    assert_eq!(rig.input, "hi");
                }
                InputCmd::DeleteBack => {
                    no_frames("Backspace");
                    assert_eq!(rig.input, "h");
                }
                InputCmd::OpenSlashPopup => {
                    no_frames("/");
                    assert_eq!(rig.input, "/");
                    assert!(rig.state.autocomplete.is_open());
                    assert_eq!(
                        rig.state.autocomplete.kind,
                        Some(AutocompleteKind::SlashCommand)
                    );
                }
                InputCmd::TranscriptPageUp => {
                    no_frames("PgUp");
                    assert_eq!((rig.scroll_offset, rig.auto_scroll), (17, false));
                }
                InputCmd::TranscriptPageDown => {
                    no_frames("PgDn");
                    assert_eq!((rig.scroll_offset, rig.auto_scroll), (7, false));
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
            // Viewport 6: a half page is 3 rows and a full page is 6, capped to 5 per step.
            let mut rig = Rig::at(FocusState::Chat, "hi").scrollable();
            rig.state.viewport_height = 6;
            assert_eq!(rig.key(key).await, KeyFlow::Continue, "{row:?}");
            assert!(rig.frames().is_empty(), "{row:?}: a Chat key sends nothing");
            match cmd {
                ChatCmd::FocusToggle | ChatCmd::FocusInput => {
                    assert_eq!(rig.state.focus, FocusState::Input, "{row:?}");
                    assert_eq!(rig.input, "hi");
                }
                ChatCmd::Quit | ChatCmd::CancelOrQuit => {
                    assert!(
                        rig.state.quit_confirm.is_some(),
                        "{row:?}: a draft asks first"
                    );
                    assert_eq!(rig.input, "hi");
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
                    assert_eq!((rig.scroll_offset, rig.auto_scroll), (9, false), "{row:?}");
                }
                ChatCmd::ScrollPageDown => {
                    assert_eq!((rig.scroll_offset, rig.auto_scroll), (7, false), "{row:?}");
                }
                ChatCmd::ScrollHalfPageUp => {
                    assert_eq!((rig.scroll_offset, rig.auto_scroll), (15, false), "{row:?}");
                }
                ChatCmd::ScrollPageUp => {
                    assert_eq!((rig.scroll_offset, rig.auto_scroll), (17, false), "{row:?}");
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
        assert_eq!(rig.input, "abca");
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
        assert_eq!(rig.input, "draft");
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
        rig.streaming.is_streaming = true;
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
        assert_eq!(rig.input, "draft");
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
            rig.streaming.is_streaming = true;
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
        rig.input.clear();
        rig.ch('/').await;
        assert!(rig.state.autocomplete.is_open());
        rig.ctrl('c').await;
        assert!(!rig.state.autocomplete.is_open(), "popup closed");
        assert!(rig.input.is_empty(), "the composer is restored");
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

    /// K8 (attached leg, AC11): the attached composer is append-only: Ctrl+W, Ctrl+U
    /// and Ctrl+H edit its tail by grapheme; Ctrl+A / Ctrl+E / Ctrl+K change nothing.
    #[tokio::test]
    async fn story21_3_attached_composer_edits_its_tail_by_grapheme() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let mut rig = Rig::at(FocusState::Input, &format!("alpha beta {family}"));
        rig.ctrl('w').await;
        assert_eq!(rig.input, "alpha beta ", "the family is one word");
        rig.ctrl('w').await;
        assert_eq!(rig.input, "alpha ");
        rig.input = "xe\u{301}".into();
        rig.ctrl('h').await;
        assert_eq!(rig.input, "x", "the accent goes with its letter");
        rig.input = "one\ntwo three".into();
        rig.ctrl('u').await;
        assert_eq!(rig.input, "one\n", "Ctrl+U deletes the last logical line");
        rig.input = "keep".into();
        for c in ['a', 'e', 'k'] {
            assert_eq!(rig.ctrl(c).await, KeyFlow::Continue);
            assert_eq!(rig.input, "keep", "Ctrl+{c} has no cursor to move");
        }
        // Chat focus: the keys do not edit the unsent draft.
        let mut rig = Rig::at(FocusState::Chat, "draft");
        rig.ctrl('w').await;
        assert_eq!(rig.input, "draft");
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
            rig.input
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
        format!("help={} scroll={scroll} {}", presentation::is_open(&rig.state, Surface::Help), open_surface(rig))
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
            assert!(presentation::is_open(&rig.state, Surface::CommandPalette) && rig.input == "draft" && rig.frames().is_empty());
        }
        let mut typing = Rig::at(FocusState::Input, "draft");
        typing.ctrl('p').await;
        typing.ch('z').await;
        assert_eq!(typing.state.command_palette.filter_text.as_str(), "z");
        assert!(presentation::is_open(&typing.state, Surface::CommandPalette));
        assert_eq!(typing.input, "draft", "the palette filter never touches the composer");
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
        format!("log={} sel={sel} detail={detail} scroll={scroll} archived={archived} records={} clip={clip} {}", s.notification_log.is_some(), s.notifications.len(), open_surface(rig))
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

    /// A node view at line 20 of an 11-row viewport over Chat (prior focus Input); read-only, so no action letter is live.
    #[rustfmt::skip]
    fn surface_rig(_: Key) -> Rig {
        use crate::adapters::tui::state::{RecipientSurface, RecipientSurfaceAccess, RecipientSurfaceTarget};
        use crate::domain::models::{SemanticMessageType, VerdictOffer};
        let mut rig = Rig::at(FocusState::Chat, "draft");
        rig.state.recipient_surface = Some(RecipientSurface {
            target: RecipientSurfaceTarget::Node("n".into()), access: RecipientSurfaceAccess::ReadOnly,
            can_toggle_target: false, root_id: None, first_non_pinned_ordinal: None,
            last_non_pinned_ordinal: None, omitted_before: 2, omitted_after: 3, node_id: None,
            item_state: None, context_available: false, context_selected: false,
            prior_focus: FocusState::Input, scroll_offset: 20, viewport_rows: 11, content: String::new(),
            message_type: SemanticMessageType::Unknown, verdict_offer: VerdictOffer::None, cached_width: 0,
            cached_lines: Vec::new(), cached_header: Vec::new(), cached_footer: Vec::new(),
            cached_action_mask: 0, results: Vec::new(),
        });
        rig
    }

    #[rustfmt::skip]
    fn surface_digest(rig: &Rig) -> String {
        let scroll = rig.state.recipient_surface.as_ref().map_or(0, |s| s.scroll_offset);
        format!("surface={} scroll={scroll} {}", rig.state.recipient_surface.is_some(), open_surface(rig))
    }

    /// Catches a surface row that closes into the wrong focus, pages the wrong way or skips the quit question, or a key leaking below.
    // LIMITATION: `p`/`l` (thread paging) and `Tab` (item/node toggle) need an item thread window and an a2a
    // workspace; on this node view they are only witnessed as "surface survives, nothing sent".
    #[rustfmt::skip]
    #[tokio::test]
    async fn story21_3_attached_witness_recipient_surface() {
        let up = |scroll: u8, focus: &str, surface: bool, quit: bool| format!("Continue | surface={surface} scroll={scroll} quit={quit} focus={focus} input=draft");
        let cases = [
            (Key::Char('p'), up(20, "Chat", true, false)), (Key::Char('l'), up(20, "Chat", true, false)), (Key::Tab, up(20, "Chat", true, false)),
            (Key::Esc, up(0, "Input", false, false)), (Key::Ctrl('c'), up(0, "Input", false, false)),
            (Key::PageUp, up(10, "Chat", true, false)), (Key::PageDown, up(30, "Chat", true, false)),
            (Key::Ctrl('q'), up(20, "Chat", true, true)),
        ];
        let open = |k| std::future::ready(surface_rig(k));
        witness_rows(Context::RecipientSurface, open, surface_digest, &cases).await;
        // Ctrl+X passes through the surface to the leader (`dispatch_recipient_surface_key`): exempt, and checked here.
        witness_inert(Context::RecipientSurface, open, surface_digest, |k| k == Key::Ctrl('x')).await;
        let mut rig = surface_rig(Key::Esc);
        rig.ctrl('x').await;
        assert!(rig.state.pending_leader.is_some() && rig.state.recipient_surface.is_some());
        assert!(rig.frames().is_empty());
    }

    /// Catches a writer's item-surface action key (a named exclusion of the table) that sends nothing or the wrong frame.
    #[tokio::test]
    async fn story21_3_attached_recipient_action_key_sends_its_frame() {
        use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
        use crate::domain::models::{ItemAddress, ItemId, PeerId};
        let mut rig = surface_rig(Key::Esc);
        let peer = PeerId::from_public_key(&[91; 32]).unwrap();
        let surface = rig.state.recipient_surface.as_mut().unwrap();
        surface.target = RecipientSurfaceTarget::Item(ItemAddress::from_a2a_ingress(
            peer,
            ItemId::from_replay("ri-writer"),
        ));
        surface.access = RecipientSurfaceAccess::Writer;
        surface.context_available = true;
        assert_eq!(rig.key(Key::Char('x')).await, KeyFlow::Continue);
        assert!(
            rig.frames().is_empty(),
            "a letter with no action sends nothing"
        );
        assert_eq!(rig.key(Key::Char('c')).await, KeyFlow::Continue);
        let frames = rig.frames();
        assert!(
            matches!(frames.as_slice(), [ClientFrame::SetRecipientContext { item_id, selected: true }] if item_id == "ri-writer"),
            "{frames:?}"
        );
        assert!(
            rig.state.recipient_surface.is_some(),
            "the surface stays open while the daemon answers"
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
        assert_eq!(rig.input, "draft");
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
        assert_eq!((rig.scroll_offset, rig.auto_scroll), (15, false));
        assert_eq!(rig.input, "draft", "Ctrl+U in Chat never edits the draft");
        assert_eq!(rig.key(Key::Ctrl('d')).await, KeyFlow::Continue);
        assert_eq!((rig.scroll_offset, rig.auto_scroll), (12, false));
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
        assert_eq!(rig.input, "hI?@");
        assert!(!rig.state.autocomplete.is_open());

        let mut rig = Rig::at(FocusState::Input, "");
        rig.key(Key::Char('/')).await;
        assert!(rig.state.autocomplete.is_open());
        assert_eq!(rig.input, "/");
        let mut rig = Rig::at(FocusState::Input, "a");
        rig.key(Key::Char('/')).await;
        assert!(
            !rig.state.autocomplete.is_open(),
            "only an empty composer triggers"
        );
        assert_eq!(rig.input, "a/");

        let mut rig = Rig::at(FocusState::Input, "hi");
        rig.key(Key::Alt('m')).await;
        assert!(rig.state.multiline_mode);
        assert_eq!(rig.input, "hi", "Alt+M is not typed");

        let mut rig = Rig::at(FocusState::Input, "hi");
        assert_eq!(rig.key(Key::CtrlEnter).await, KeyFlow::Continue);
        let frames = rig.frames();
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(user_text(&frames[0]), Some("hi"));
        assert!(rig.input.is_empty());
        let mut empty = Rig::at(FocusState::Input, "");
        empty.key(Key::CtrlEnter).await;
        assert!(empty.frames().is_empty(), "an empty composer sends nothing");

        for focus in [FocusState::Input, FocusState::Chat] {
            let mut rig = Rig::at(focus.clone(), "hi").scrollable();
            rig.key(Key::PageUp).await;
            assert_eq!(
                (rig.scroll_offset, rig.auto_scroll),
                (17, false),
                "{focus:?}"
            );
            rig.key(Key::PageDown).await;
            rig.key(Key::PageDown).await;
            assert_eq!(
                (rig.scroll_offset, rig.auto_scroll),
                (7, false),
                "{focus:?}"
            );
            assert_eq!(rig.input, "hi");
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
    async fn story21_4_attached_ctrl_q_over_team_retract_preserves_card() {
        let mut rig = Rig::at(FocusState::Input, "retained draft");
        crate::adapters::tui::handlers::team_command::open_team_retract_card(
            &mut rig.state,
            "daemon-attachment",
            crate::domain::events::TeamRetractPreview::Card(
                crate::domain::events::TeamRetractCard {
                    peer: "peer-a".into(),
                    item_id: "item-a".into(),
                    task: Some("task-a".into()),
                    body: "Retraction still needs its own answer".into(),
                    armed: true,
                },
            ),
        );
        rig.team_requests.open_retract = Some("retract-a".into());
        let before = (
            format!("{:?}", rig.state.pending_team_retract),
            rig.state.focus.clone(),
            rig.input.clone(),
        );
        assert_eq!(rig.ctrl('q').await, KeyFlow::Continue);
        assert!(presentation::is_open(
            &rig.state,
            presentation::Surface::QuitConfirm
        ));
        assert_eq!(rig.state.modal_stack.len(), 1);
        assert_eq!(
            (
                format!("{:?}", rig.state.pending_team_retract),
                rig.state.focus.clone(),
                rig.input.clone()
            ),
            before
        );
        rig.ch('n').await;
        assert!(rig.state.modal_stack.is_empty());
        assert_eq!(
            (
                format!("{:?}", rig.state.pending_team_retract),
                rig.state.focus.clone(),
                rig.input.clone()
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
                    Surface::DecisionPane
                    | Surface::ModelSelector
                    | Surface::ProfileSwitcher
                    | Surface::UsagePanel
                    | Surface::Search
                    | Surface::BookmarkList
                    | Surface::WaveOverlay
                    | Surface::CrossSearch
                    | Surface::Login => unreachable!("surface not offered attached"),
                }
                let before = (rig.input.clone(), rig.scroll_offset, rig.auto_scroll);
                let flow = rig.key(key).await;
                assert_eq!(
                    (rig.input.clone(), rig.scroll_offset, rig.auto_scroll),
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
            base.input, "y",
            "positive control: base keys still reach Input"
        );
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
        assert_eq!(rig.input, "draft");
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
        super::handle_attached_paste(&mut rig.state, &mut rig.input, "hidden paste", false);
        assert!(rig.input.is_empty());
        assert_eq!(rig.approvals.pending.len(), 1);
        rig.state.terminal_width = 80;
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
            assert_eq!(rig.input, "/te");
            assert!(rig.frames().is_empty());
            rig.esc().await;
            assert!(!rig.state.autocomplete.is_open());
            assert!(
                rig.input.is_empty(),
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
                let allocation = (rig.input.as_ptr(), rig.input.capacity());
                super::handle_attached_paste(&mut rig.state, &mut rig.input, "hidden paste", false);
                assert_eq!(salient(&rig), before);
                assert_eq!((rig.input.as_ptr(), rig.input.capacity()), allocation);
            }
        }
        let mut visible = Rig::at(FocusState::Input, "");
        super::handle_attached_paste(
            &mut visible.state,
            &mut visible.input,
            "visible paste",
            false,
        );
        assert_eq!(
            visible.input, "visible paste",
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
        use crate::domain::models::visual::DensityMode;
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
                    rig.streaming.is_streaming = true;
                    notifications::notify(
                        &mut rig.state,
                        NoticeSource::System,
                        NoticeSeverity::Info,
                        "body-witness",
                    );
                    let draw = |rig: &Rig, title: Option<&str>| {
                        let state = &rig.state;
                        let display = attached_display_input(state, &rig.input);
                        let area = Rect::new(0, 0, width, height);
                        let layout = compute_layout(
                            area,
                            &state.theme,
                            display,
                            1,
                            false,
                            DensityMode::Focus,
                        )
                        .unwrap();
                        assert_eq!(layout.input_area.height, if full { 10 } else { 3 });
                        assert!(layout.sidebar.is_none());
                        let shell = Shell::new(area, &layout);
                        assert_eq!(shell.region.bottom(), layout.chat_pane.bottom());
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal
                            .draw(|frame| {
                                super::super::render_attached_composer(
                                    frame,
                                    layout.input_area,
                                    display,
                                    if state.modal_stack.is_empty()
                                        && state.pending_leader.is_none()
                                    {
                                        state.focus.clone()
                                    } else {
                                        FocusState::Chat
                                    },
                                    &state.theme,
                                    state.multiline_mode,
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
                                    &log_facts(&rig.conversation.id),
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
                                            let footer = if quit { "Stay" } else { "Close" };
                                            assert!(row(buffer, rect, rect.y + 1).contains(header));
                                            assert!(row(buffer, rect, rect.y + 2).contains(body));
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
                    assert_eq!(rig.input, draft);
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
                    assert_eq!(rig.input, draft);
                    assert!(rig.state.input_buffer.is_empty() && rig.frames().is_empty());
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
                assert_eq!(rig.input, "attached draft 界");
                assert!(presentation::consistent(&rig.state));
                if depth == 1 {
                    assert_eq!(
                        palette_digest(&rig),
                        palette,
                        "lent filter and selection restored"
                    );
                }
            }
            assert!(rig.state.input_buffer.is_empty() && rig.frames().is_empty());
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
        let LeaderStep::Unavailable(_, remedy) =
            leader_step(Host::Attached, Key::Char('q'), &avail, Context::Help.name())
        else {
            panic!("attached frame-origin pane absence must be registry-gated");
        };
        assert_eq!(remedy, "No decisions waiting in this client");
        assert!(!presentation::is_open(&rig.state, Surface::DecisionPane));
        rig.ch('q').await;
        assert_eq!(rig.notice(NoticeSource::Chord), Some(remedy));
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
        assert!(rig.state.input_buffer.is_empty() && rig.frames().is_empty());
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
            let scroll = (rig.scroll_offset, rig.auto_scroll);
            let allocation = (rig.input.as_ptr(), rig.input.capacity());
            for pending in [false, true] {
                if pending {
                    rig.ctrl('x').await;
                }
                let before = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
                super::handle_attached_paste(&mut rig.state, &mut rig.input, "hidden", false);
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
                assert_eq!((rig.scroll_offset, rig.auto_scroll), scroll);
                assert_eq!((rig.input.as_ptr(), rig.input.capacity()), allocation);
                assert_eq!(rig.input, "draft");
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
        use crate::domain::models::visual::DensityMode;
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
                    let layout =
                        compute_layout(area, &state.theme, &draft, 1, false, DensityMode::Focus)
                            .unwrap();
                    let shell = Shell::new(area, &layout);
                    let rect = geometry::frame_rect(shell.region);
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| {
                            super::super::render_attached_composer(
                                frame,
                                layout.input_area,
                                &draft,
                                if state.modal_stack.is_empty() && state.pending_leader.is_none() {
                                    state.focus.clone()
                                } else {
                                    FocusState::Chat
                                },
                                &state.theme,
                                false,
                                None,
                            );
                            frame.render_widget(Paragraph::new("status-before"), layout.status_bar);
                            presentation::compose(
                                frame,
                                state,
                                &shell,
                                Host::Attached,
                                &log_facts(&rig.conversation.id),
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
                    assert_eq!(rig.input, draft);
                    assert!(rig.state.input_buffer.is_empty() && rig.frames().is_empty());
                    assert!(presentation::consistent(&rig.state));
                }
            }
        }
    }
    #[tokio::test]
    async fn story21_4_attached_a2a_nested_palette_retract_preserves_owner() {
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
                assert!(rig.retract_confirm.is_none());
                assert_eq!(rig.state.notifications.records().len(), records + 1);
                assert_eq!(rig.notice(NoticeSource::Palette), Some(CLOSE_WINDOW_FIRST));
            } else {
                assert!(rig.state.modal_stack.is_empty());
                assert_eq!(
                    rig.retract_confirm.as_ref().map(|(id, _)| id.as_str()),
                    Some("peer-response-auto-b")
                );
                assert_eq!(rig.state.focus, FocusState::Chat);
                assert_eq!(rig.state.notifications.records().len(), records);
            }
            assert_eq!(rig.input, "retained retract draft");
            assert!(rig.state.input_buffer.is_empty() && rig.frames().is_empty());
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
                    "No decisions waiting in this client".to_owned()
                } else {
                    crate::adapters::tui::keymap::resolve::no_binding_text(
                        Key::Char(key),
                        Context::Help.name(),
                    )
                };
                assert_eq!(rig.state.pending_leader, Some(Context::Help));
                assert_eq!(rig.notice(NoticeSource::Chord), Some(expected.as_str()));
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
                        .find(|(_, row)| row.contains("[Q] Decisions (n/a)"))
                        .unwrap();
                    let x = row[..row.find("[Q]").unwrap()].chars().count() as u16;
                    assert!(
                        buffer[(x, y as u16)].modifier.intersects(
                            ratatui::style::Modifier::BOLD | ratatui::style::Modifier::REVERSED
                        ),
                        "pressed unavailable row must stand out"
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
                assert_eq!(rig.input, "leader draft");
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
                        base.notice(NoticeSource::Attached),
                        Some("No decisions waiting in this client")
                    );
                } else {
                    assert_eq!(
                        base.notice(NoticeSource::Chord),
                        Some("No binding for Ctrl+X z in Input")
                    );
                }
                assert_eq!(base.input, "base draft");
                assert_eq!(base.state.focus, FocusState::Input);
            }
        }
    }

    #[tokio::test]
    async fn story21_4_attached_a2a_nested_palette_refusal_is_visible_without_dispatch() {
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
            assert!(rig.retract_confirm.is_none() && rig.frames().is_empty());
            rig.press(KeyCode::Left, KeyModifiers::NONE).await;
            assert_eq!(
                palette_digest(&rig),
                palette,
                "an inert key retains query and selection"
            );
            let cleared = footer_text(&paint(&rig.state, width));
            assert!(
                !cleared.contains("close the window first"),
                "{width}: {cleared}"
            );
            assert_eq!(
                rig.state.notifications.records().len(),
                records + 1,
                "clearing does not erase history"
            );
            assert_eq!(rig.input, "palette draft");
            assert_eq!(rig.state.focus, FocusState::Input);
            assert!(rig.retract_confirm.is_none() && rig.frames().is_empty());
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
                        .contains("close the window first")
                );
            } else {
                assert!(rig.state.modal_stack.is_empty());
                assert_eq!(
                    rig.notice(NoticeSource::Login),
                    Some(ATTACHED_SIGN_IN),
                    "base palette retains the existing attached auth refusal"
                );
            }
            assert_eq!(rig.input, "attached draft");
            assert!(rig.state.input_buffer.is_empty() && rig.frames().is_empty());
        }
    }
}
