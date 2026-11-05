//! Sidebar and periodic display state mutations; dispatch and rendering stay in run.

use super::room_bridge;
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::{layout, state::TuiState, task_panel_handlers};
use crate::domain::models::visual::{DensityMode, PanelType};
use crate::domain::models::{Conversation, FocusState, NoticeLevel, StatusState};

pub(crate) enum PanelOpenOutcome {
    Opened,
    Notice(task_panel_handlers::PendingNotice),
    Handled,
}

/// Quiet reveal (Story 21.3 A6, EV-9): Focus density hides the sidebar column,
/// so a panel opened there would take focus invisibly. Opening any panel in
/// Focus density moves to Monitor without a toast and records the move as the
/// user's choice (`density_user_overridden`: no auto-density trigger reads it
/// yet); the status density chip shows the change.
fn reveal_sidebar(state: &mut TuiState) {
    if state.density_mode == DensityMode::Focus {
        state.density_mode = DensityMode::Monitor;
        state.density_user_overridden = true;
    }
}

/// Open (or toggle closed) a sidebar panel and move focus to it: the whole open
/// is this one function (A6); the caller only runs the async source bridges.
pub(crate) fn open_panel(
    state: &mut TuiState,
    conversation: &Conversation,
    panel_type: PanelType,
) -> PanelOpenOutcome {
    if panel_type == PanelType::Tasks {
        let terminal_width = state.terminal_width;
        let outcome = task_panel_handlers::handle_open_panel_tasks(
            state,
            conversation,
            terminal_width,
            layout::SIDEBAR_MIN_WIDTH,
        );
        if outcome.opened {
            reveal_sidebar(state);
            let selected = state.sidebar_selected;
            reducer::set_base(
                state,
                FocusState::Sidebar {
                    panel: panel_type,
                    selected,
                },
            );
        } else if outcome.closed {
            reducer::set_base(state, FocusState::Chat);
        }
        if let Some(notice) = outcome.notice {
            PanelOpenOutcome::Notice(notice)
        } else {
            PanelOpenOutcome::Handled
        }
    } else if state.sidebar_visible
        && state.sidebar_panel == Some(panel_type)
        && state.density_mode != DensityMode::Focus
    {
        state.sidebar_visible = false;
        state.sidebar_panel = None;
        state.sidebar_entry_count = 0;
        reducer::set_base(state, FocusState::Chat);
        state.needs_redraw = true;
        PanelOpenOutcome::Handled
    } else if state.terminal_width >= layout::SIDEBAR_MIN_WIDTH {
        reveal_sidebar(state);
        state.sidebar_visible = true;
        state.sidebar_panel = Some(panel_type);
        if panel_type == PanelType::Adapters {
            state.sidebar_entry_count =
                crate::adapters::tui::widgets::adapter_status_panel::port_count();
            if state.sidebar_selected >= state.sidebar_entry_count {
                state.sidebar_selected = 0;
            }
        } else if panel_type == PanelType::Agents {
            state.sidebar_entry_count = state.agent_panel_state.cached_entries.len();
            if state.sidebar_selected >= state.sidebar_entry_count && state.sidebar_entry_count > 0
            {
                state.sidebar_selected = state.sidebar_entry_count - 1;
            }
        }
        let selected = state.sidebar_selected;
        reducer::set_base(
            state,
            FocusState::Sidebar {
                panel: panel_type,
                selected,
            },
        );
        state.needs_redraw = true;
        PanelOpenOutcome::Opened
    } else {
        let message = if panel_type == PanelType::Adapters {
            "Adapter Status: terminal too narrow for sidebar — widen to ≥120 cols or use /memory|/persona|... slash commands"
        } else if panel_type == PanelType::Agents {
            "Subagent panel: terminal too narrow for sidebar — widen to ≥120 cols."
        } else {
            "Panel requires terminal width >= 120 cols."
        };
        PanelOpenOutcome::Notice(task_panel_handlers::PendingNotice {
            conversation_id: conversation.id.clone(),
            level: NoticeLevel::Warning,
            message: message.to_string(),
        })
    }
}

/// Advance only poll state; the root loop owns the durable-head read.
pub(crate) fn room_head_poll_due(state: &mut TuiState, tick_ms: u64) -> bool {
    if state.sidebar_visible && state.sidebar_panel == Some(PanelType::Room) {
        state
            .room_panel
            .head_poll_due(tick_ms, room_bridge::ROOM_HEAD_POLL_INTERVAL_MS)
    } else {
        state.room_panel.reset_head_poll();
        false
    }
}

pub(crate) fn tick_status(state: &mut TuiState, conversation: &Conversation, tick_ms: u64) {
    if let StatusState::Executing { elapsed_ms, .. } = &mut state.status {
        *elapsed_ms += tick_ms;
        state.needs_redraw = true;
    }
    // Running-task elapsed display must remain live between domain events.
    if state.sidebar_visible
        && state.sidebar_panel == Some(PanelType::Tasks)
        && task_panel_handlers::any_plan_has_running_task(conversation)
    {
        state.needs_redraw = true;
    }
    // Story 21.1 A4: a notification deadline that has passed asks for the redraw
    // that sweeps it; nothing is decremented per tick.
    crate::adapters::tui::notifications::tick(state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::app::{InputAction, dispatch_palette_action, handle_input};
    use crate::domain::events::{DomainInputEvent, DomainKey};
    use crate::domain::models::generate_conversation_id;
    use crate::domain::models::palette::PaletteAction;
    use crate::domain::models::visual::DensityMode;
    use ratatui::layout::Rect;

    fn conversation() -> Conversation {
        Conversation {
            id: generate_conversation_id(),
            title: "K5".into(),
            messages: vec![],
            turns: vec![],
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: Default::default(),
            fork_source: None,
            compaction: None,
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
        }
    }

    fn leader(state: &mut TuiState, follow_up: char) -> InputAction {
        handle_input(state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
        handle_input(state, &DomainInputEvent::KeyPress(follow_up))
    }

    /// The layout the event loop computes from this state (what is painted).
    fn painted_sidebar(state: &TuiState) -> bool {
        layout::compute_layout(
            Rect::new(0, 0, state.terminal_width, 30),
            &state.theme,
            "",
            1,
            state.sidebar_visible,
            state.density_mode,
        )
        .expect("layout")
        .sidebar
        .is_some()
    }

    fn open_through_the_door(
        state: &mut TuiState,
        key: char,
        panel: PanelType,
    ) -> PanelOpenOutcome {
        assert_eq!(leader(state, key), InputAction::OpenPanel(panel), "{key}");
        open_panel(state, &conversation(), panel)
    }

    #[test]
    fn story21_4_panel_effect_preserves_help_frame_and_interrupted_base() {
        use crate::adapters::tui::presentation::{Surface, reducer};

        let mut state = TuiState::new(130, 30);
        assert_eq!(leader(&mut state, '?'), InputAction::Consumed);
        let frame = state.modal_stack.top().expect("Help opened").clone();
        assert_eq!(frame.surface, Surface::Help);
        let base = state.focus.clone();

        assert!(matches!(
            open_panel(&mut state, &conversation(), PanelType::History),
            PanelOpenOutcome::Opened
        ));
        assert!(
            painted_sidebar(&state),
            "panel effect still reveals the sidebar"
        );
        assert_eq!(state.sidebar_panel, Some(PanelType::History));
        assert_eq!(state.modal_stack.top(), Some(&frame));
        assert_eq!(
            state.focus, base,
            "panel effect cannot replace interrupted base"
        );
        reducer::back(&mut state);
        assert!(state.modal_stack.is_empty());
        assert_eq!(state.focus, base);

        let mut empty = TuiState::new(130, 30);
        assert!(matches!(
            open_panel(&mut empty, &conversation(), PanelType::History),
            PanelOpenOutcome::Opened
        ));
        assert!(painted_sidebar(&empty));
        assert_eq!(
            empty.focus,
            FocusState::Sidebar {
                panel: PanelType::History,
                selected: empty.sidebar_selected,
            }
        );
    }

    #[test]
    fn story21_3_ctrl_x_h_reveals_history_in_focus_density() {
        let mut state = TuiState::new(130, 30);
        assert_eq!(state.density_mode, DensityMode::Focus);
        assert!(!painted_sidebar(&state));
        let outcome = open_through_the_door(&mut state, 'h', PanelType::History);
        assert!(matches!(outcome, PanelOpenOutcome::Opened));
        // The sidebar is really painted (the EV-9 defect painted nothing) and
        // focus is on the History panel inside it.
        assert!(painted_sidebar(&state), "History panel must be visible");
        assert_eq!(state.density_mode, DensityMode::Monitor);
        assert!(state.density_user_overridden);
        assert_eq!(state.sidebar_panel, Some(PanelType::History));
        assert_eq!(
            state.focus,
            FocusState::Sidebar {
                panel: PanelType::History,
                selected: state.sidebar_selected
            }
        );
        // Quiet: no toast, and not the Tasks default.
        assert_eq!(state.notifications.records().count(), 0);
        assert_ne!(state.sidebar_panel, Some(PanelType::Tasks));
        // A second `Ctrl+X h` closes it again.
        state.focus = FocusState::Input;
        let outcome = open_through_the_door(&mut state, 'h', PanelType::History);
        assert!(matches!(outcome, PanelOpenOutcome::Handled));
        assert!(!state.sidebar_visible && state.sidebar_panel.is_none());
        assert_eq!(state.focus, FocusState::Chat);
    }

    #[test]
    fn story21_3_every_panel_chord_reveals_and_monitor_density_is_not_touched() {
        for (key, panel) in [
            ('s', PanelType::Agents),
            ('a', PanelType::Adapters),
            ('r', PanelType::Room),
            ('e', PanelType::Artifacts),
            ('l', PanelType::TransparencyLog),
        ] {
            let mut state = TuiState::new(130, 30);
            let outcome = open_through_the_door(&mut state, key, panel);
            assert!(matches!(outcome, PanelOpenOutcome::Opened), "{key}");
            assert!(painted_sidebar(&state), "{key}: revealed");
            assert_eq!(
                state.sidebar_panel,
                Some(panel),
                "{key}: the REQUESTED panel only"
            );
        }
        // The Tasks route reveals too.
        let mut state = TuiState::new(130, 30);
        let outcome = open_through_the_door(&mut state, 't', PanelType::Tasks);
        assert!(matches!(outcome, PanelOpenOutcome::Handled));
        assert!(painted_sidebar(&state), "Tasks: revealed");
        assert_eq!(state.density_mode, DensityMode::Monitor);
        // Control: already in Monitor, nothing is overridden.
        let mut monitor = TuiState::new(130, 30);
        monitor.density_mode = DensityMode::Monitor;
        open_through_the_door(&mut monitor, 'h', PanelType::History);
        assert!(!monitor.density_user_overridden, "no reveal in Monitor");
        assert!(painted_sidebar(&monitor));
    }

    #[test]
    fn story21_3_narrow_terminal_answers_with_the_remedy_and_keeps_focus() {
        let mut state = TuiState::new(80, 24);
        let action = leader(&mut state, 'h');
        assert!(matches!(
            action,
            InputAction::Consumed | InputAction::Ignored
        ));
        assert_eq!(state.focus, FocusState::Input);
        assert!(!state.sidebar_visible);
        let notices: Vec<String> = state
            .notifications
            .records()
            .map(|r| r.message.clone())
            .collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("120 columns"), "{notices:?}");
    }

    #[test]
    fn story21_3_ctrl_h_is_inert_and_the_palette_history_entry_behaves_like_the_chord() {
        let mut state = TuiState::new(130, 30);
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlH));
        assert!(
            matches!(action, InputAction::Consumed | InputAction::Ignored),
            "{action:?}"
        );
        assert!(!state.sidebar_visible && state.focus == FocusState::Chat);
        // The palette route returns the same action and opens the same panel.
        let mut palette = TuiState::new(130, 30);
        let action =
            dispatch_palette_action(&mut palette, PaletteAction::OpenPanel(PanelType::History));
        assert_eq!(action, InputAction::OpenPanel(PanelType::History));
        assert!(matches!(
            open_panel(&mut palette, &conversation(), PanelType::History),
            PanelOpenOutcome::Opened
        ));
        assert!(painted_sidebar(&palette));
    }
}
