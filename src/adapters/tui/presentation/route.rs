//! Keyboard ownership before either host's legacy dispatch. No transport effects here.
use super::Surface;
use crate::adapters::tui::keymap::{
    table::{GlobalCmd, LeaderCmd},
    table_input::InputCmd,
    table_overlay::OverlayCmd,
};
use crate::adapters::tui::{
    app::{self, InputAction},
    keymap::{self, CommandId, Context, Host, Key, Resolved},
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
    /// Local frame command, already applied inside the settlement boundary.
    Action(InputAction),
    /// Attached host effects retain their async chrome/transport executors.
    Frame(Context),
    Leader,
    OpenPalette,
}

impl RouteEffect {
    pub fn into_action(self) -> InputAction {
        match self {
            Self::Consumed => InputAction::Consumed,
            Self::QuitRequest => InputAction::Quit,
            Self::QuitConfirmed => InputAction::QuitConfirmed,
            Self::Log(outcome) => InputAction::NotificationLog(outcome),
            Self::Action(action) => action,
            Self::Frame(_) | Self::Leader | Self::OpenPalette => {
                unreachable!("attached-only presentation effect in local dispatch")
            }
        }
    }
}

pub fn too_small(state: &TuiState) -> bool {
    state.terminal_width < 60 || state.terminal_height < 16
}

/// The attached paste ingress borrows its text; checking ownership must not copy it.
pub fn blocks_paste(state: &TuiState) -> bool {
    too_small(state) || !state.modal_stack.is_empty() || state.pending_leader.is_some()
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
    // Quit owns every key even while a leader remains pending underneath it.
    if state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::QuitConfirm)
    {
        return Some(match quit_confirm::execute(state, host, key) {
            QuitKeyOutcome::Confirmed => RouteEffect::QuitConfirmed,
            QuitKeyOutcome::Cancelled | QuitKeyOutcome::Consumed => RouteEffect::Consumed,
        });
    }
    // Attached quit always precedes cards. Local frame/window owners use
    // their rows; base dispatch still clears prefixes and retains card policy.
    let quit_context = if host == Host::Attached {
        Some(Context::Global)
    } else if state.pending_leader.is_some() || !state.modal_stack.is_empty() {
        app::focus_context(state)
    } else {
        app::overlay_focus_context(state).or_else(|| match app::focus_context(state) {
            Some(context @ (Context::ReverseSearch | Context::Autocomplete)) => Some(context),
            _ => None,
        })
    };
    if let Some(context) = quit_context
        && matches!(keymap::resolve(context, host, &[key]), Resolved::Hit(row)
            if matches!(row.command,
                CommandId::Global(GlobalCmd::QuitRequest)
                    | CommandId::Leader(LeaderCmd::QuitRequest)
                    | CommandId::Overlay(OverlayCmd::QuitRequest)))
    {
        return Some(RouteEffect::QuitRequest);
    }
    if state.pending_leader.is_some() {
        return Some(if Key::from_domain(event).is_none() {
            RouteEffect::Consumed
        } else if host == Host::Local {
            RouteEffect::Action(app::handle_leader_key(state, key))
        } else {
            RouteEffect::Leader
        });
    }
    let surface = state.modal_stack.top()?.surface;
    if Key::from_domain(event).is_none() {
        return Some(RouteEffect::Consumed);
    }
    if surface == Surface::CommandPalette && state.command_palette.refusal.take().is_some() {
        state.needs_redraw = true;
    }
    let context = surface.context();
    if let Resolved::Hit(row) = keymap::resolve(context, host, &[key]) {
        match row.command {
            CommandId::Global(GlobalCmd::OpenLeader) => {
                app::open_leader(state);
                return Some(RouteEffect::Consumed);
            }
            CommandId::Input(InputCmd::OpenPalette) => {
                return Some(if host == Host::Local {
                    RouteEffect::Action(app::open_palette(state))
                } else {
                    RouteEffect::OpenPalette
                });
            }
            _ => {}
        }
    }
    Some(match surface {
        Surface::NotificationLog => match notification_log::handle_key(state, host, key, facts) {
            LogOutcome::Consumed | LogOutcome::Closed => RouteEffect::Consumed,
            LogOutcome::Quit => RouteEffect::QuitRequest,
            effect => RouteEffect::Log(effect),
        },
        Surface::Help | Surface::CommandPalette if host == Host::Local => {
            RouteEffect::Action(app::dispatch_frame_key(state, context, key))
        }
        Surface::Help | Surface::CommandPalette => RouteEffect::Frame(context),
        Surface::QuitConfirm => unreachable!("Quit handled before the pending leader"),
    })
}
