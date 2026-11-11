//! Periodic display state mutations (the Room poll gate, the status tick); dispatch and
//! rendering stay in run. The sidebar panels are gone (Story 21.7): History is a frame
//! (`history_window`), the rest are Mission Control tabs, and the column that remains paints the
//! read-only monitor body.

use super::room_bridge;
use crate::adapters::tui::{state::TuiState, task_panel_handlers};
use crate::domain::models::{Conversation, StatusState};

/// Advance only poll state; the root loop owns the durable-head read. The Room is polled only
/// while it is *presented* (A14, RU-28): the last completed draw painted its body on the
/// Mission Control frame that is still on top, or in the monitor column that is still shown.
/// Held under another frame, on another tab, or after an interrupted draw it is not polled and
/// the counter starts over.
pub fn room_head_poll_due(state: &mut TuiState, tick_ms: u64) -> bool {
    if crate::adapters::tui::mission_control::room_projection_presented(state) {
        state
            .room_panel
            .head_poll_due(tick_ms, room_bridge::ROOM_HEAD_POLL_INTERVAL_MS)
    } else {
        state.room_panel.reset_head_poll();
        false
    }
}

/// Story 21.9 (review P4): the live `◇ Thinking… · <elapsed>` row advances with
/// the clock, so a draw is due on every tick while the model is thinking, even
/// between chunks.
pub(crate) fn tick_live_reasoning(
    state: &mut TuiState,
    streaming: &crate::domain::models::StreamingState,
) {
    if !streaming.thinking_buffer.is_empty() {
        state.needs_redraw = true;
    }
}

pub(crate) fn tick_status(state: &mut TuiState, conversation: &Conversation, tick_ms: u64) {
    if let StatusState::Executing { elapsed_ms, .. } = &mut state.status {
        *elapsed_ms += tick_ms;
        state.needs_redraw = true;
    }
    // Running-task elapsed display must remain live between domain events.
    if crate::adapters::tui::mission_control::tasks_surface_open(state)
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
    use crate::adapters::tui::history_window;
    use crate::adapters::tui::presentation::{Surface, reducer};
    use crate::domain::events::{DomainInputEvent, DomainKey};
    use crate::domain::models::palette::{PaletteAction, SurfaceTarget};
    use crate::domain::models::{FocusState, generate_conversation_id};
    use crate::domain::services::session_index::SessionIndex;
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
        crate::adapters::tui::layout::compute_layout(
            Rect::new(0, 0, state.terminal_width, 30),
            &state.theme,
            "",
            1,
            state.monitor,
        )
        .expect("layout")
        .sidebar
        .is_some()
    }

    fn top_surface(state: &TuiState) -> Option<Surface> {
        state.modal_stack.top().map(|frame| frame.surface)
    }

    /// The host's effect of `InputAction::OpenHistory`.
    fn open_history(state: &mut TuiState) {
        history_window::open(state, &SessionIndex::new(), 0);
    }

    #[test]
    fn story21_7_the_history_chord_and_the_palette_open_the_window_at_every_width() {
        for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
            let mut state = TuiState::new(width, height);
            assert_eq!(leader(&mut state, 'h'), InputAction::OpenHistory, "{width}");
            open_history(&mut state);
            assert_eq!(top_surface(&state), Some(Surface::History), "{width}");
            // No refusal toast, no column, the monitor untouched, the base focus where it was.
            assert_eq!(state.notifications.records().count(), 0, "{width}");
            assert!(!painted_sidebar(&state) && !state.monitor, "{width}");
            assert_eq!(state.focus, FocusState::Input, "{width}");
        }
        // The palette entry returns the same action.
        let mut palette = TuiState::new(80, 24);
        assert_eq!(
            dispatch_palette_action(
                &mut palette,
                PaletteAction::OpenSurface(SurfaceTarget::History)
            ),
            InputAction::OpenHistory
        );
    }

    #[test]
    fn story21_7_a_history_window_over_help_keeps_help_beneath_and_the_interrupted_base() {
        let mut state = TuiState::new(130, 30);
        assert_eq!(leader(&mut state, '?'), InputAction::Consumed);
        let help = state.modal_stack.top().expect("Help opened").clone();
        let base = state.focus.clone();
        open_history(&mut state);
        assert_eq!(top_surface(&state), Some(Surface::History));
        assert_eq!(
            state.focus, base,
            "the window cannot replace the interrupted base"
        );
        reducer::back(&mut state);
        assert_eq!(state.modal_stack.top(), Some(&help));
        assert_eq!(state.focus, base);
    }

    #[test]
    fn story21_7_every_panel_chord_opens_a_frame_and_never_turns_the_monitor_on() {
        // `s` and `a` are Mission Control's Agents tab and the Adapters window since Story
        // 21.7 (7-C): they open frames and never reveal the column.
        let mut agents = TuiState::new(130, 30);
        assert_eq!(
            leader(&mut agents, 's'),
            InputAction::OpenMissionControl(crate::adapters::tui::mission_control::McTab::Agents)
        );
        let mut adapters = TuiState::new(130, 30);
        assert_eq!(leader(&mut adapters, 'a'), InputAction::OpenAdapters);
        for state in [&agents, &adapters] {
            assert!(!painted_sidebar(state));
            assert!(!state.monitor);
        }
        // `t` is Mission Control's Tasks tab since Story 21.7 (7-D): a frame, never the column.
        let mut tasks = TuiState::new(130, 30);
        assert_eq!(
            leader(&mut tasks, 't'),
            InputAction::OpenMissionControl(crate::adapters::tui::mission_control::McTab::Tasks)
        );
        assert!(crate::adapters::tui::presentation::is_open(
            &tasks,
            Surface::MissionControl
        ));
        assert!(!painted_sidebar(&tasks));
        assert!(!tasks.monitor);
        // Control: with the monitor on `Ctrl+X h` still opens the window and the column stays.
        let mut monitor = TuiState::new(130, 30);
        monitor.monitor = true;
        assert_eq!(leader(&mut monitor, 'h'), InputAction::OpenHistory);
        open_history(&mut monitor);
        assert_eq!(top_surface(&monitor), Some(Surface::History));
        assert!(painted_sidebar(&monitor));
    }

    #[test]
    fn story21_3_ctrl_h_is_inert() {
        let mut state = TuiState::new(130, 30);
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlH));
        assert!(
            matches!(action, InputAction::Consumed | InputAction::Ignored),
            "{action:?}"
        );
        assert!(!state.monitor && state.focus == FocusState::Chat);
        assert!(state.modal_stack.is_empty());
    }

    #[test]
    fn story21_7_running_task_tick_repaints_only_while_a_tasks_surface_is_open() {
        use crate::domain::models::{Plan, PlanStatus, PlanTask, PlanTaskStatus};
        let mut conversation = conversation();
        conversation.plans.insert(
            "plan".into(),
            Plan {
                id: "plan".into(),
                title: "Running".into(),
                tasks: vec![PlanTask {
                    number: 1,
                    title: "Task 1".into(),
                    description: String::new(),
                    depends_on: vec![],
                    status: PlanTaskStatus::Running,
                    started_at_ms: Some(1),
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                }],
                estimated_effort: None,
                status: PlanStatus::Executing,
                created_at: 1_700_000_000,
                resolved_at: None,
                host_message_id: None,
            },
        );
        // No Tasks surface (the History window is not one): the tick stays quiet.
        let mut state = TuiState::new(130, 30);
        open_history(&mut state);
        state.needs_redraw = false;
        tick_status(&mut state, &conversation, 50);
        assert!(!state.needs_redraw, "no Tasks surface: no repaint");
        // The monitor column on its default tab (Tasks) is one: the elapsed time keeps moving.
        let mut monitor = TuiState::new(130, 30);
        monitor.monitor = true;
        monitor.needs_redraw = false;
        tick_status(&mut monitor, &conversation, 50);
        assert!(
            monitor.needs_redraw,
            "monitor on Tasks: running task repaints"
        );
        // Mission Control on the Tasks tab: the elapsed time keeps moving.
        let mut hub = TuiState::new(130, 30);
        assert_eq!(
            leader(&mut hub, 't'),
            InputAction::OpenMissionControl(crate::adapters::tui::mission_control::McTab::Tasks)
        );
        hub.needs_redraw = false;
        tick_status(&mut hub, &conversation, 50);
        assert!(hub.needs_redraw, "Tasks tab open: running task repaints");
    }
}
