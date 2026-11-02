//! Keyboard ownership before either host's legacy dispatch. No transport effects here.
use super::Surface;
use crate::adapters::tui::keymap::{
    table::{GlobalCmd, LeaderCmd},
    table_overlay::OverlayCmd,
};
use crate::adapters::tui::{
    app::InputAction,
    keymap::{self, CommandId, Host, Key, Resolved},
    notification_log::{self, LogFacts, LogOutcome},
    quit_confirm::{self, QuitKeyOutcome},
    state::TuiState,
};
use crate::domain::{
    events::DomainInputEvent,
    models::{FocusState, visual::OverlayType},
};

#[derive(Debug, PartialEq, Eq)]
pub enum RouteEffect {
    Consumed,
    QuitRequest,
    QuitConfirmed,
    Log(LogOutcome),
}

impl RouteEffect {
    pub fn into_action(self) -> InputAction {
        match self {
            Self::Consumed => InputAction::Consumed,
            Self::QuitRequest => InputAction::Quit,
            Self::QuitConfirmed => InputAction::QuitConfirmed,
            Self::Log(outcome) => InputAction::NotificationLog(outcome),
        }
    }
}

pub fn too_small(state: &TuiState) -> bool {
    state.terminal_width < 60 || state.terminal_height < 16
}

/// The attached paste ingress borrows its text; checking ownership must not copy it.
pub fn blocks_paste(state: &TuiState) -> bool {
    too_small(state) || !state.modal_stack.is_empty()
}

fn legacy_surface(state: &TuiState) -> bool {
    if matches!(
        &state.focus,
        FocusState::Overlay(OverlayType::Confirmation(_))
    ) {
        return false;
    }
    state.which_key.active
        || state.command_palette.active
        || state.help_overlay.active
        || state.autocomplete.active
        || state.reverse_search.active
        || matches!(&state.focus, FocusState::Overlay(overlay) if !matches!(overlay, OverlayType::Confirmation(_)))
}

/// `None` lets the host's existing base/legacy dispatcher run. Only system events
/// may pass a live frame, and no user event acts below the minimum terminal size.
pub fn route(
    state: &mut TuiState,
    event: &DomainInputEvent,
    host: Host,
    facts: &LogFacts,
) -> Option<RouteEffect> {
    if matches!(
        event,
        DomainInputEvent::Resize(..) | DomainInputEvent::FocusGained | DomainInputEvent::FocusLost
    ) {
        return None;
    }
    if too_small(state) {
        return Some(RouteEffect::Consumed);
    }
    let key = Key::from_domain(event).unwrap_or(Key::Other);
    if let Some(top) = state.modal_stack.top() {
        let effect = match top.surface {
            Surface::NotificationLog => match notification_log::handle_key(state, host, key, facts)
            {
                LogOutcome::Consumed | LogOutcome::Closed => RouteEffect::Consumed,
                LogOutcome::Quit => RouteEffect::QuitRequest,
                effect => RouteEffect::Log(effect),
            },
            Surface::QuitConfirm => match quit_confirm::execute(state, host, key) {
                QuitKeyOutcome::Confirmed => RouteEffect::QuitConfirmed,
                QuitKeyOutcome::Cancelled | QuitKeyOutcome::Consumed => RouteEffect::Consumed,
            },
        };
        return Some(effect);
    }
    // The registered quit route covers legacy windows without closing/resetting
    // them. Inline cards keep their existing isolated routes until 21-6.
    if legacy_surface(state)
        && let Some(context) = crate::adapters::tui::app::focus_context(state)
        && matches!(keymap::resolve(context, host, &[key]), Resolved::Hit(row)
            if matches!(row.command, CommandId::Global(GlobalCmd::QuitRequest)
                | CommandId::Leader(LeaderCmd::QuitRequest)
                | CommandId::Overlay(OverlayCmd::QuitRequest)))
    {
        return Some(RouteEffect::QuitRequest);
    }
    None
}
