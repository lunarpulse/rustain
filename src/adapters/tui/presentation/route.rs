//! Keyboard ownership before either host's legacy dispatch. No transport effects here.
use super::Surface;
use crate::adapters::tui::keymap::{
    table::{GlobalCmd, LeaderCmd},
    table_input::InputCmd,
    table_overlay::OverlayCmd,
};
use crate::adapters::tui::{
    app::{self, InputAction},
    decision_queue::{self, ListOutcome, PaneOutcome},
    keymap::{self, CommandId, Context, Host, Key, Resolved},
    notification_log::{self, LogFacts, LogOutcome},
    quit_confirm::{self, QuitKeyOutcome},
    state::TuiState,
};
use crate::domain::events::DomainInputEvent;

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
    /// A decision rail key of the attached host: the item stays queued while the host
    /// sends its `ClientFrame` (`decision_queue::settle_sent` retires it after the send).
    Decision(decision_queue::DecisionEffect),
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
            Self::Frame(_) | Self::Decision(_) | Self::Leader | Self::OpenPalette => {
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

/// Borrowed text goes to the current editable owner, never its covered composer.
pub fn paste(state: &mut TuiState, text: &str, host: Host) -> Option<InputAction> {
    if too_small(state)
        || state.pending_leader.is_some()
        || state
            .modal_stack
            .top()
            .is_some_and(|frame| frame.surface == Surface::QuitConfirm)
    {
        return Some(InputAction::Consumed);
    }
    app::try_with_card_focus_reconcile(state, |state| app::paste_query(state, text, host))
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
    if Key::from_domain(event).is_some()
        && state
            .modal_stack
            .top()
            .is_some_and(|frame| frame.surface == Surface::Decision)
    {
        decision_queue::clear_feedback(state);
    }
    if (!state.modal_stack.is_empty() || state.pending_leader.is_some())
        && matches!(key, Key::Char(c) if !c.is_control())
    {
        state.last_printable_at = Some(state.notifications.now());
    }
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
        match app::focus_context(state) {
            Some(context @ (Context::ReverseSearch | Context::Autocomplete)) => Some(context),
            _ => None,
        }
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
    if let DomainInputEvent::Paste(text) = event
        && let Some(action) = paste(state, text, host)
    {
        return Some(RouteEffect::Action(action));
    }
    let surface = state.modal_stack.top()?.surface;
    if surface == Surface::Decision && decision_queue::current(state, facts).is_none() {
        super::reducer::close(state, Surface::Decision);
        return route(state, event, host, facts);
    }
    // Search's Stage is its body: only this top owner permits wheel navigation.
    if surface == Surface::Search
        && let DomainInputEvent::MouseScroll(delta) = event
    {
        return Some(RouteEffect::Action(InputAction::MouseScroll(*delta)));
    }
    if matches!(surface, Surface::ToolInspector(_))
        && let DomainInputEvent::MouseScroll(delta) = event
    {
        return Some(RouteEffect::Action(
            crate::adapters::tui::tool_inspector::on_wheel(state, *delta),
        ));
    }
    if matches!(surface, Surface::BlockViewer(_))
        && let DomainInputEvent::MouseScroll(delta) = event
    {
        return Some(RouteEffect::Action(
            crate::adapters::tui::block_viewer::on_wheel(state, *delta),
        ));
    }
    // The thread window's wheel is state-only in both hosts.
    if surface == Surface::ThreadInspector
        && let DomainInputEvent::MouseScroll(delta) = event
    {
        crate::adapters::tui::thread_inspector::on_wheel(state, *delta);
        return Some(RouteEffect::Consumed);
    }
    if surface == Surface::Login && Key::from_domain(event).is_none() {
        return Some(RouteEffect::Action(
            crate::adapters::tui::handlers::login::on_key(state, event),
        ));
    }
    if Key::from_domain(event).is_none() {
        return Some(RouteEffect::Consumed);
    }
    if surface == Surface::CommandPalette && state.command_palette.refusal.take().is_some() {
        state.needs_redraw = true;
    }
    let context = super::frame_context(state, surface);
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
    // The reply field edits its own text; only `Enter`/`Esc` reach the attached host.
    if context == Context::ThreadReply {
        let own = matches!(
            keymap::resolve(context, host, &[key]),
            Resolved::Hit(row) if matches!(row.command, CommandId::Thread(_))
        );
        return Some(if own && host == Host::Attached {
            RouteEffect::Frame(context)
        } else {
            if !own {
                app::dispatch_frame_key(state, context, key, host);
            }
            RouteEffect::Consumed
        });
    }
    if context == Context::InspectorSearch {
        let action = app::dispatch_frame_key(state, context, key, host);
        return Some(if host == Host::Local {
            RouteEffect::Action(action)
        } else {
            assert_eq!(
                action,
                InputAction::Consumed,
                "attached inspector is state-only"
            );
            RouteEffect::Consumed
        });
    }
    Some(match surface {
        Surface::ToolInspector(_) => RouteEffect::Action(
            crate::adapters::tui::tool_inspector::on_key(state, key, host),
        ),
        Surface::BlockViewer(_) => {
            RouteEffect::Action(crate::adapters::tui::block_viewer::on_key(state, key, host))
        }
        // The attached host executes the thread keys itself (its writer frames are async).
        Surface::ThreadInspector if host == Host::Local => RouteEffect::Action(
            match crate::adapters::tui::thread_inspector::on_key(state, key, host, None) {
                crate::adapters::tui::thread_inspector::ThreadOutcome::Page { earlier } => {
                    InputAction::ThreadPage { earlier }
                }
                crate::adapters::tui::thread_inspector::ThreadOutcome::Retract(
                    crate::adapters::tui::thread_inspector::RetractPlan::Item { peer, item },
                ) => InputAction::ThreadRetract { peer, item },
                _ => InputAction::Consumed,
            },
        ),
        Surface::ThreadInspector => RouteEffect::Frame(context),
        Surface::Decision => match decision_queue::handle_domain_event(state, event, facts, host) {
            Some(PaneOutcome::Effect(effect)) if host == Host::Attached => {
                RouteEffect::Decision(effect)
            }
            Some(PaneOutcome::Effect(effect)) => RouteEffect::Action(effect.into_action()),
            Some(PaneOutcome::Quit) => RouteEffect::QuitRequest,
            // `c` on any decision frame copies the item's original text (both hosts).
            Some(PaneOutcome::Copy(text)) => {
                let copier = state.clipboard_copier.clone();
                match copier {
                    Some(copier) => crate::adapters::tui::handlers::clipboard::notify_copy(
                        state, &text, &*copier,
                    ),
                    None => crate::adapters::tui::handlers::clipboard::notify_copy(
                        state,
                        &text,
                        &crate::adapters::tui::clipboard::copy_to_clipboard,
                    ),
                }
                RouteEffect::Consumed
            }
            Some(PaneOutcome::Consumed) | None => RouteEffect::Consumed,
        },
        Surface::DecisionQueue => match decision_queue::handle_list_key(state, host, key, facts) {
            ListOutcome::Quit => RouteEffect::QuitRequest,
            ListOutcome::Consumed => RouteEffect::Consumed,
            ListOutcome::OpenTab {
                conversation_id,
                key,
            } => RouteEffect::Action(InputAction::OpenDecisionTab {
                conversation_id,
                key,
            }),
        },
        Surface::NotificationLog => match notification_log::handle_key(state, host, key, facts) {
            LogOutcome::Consumed | LogOutcome::Closed => RouteEffect::Consumed,
            LogOutcome::Quit => RouteEffect::QuitRequest,
            effect => RouteEffect::Log(effect),
        },
        Surface::Login => {
            RouteEffect::Action(crate::adapters::tui::handlers::login::on_key(state, event))
        }
        Surface::WaveOverlay if host == Host::Local => RouteEffect::Action(
            crate::adapters::tui::handlers::wave_overlay::on_key(state, key, host),
        ),
        Surface::Help
        | Surface::CommandPalette
        | Surface::ModelSelector
        | Surface::ProfileSwitcher
        | Surface::UsagePanel
        | Surface::Search
        | Surface::BookmarkList
        | Surface::CrossSearch
            if host == Host::Local =>
        {
            RouteEffect::Action(app::dispatch_frame_key(state, context, key, host))
        }
        Surface::Help
        | Surface::CommandPalette
        | Surface::ModelSelector
        | Surface::ProfileSwitcher
        | Surface::UsagePanel
        | Surface::Search
        | Surface::BookmarkList
        | Surface::CrossSearch
        | Surface::WaveOverlay => RouteEffect::Frame(context),
        Surface::QuitConfirm => unreachable!("Quit handled before the pending leader"),
    })
}
