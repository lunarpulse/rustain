//! Attached host key front door (Story 21.2 Task 5: AC6 / AC8 / AC12 / AC13).
//!
//! [`handle_attached_key`] is the one place `run_attached` sends EVERY key
//! event. It keeps the pre-existing owner order (S-QUIT first, then S-LOG, then
//! the new overlays, then the team-retract card, approvals, proposals, recipient
//! surface, the peer card Tab and the inline decision cards) and adds what the
//! attached client was missing:
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
use crate::adapters::tui::app::{cancel_autocomplete_popup, convert_crossterm_event, handle_input};
use crate::adapters::tui::chord_menu::{self, AttachedRoute, ChordHost};
use crate::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
use crate::adapters::tui::palette_gate::PaletteGate;
use crate::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitKeyOutcome, QuitRequest};
use crate::adapters::tui::state::{PendingConsolidationCard, TabRenderState, TuiState};
use crate::adapters::tui::widgets::{
    autocomplete_popup, command_palette as command_palette_widget, help_overlay, input_box,
    toast_layer, which_key_bar,
};
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use crate::domain::models::visual::OverlayType;
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
                shortcut: match name {
                    "ml" => Some("Alt+M".to_owned()),
                    "quit" => Some("Ctrl+Q".to_owned()),
                    "notifications" => Some("Ctrl+X, N".to_owned()),
                    "detach" => Some("Ctrl+X, D".to_owned()),
                    _ => None,
                },
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

    /// Fill the slash popup from the attached catalogue. Re-scans custom
    /// command files only when the popup opens (empty filter), like the local
    /// host; filtering as you type uses the cached registry.
    fn sync_slash(&mut self, state: &mut TuiState, workspace: &Path) {
        if !state.autocomplete.active || state.autocomplete.kind != AutocompleteKind::SlashCommand {
            self.last_slash_filter = None;
            return;
        }
        let unchanged = self.last_slash_filter.as_deref() == Some(&*state.autocomplete.filter_text);
        if unchanged && !state.autocomplete.suggestions.is_empty() {
            return;
        }
        if state.autocomplete.filter_text.is_empty() {
            self.commands.refresh(workspace);
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
                "ml" => "Toggle multi-line input (Alt+M)",
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

/// Close every transient surface and restore its origin focus (quit and the
/// `Ctrl+Q` path never leave a half-open overlay behind the decision frame).
fn close_surfaces(state: &mut TuiState) {
    if state.which_key.active {
        state.focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
    }
    if state.command_palette.active {
        state.focus = state.command_palette.dismiss().unwrap_or(FocusState::Input);
    }
    if state.help_overlay.active {
        state.focus = state.help_overlay.close();
    }
    if state.notification_log.is_some() {
        notification_log::close(state);
    }
    state.needs_redraw = true;
}

/// True while a paste must not edit the draft: the decision frame or an overlay
/// owns the keyboard.
pub(super) fn paste_blocked(state: &TuiState) -> bool {
    state.quit_confirm.is_some()
        || state.notification_log.is_some()
        || state.command_palette.active
        || state.which_key.active
        || state.help_overlay.active
}

/// The status-row hint for the attached client (no key here detaches except
/// the leader chord).
pub(super) fn status_hint(state: &TuiState) -> Option<&'static str> {
    state
        .recipient_surface
        .is_none()
        .then_some(if state.multiline_mode {
            "^X d detach · [^X Enter] Send"
        } else {
            "^X d detach"
        })
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

/// Paint the attached overlays above the base frame. Draw order: the toast
/// layer (below every overlay), slash popup, the untimed which-key menu (above
/// the status row), the palette, the help overlay, S-LOG, and S-QUIT last.
pub(super) fn paint_overlays(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    chat_pane: Rect,
    input_area: Rect,
    status_bar: Rect,
    state: &TuiState,
    facts: &LogFacts,
) {
    toast_layer::render(frame, chat_pane, &state.notifications, &state.theme, facts);
    let above_status = Rect::new(
        area.x,
        area.y,
        area.width,
        status_bar.y.saturating_sub(area.y),
    );
    if state.autocomplete.active {
        autocomplete_popup::render(frame, input_area, &state.autocomplete, &state.theme);
    }
    if state.which_key.active {
        which_key_bar::render(
            frame,
            above_status,
            &state.which_key,
            &state.theme,
            ChordHost::Attached,
        );
    }
    if state.command_palette.active {
        command_palette_widget::render(frame, area, &state.command_palette, &state.theme);
    }
    if state.help_overlay.active {
        help_overlay::render(
            frame,
            area,
            &state.help_overlay,
            &state.theme,
            state.multiplexer_detected,
        );
    }
    if let Some(log) = &state.notification_log {
        notification_log::render(
            frame,
            above_status,
            log,
            &state.notifications,
            &state.theme,
            facts,
        );
    }
    if let Some(quit) = &state.quit_confirm {
        quit_confirm::render(frame, area, quit, &state.theme);
    }
}

/// The attached key front door. Called by `run_attached` for every key event.
pub(super) async fn handle_attached_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crossterm::event::KeyEventKind;

    // S-QUIT owns every key while its frame is open — before approvals,
    // proposals, the retract cards or any surface (a stray `y` can never
    // answer a card underneath).
    if ctx.state.quit_confirm.is_some() {
        return match quit_confirm::handle_raw_key(ctx.state, key) {
            QuitKeyOutcome::Confirmed => KeyFlow::Quit,
            QuitKeyOutcome::Cancelled | QuitKeyOutcome::Consumed => KeyFlow::Continue,
        };
    }
    // Ctrl+Q is a quit request from any focus.
    if is_ctrl(&key, 'q') {
        close_surfaces(ctx.state);
        return request_quit(ctx);
    }
    // S-LOG owns every key while it is open (S-QUIT and Ctrl+Q above it).
    if ctx.state.notification_log.is_some() {
        return log_key(ctx, key);
    }
    // A transient surface that is open owns the key until it closes.
    if ctx.state.which_key.active {
        return leader_key(ctx, key).await;
    }
    if ctx.state.command_palette.active {
        return palette_key(ctx, key).await;
    }
    if ctx.state.help_overlay.active {
        return help_key(ctx, key);
    }
    if ctx.state.autocomplete.active
        && ctx.state.focus == FocusState::Input
        && let Some(flow) = popup_key(ctx, key)
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
        ctx.read_only
            || approval_higher
            || !matches!(ctx.state.focus, FocusState::Chat | FocusState::Input),
        attached_display_input(ctx.state, ctx.input),
        ctx.frame_tx,
    ) {
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

    // `Ctrl+C` inside a decision card is "back one frame": it answers exactly
    // like `Esc` (the card's own decline), never an interrupt or a quit.
    let key =
        if is_ctrl(&key, 'c') && (ctx.decision_cards_pending() || ctx.retract_confirm.is_some()) {
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            key
        };

    match (key.code, key.modifiers) {
        // Story 18.3d AC1 — sender-consent Approval Pattern. The guard is the
        // daemon-derived pending FIFO, so these single-letter keys are inert
        // everywhere else.
        (KeyCode::Char('y'), m)
            if !m.contains(KeyModifiers::CONTROL) && !ctx.consent.is_empty() =>
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
            if !m.contains(KeyModifiers::CONTROL) && !ctx.consent.is_empty() =>
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
        (KeyCode::Char('n'), m) | (KeyCode::Esc, m)
            if !m.contains(KeyModifiers::CONTROL) && !ctx.consent.is_empty() =>
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
            if !m.contains(KeyModifiers::CONTROL) && ctx.consolidation_token.is_some() =>
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
        (KeyCode::Char('n'), m) | (KeyCode::Esc, m)
            if !m.contains(KeyModifiers::CONTROL) && ctx.consolidation_token.is_some() =>
        {
            // n or Esc while the card is shown → decline.
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
            if !m.contains(KeyModifiers::CONTROL) && ctx.retract_confirm.is_some() =>
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
        (KeyCode::Char('n'), m) | (KeyCode::Esc, m)
            if !m.contains(KeyModifiers::CONTROL) && ctx.retract_confirm.is_some() =>
        {
            *ctx.retract_confirm = None;
            ctx.state.needs_redraw = true;
        }
        // Ctrl+X opens the untimed leader (RU-3); it is never a retract.
        (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
            let origin = ctx.state.focus.clone();
            ctx.state.which_key.open(origin);
            ctx.state.focus = FocusState::Overlay(OverlayType::WhichKey);
            ctx.state.needs_redraw = true;
        }
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => open_palette(ctx).await,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return ctrl_c(ctx),
        // Ctrl+D keeps its local focus-specific meaning: Chat half-page down,
        // inert in Input — never a detach.
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            if ctx.state.focus == FocusState::Chat {
                scroll_attached_transcript(
                    false,
                    ctx.state.viewport_height / 2,
                    ctx.streaming.is_streaming,
                    ctx.scroll_offset,
                    ctx.auto_scroll,
                );
                ctx.state.needs_redraw = true;
            }
        }
        // Ctrl+E is end-of-line; the attached cursor is always at the end, so it
        // is a no-op — never the multiline toggle.
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {}
        // Alt+M toggles multiline (Input focus, like the local host).
        (KeyCode::Char('m'), KeyModifiers::ALT) => {
            if ctx.state.focus == FocusState::Input {
                ctx.state.multiline_mode = !ctx.state.multiline_mode;
                ctx.state.needs_redraw = true;
            }
        }
        // Esc never detaches, quits or submits: with no popup/overlay open it
        // moves focus between Input and Chat and preserves the unsent text.
        (KeyCode::Esc, _) => {
            let next = match ctx.state.focus {
                FocusState::Input => Some(FocusState::Chat),
                FocusState::Chat => Some(FocusState::Input),
                _ => None,
            };
            if let Some(next) = next {
                ctx.state.focus = next;
            }
            ctx.state.needs_redraw = true;
        }
        (KeyCode::Enter, m) => return enter_key(ctx, m).await,
        (KeyCode::Backspace, _) => {
            if ctx.state.recipient_surface.is_some() || ctx.state.focus != FocusState::Input {
                return KeyFlow::Continue;
            }
            let _ = attached_composer_key_with_team_requests(
                KeyCode::Backspace,
                ctx.read_only,
                ctx.input,
                ctx.state,
                ctx.frame_tx,
                ctx.team_requests,
            );
        }
        (KeyCode::PageUp | KeyCode::PageDown, _) => {
            scroll_attached_transcript(
                key.code == KeyCode::PageUp,
                ctx.state.viewport_height,
                ctx.streaming.is_streaming,
                ctx.scroll_offset,
                ctx.auto_scroll,
            );
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            if ctx.state.recipient_surface.is_some() {
                return KeyFlow::Continue;
            }
            // Same case rule as the local converter: shift-folded letters.
            let c = if m.contains(KeyModifiers::SHIFT) && !m.contains(KeyModifiers::ALT) {
                c.to_ascii_uppercase()
            } else {
                c
            };
            return match ctx.state.focus {
                FocusState::Chat => chat_char(ctx, c, m),
                FocusState::Input => input_char(ctx, c, m),
                _ => KeyFlow::Continue,
            };
        }
        _ => {}
    }
    KeyFlow::Continue
}

/// Plain `q` / `i` / `?` are commands only in Chat focus.
fn chat_char(ctx: &mut KeyCtx<'_>, c: char, m: KeyModifiers) -> KeyFlow {
    if !(m.is_empty() || m == KeyModifiers::SHIFT) {
        return KeyFlow::Continue;
    }
    match c {
        'q' => return request_quit(ctx),
        'i' => {
            ctx.state.focus = FocusState::Input;
            ctx.state.needs_redraw = true;
        }
        '?' => {
            let prior = ctx.state.focus.clone();
            ctx.state.help_overlay.open(prior);
            ctx.state.focus = FocusState::Overlay(OverlayType::Help);
            ctx.state.needs_redraw = true;
        }
        _ => {}
    }
    KeyFlow::Continue
}

/// A printable key in Input focus: text, or the slash trigger.
fn input_char(ctx: &mut KeyCtx<'_>, c: char, m: KeyModifiers) -> KeyFlow {
    // Alt+V is the clipboard paste key locally; the attached host has none.
    if m.contains(KeyModifiers::ALT) && c.eq_ignore_ascii_case(&'v') {
        return KeyFlow::Continue;
    }
    if c == '/' && !m.contains(KeyModifiers::ALT) && ctx.input.is_empty() && !ctx.composer_locked()
    {
        // The shared composer owns the slash trigger: it saves the pre-trigger
        // composer so Esc can restore it exactly.
        let event = crate::domain::events::DomainInputEvent::KeyPress('/');
        with_lent_input(ctx.state, ctx.input, |state| {
            let _ = handle_input(state, &event);
        });
        ctx.chrome.sync_slash(ctx.state, ctx.workspace);
        return KeyFlow::Continue;
    }
    // Story 19.16g: read-only composers edit too; only submission is refused.
    let _ = attached_composer_key_with_team_requests(
        KeyCode::Char(c),
        ctx.read_only,
        ctx.input,
        ctx.state,
        ctx.frame_tx,
        ctx.team_requests,
    );
    KeyFlow::Continue
}

/// Enter and its modified forms. Plain Enter is never mistaken for Ctrl+Enter.
async fn enter_key(ctx: &mut KeyCtx<'_>, m: KeyModifiers) -> KeyFlow {
    if ctx.state.recipient_surface.is_some() || ctx.state.focus != FocusState::Input {
        return KeyFlow::Continue;
    }
    if m.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
        if !ctx.composer_locked() {
            ctx.input.push('\n');
            ctx.state.needs_redraw = true;
        }
        return KeyFlow::Continue;
    }
    if m.contains(KeyModifiers::CONTROL) {
        // Ctrl+Enter (where the terminal distinguishes it) sends.
        if ctx.input.is_empty() {
            return KeyFlow::Continue;
        }
        return submit_enter(ctx).await;
    }
    if ctx.state.multiline_mode {
        if !ctx.input.is_empty() && !ctx.composer_locked() {
            ctx.input.push('\n');
            ctx.state.needs_redraw = true;
        }
        return KeyFlow::Continue;
    }
    submit_enter(ctx).await
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
    match quit_confirm::request(ctx.state, facts) {
        QuitRequest::ExitNow => KeyFlow::Quit,
        QuitRequest::Confirming => KeyFlow::Continue,
    }
}

// ── Ctrl+X leader ────────────────────────────────────────────────────────────

/// A follow-up to the open leader. Esc and every non-character key cancel;
/// an unknown character is consumed once. The origin focus is always restored.
async fn leader_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    let origin = ctx.state.which_key.dismiss().unwrap_or(FocusState::Input);
    ctx.state.focus = origin.clone();
    ctx.state.needs_redraw = true;
    match (key.code, key.modifiers) {
        // Portable multiline send: the same path as Enter, exactly once.
        (KeyCode::Enter, m) if m.is_empty() => {
            if origin == FocusState::Input && !ctx.input.is_empty() {
                return submit_enter(ctx).await;
            }
            KeyFlow::Continue
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => chord(ctx, c, origin).await,
        _ => KeyFlow::Continue,
    }
}

async fn chord(ctx: &mut KeyCtx<'_>, c: char, origin: FocusState) -> KeyFlow {
    let Some(spec) = chord_menu::spec_for(c) else {
        return KeyFlow::Continue;
    };
    // The dispatch table (what the menu paints) must contain the chord too.
    if ctx.state.which_key.lookup_chord(c).is_none() {
        return KeyFlow::Continue;
    }
    match spec.attached {
        AttachedRoute::Same => {
            ctx.state.help_overlay.open(origin);
            ctx.state.focus = FocusState::Overlay(OverlayType::Help);
            ctx.state.needs_redraw = true;
        }
        AttachedRoute::Source => {
            // The existing attached `/team log` local read.
            let args = crate::adapters::tui::handlers::team_command::TeamLogArgs::default();
            run_team_log(ctx, &args).await;
        }
        AttachedRoute::Unavailable(remedy) => show_unavailable(ctx, remedy),
        AttachedRoute::NotificationLog => {
            // The leader restored the origin focus; S-LOG saves it as its caller.
            notification_log::open(ctx.state);
        }
        AttachedRoute::ClearToasts => {
            let _ = ctx.state.notifications.clear_toasts();
            ctx.state.needs_redraw = true;
        }
        AttachedRoute::Decisions => {
            // The attached client has no decision producers: every permission,
            // plan and question card stays inline (21-6 migrates them).
            ctx.info("No decisions waiting in this client");
        }
        AttachedRoute::Detach => return KeyFlow::Detach,
    }
    KeyFlow::Continue
}

/// The capability-qualified unavailable result of one retained chord.
fn show_unavailable(ctx: &mut KeyCtx<'_>, remedy: &str) {
    // `i`/`m` reuse the existing sign-in refusal (same notice as typed `/login`).
    let source = if remedy == chord_menu::ATTACHED_SIGN_IN {
        NoticeSource::Login
    } else {
        NoticeSource::Chord
    };
    notifications::info(ctx.state, source, remedy);
}

/// One key while S-LOG is open: the shared handler decides; a copy goes through
/// the clipboard helper (its result is itself a notice). The attached client has
/// no retry path, so `Retry` is never offered and never acted on.
fn log_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    let facts = log_facts(&ctx.conversation.id);
    match notification_log::handle_raw_key(ctx.state, key, &facts) {
        LogOutcome::Copy(text) => {
            crate::adapters::tui::handlers::clipboard::notify_copy(
                ctx.state,
                &text,
                &crate::adapters::tui::clipboard::copy_to_clipboard,
            );
        }
        LogOutcome::Closed | LogOutcome::Consumed | LogOutcome::Retry { .. } => {}
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

/// Help is passive: Esc / `?` close it and restore the origin (Input text is
/// untouched); scroll keys move it. Everything else is consumed.
fn help_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    if is_ctrl(&key, 'c') {
        ctx.state.focus = ctx.state.help_overlay.close();
        ctx.state.needs_redraw = true;
        return KeyFlow::Continue;
    }
    let delegated = match key.code {
        KeyCode::Esc | KeyCode::Up | KeyCode::Down => key.modifiers.is_empty(),
        KeyCode::Char(_) => !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        _ => false,
    };
    if delegated
        && let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default())
    {
        let _ = handle_input(ctx.state, &event);
    }
    KeyFlow::Continue
}

// ── slash popup ──────────────────────────────────────────────────────────────

/// A key while the slash popup is open. `Some` = consumed; `None` = the popup
/// was dismissed by an unrelated key which then continues as a normal key.
fn popup_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> Option<KeyFlow> {
    if is_ctrl(&key, 'c') {
        with_lent_input(ctx.state, ctx.input, cancel_autocomplete_popup);
        ctx.chrome.sync_slash(ctx.state, ctx.workspace);
        return Some(KeyFlow::Continue);
    }
    let delegated = match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Tab
        | KeyCode::Backspace
        | KeyCode::Esc
        | KeyCode::Enter => key.modifiers.is_empty(),
        KeyCode::Char(_) => !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        _ => false,
    };
    if !delegated {
        ctx.state.autocomplete.dismiss();
        ctx.state.needs_redraw = true;
        return None;
    }
    if let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default()) {
        with_lent_input(ctx.state, ctx.input, |state| {
            let _ = handle_input(state, &event);
        });
    }
    ctx.chrome.sync_slash(ctx.state, ctx.workspace);
    Some(KeyFlow::Continue)
}

// ── command palette ──────────────────────────────────────────────────────────

async fn open_palette(ctx: &mut KeyCtx<'_>) {
    let prior = ctx.state.focus.clone();
    ctx.chrome.begin_palette(ctx.conversation, ctx.read_only);
    ctx.state.command_palette.open(prior);
    ctx.state.focus = FocusState::Overlay(OverlayType::CommandPalette);
    ctx.state.needs_redraw = true;
    ctx.chrome.sync_palette(ctx.state, ctx.workspace).await;
}

async fn palette_key(ctx: &mut KeyCtx<'_>, key: KeyEvent) -> KeyFlow {
    use crate::adapters::tui::app::InputAction;
    if is_ctrl(&key, 'c') {
        ctx.state.focus = ctx
            .state
            .command_palette
            .dismiss()
            .unwrap_or(FocusState::Input);
        ctx.state.needs_redraw = true;
        return KeyFlow::Continue;
    }
    let delegated = match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Enter
        | KeyCode::Esc
        | KeyCode::Backspace
        | KeyCode::Tab => key.modifiers.is_empty(),
        KeyCode::Char(_) => !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        _ => false,
    };
    if !delegated {
        return KeyFlow::Continue;
    }
    let Some(event) = convert_crossterm_event(&Event::Key(key), &MouseConfig::default()) else {
        return KeyFlow::Continue;
    };
    let action = with_lent_input(ctx.state, ctx.input, |state| handle_input(state, &event));
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
        Some(target) => *ctx.retract_confirm = Some(target),
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
                    "/{name} is not available in an attached session. Detach (Ctrl+X d) and use it from a standalone `rustain` session."
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
    use super::*;
    use crate::adapters::daemon::protocol::{DaemonFrame, InterruptRefusal, TurnGeneration};
    use crate::adapters::tui::chord_menu::menu_items;
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
            handle_attached_key(&mut ctx, KeyEvent::new(code, modifiers)).await
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
            ('f', NoticeSource::Chord, "single layout"),
        ];
        for (key, source, remedy) in unavailable {
            let mut rig = Rig::new(false);
            rig.type_text("draft").await;
            assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
            assert!(rig.state.which_key.active, "{key}: leader open");
            assert_eq!(rig.ch(key).await, KeyFlow::Continue, "{key}");
            assert!(!rig.state.which_key.active, "{key}: leader consumed");
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
            let spec = chord_menu::spec_for(key).unwrap();
            assert!(
                matches!(spec.attached, AttachedRoute::Unavailable(r) if r == message),
                "{key}: result is the spec's remedy verbatim"
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
        assert_eq!(
            typed.notice(NoticeSource::Login),
            Some(chord_menu::ATTACHED_SIGN_IN)
        );

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
        assert!(rig.state.help_overlay.active);
        assert_eq!(rig.state.focus, FocusState::Overlay(OverlayType::Help));
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(!rig.state.help_overlay.active);
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
        assert!(!rig.state.which_key.active);
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
        let painted: BTreeSet<String> =
            menu_items(&TuiState::new(80, 24).which_key, ChordHost::Attached)
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
            rig.state.which_key.active,
            "untimed: stays open until a follow-up"
        );
        assert_eq!(rig.esc().await, KeyFlow::Continue);
        assert!(!rig.state.which_key.active);
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.input, "keep");

        // An unknown follow-up is consumed once with the origin restored.
        let mut rig = Rig::new(false);
        rig.state.focus = FocusState::Chat;
        rig.ctrl('x').await;
        assert_eq!(rig.ch('z').await, KeyFlow::Continue);
        assert!(!rig.state.which_key.active);
        assert_eq!(rig.state.focus, FocusState::Chat);
        assert!(rig.state.feedback_blocks.is_empty());
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
        assert!(rig.state.command_palette.active);
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(!rig.state.command_palette.active);
        assert!(rig.frames().is_empty());
        rig.ctrl('x').await;
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(!rig.state.which_key.active);
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
        assert!(rig.state.command_palette.active);
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
        assert!(!rig.state.command_palette.active);
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
    async fn story21_2_attached_palette_runs_commands_without_touching_the_draft() {
        let mut rig = Rig::new(false);
        rig.type_text("half-written").await;
        rig.ctrl('p').await;
        rig.type_text("ml").await;
        assert_eq!(rig.palette_names().first().map(String::as_str), Some("/ml"));
        assert_eq!(rig.enter().await, KeyFlow::Continue);
        assert!(rig.state.multiline_mode, "palette /ml toggles");
        assert_eq!(rig.input, "half-written", "the draft is never the command");
        assert!(!rig.state.command_palette.active);
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
        assert!(rig.state.which_key.active);
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
        rig.ch('n').await;
        assert!(rig.retract_confirm.is_none() && rig.frames().is_empty());
        rig.retract_confirm = Some(("peer-response-node-a".to_owned(), "first".to_owned()));
        rig.ch('y').await;
        let frames = rig.frames();
        assert!(matches!(
            frames.as_slice(),
            [ClientFrame::RetractAutoResponse { message_id, target_seq: None }]
                if message_id == "peer-response-node-a"
        ));

        // Stale target: the row was retracted meanwhile — `y` sends nothing.
        let mut rig = Rig::new(false);
        rig.retractable("peer-response-node-a", "auto reply");
        rig.retract_confirm = Some(("peer-response-node-a".to_owned(), "auto".to_owned()));
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
        assert!(rig.state.autocomplete.active);
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
        assert!(!rig.state.autocomplete.active);
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
        assert!(!rig.state.autocomplete.active);
        rig.enter().await;
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(user_text(&frames[0]), Some("/clear"));

        // A slash after other text is plain text — no popup.
        rig.type_text("a/").await;
        assert!(!rig.state.autocomplete.active);
        assert_eq!(rig.input, "a/");
    }

    #[tokio::test]
    async fn story21_2_attached_paste_is_blocked_while_an_overlay_owns_the_keyboard() {
        let mut rig = Rig::new(false);
        assert!(!paste_blocked(&rig.state));
        rig.ctrl('x').await;
        assert!(paste_blocked(&rig.state), "which-key");
        rig.esc().await;
        rig.ctrl('p').await;
        assert!(paste_blocked(&rig.state), "palette");
        rig.esc().await;
        rig.ctrl('x').await;
        rig.ch('n').await;
        assert!(paste_blocked(&rig.state), "S-LOG");
        rig.esc().await;
        rig.ctrl('q').await; // idle: exits directly, no frame
        rig.type_text("draft").await;
        rig.ctrl('q').await;
        assert!(paste_blocked(&rig.state), "S-QUIT");
        rig.esc().await;
        assert!(!paste_blocked(&rig.state));
    }

    // ── the pre-existing owners keep their keys ──────────────────────────────

    #[tokio::test]
    async fn story21_2_attached_consent_card_keeps_its_keys_and_ctrl_c_declines_it() {
        fn consent() -> PendingConsentApproval {
            PendingConsentApproval {
                request_id: crate::domain::models::RequestId::new(),
                tool: "a2a/sender-consent".to_owned(),
                input_preview: "consent".to_owned(),
            }
        }
        let mut rig = Rig::new(false);
        rig.consent.push_back(consent());
        rig.ch('y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse {
                outcome: crate::domain::models::ApprovalOutcome::Once,
                ..
            }]
        ));
        // Ctrl+C is "back one frame": the card's decline, never an exit/interrupt.
        rig.consent.push_back(consent());
        assert_eq!(rig.ctrl('c').await, KeyFlow::Continue);
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ApprovalResponse {
                outcome: crate::domain::models::ApprovalOutcome::Reject { .. },
                ..
            }]
        ));
        assert!(rig.consent.is_empty());
        assert!(rig.state.quit_confirm.is_none());
    }

    // ── draw: the attached overlays at 60 / 80 / 130 columns ─────────────────

    fn paint(state: &TuiState, width: u16) -> Vec<String> {
        let height = 24;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let chat_pane = Rect::new(0, 0, width, height - 5);
                let input_area = Rect::new(0, height - 5, width, 4);
                let status_bar = Rect::new(0, height - 1, width, 1);
                paint_overlays(
                    f,
                    area,
                    chat_pane,
                    input_area,
                    status_bar,
                    state,
                    &log_facts("conversation-1"),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
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
            for item in menu_items(&rig.state.which_key, ChordHost::Attached) {
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
        // Ctrl+Q closes the log first, then asks to quit (idle: exits directly).
        assert_eq!(rig.ctrl('q').await, KeyFlow::Quit);
        assert!(rig.state.notification_log.is_none());
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
}
