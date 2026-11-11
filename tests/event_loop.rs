use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;

/// AC4: Event processing sets needs_redraw flag.
// Covers: NFR2 (redraw efficiency)
#[test]
fn test_tick_needs_redraw_flag() {
    let mut state = TuiState::new(80, 24);

    // Initially needs_redraw is true (first render)
    assert!(state.needs_redraw);

    // Simulate clearing after render
    state.needs_redraw = false;
    assert!(!state.needs_redraw);

    // Input event should set needs_redraw
    rustain::adapters::tui::app::handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    assert!(state.needs_redraw);
}

/// AC4: Resize events update terminal dimensions and trigger redraw.
// Covers: NFR2 (redraw efficiency)
#[test]
fn test_resize_event_updates_state() {
    let mut state = TuiState::new(80, 24);
    state.needs_redraw = false;

    rustain::adapters::tui::app::handle_input(&mut state, &DomainInputEvent::Resize(120, 40));

    assert_eq!(state.terminal_width, 120);
    assert_eq!(state.terminal_height, 40);
    assert!(state.needs_redraw);
}

// Task 19.9: permissions remain FIFO in the shared session decision queue.
#[test]
fn test_permission_queue_push_pop() {
    use rustain::adapters::tui::decision_queue::{
        self, DecisionKind, DecisionSubject, PaneKey, PaneOutcome, PermissionInput,
        PermissionPayload, Responder,
    };
    use rustain::adapters::tui::notification_log::LogFacts;
    use rustain::adapters::tui::notifications::NotificationStore;
    use rustain::domain::clock::MockClock;
    use rustain::domain::models::tool_call::RequestId;
    use rustain::domain::models::{ApprovalSource, NotificationsConfig, ToolRisk};
    use std::sync::Arc;
    use std::time::Instant;

    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = TuiState::new(80, 24);
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    for (tool, input, risk) in [
        (
            "Bash",
            serde_json::json!({"command": "ls"}),
            ToolRisk::Elevated,
        ),
        (
            "Write",
            serde_json::json!({"file_path": "a.rs"}),
            ToolRisk::Standard,
        ),
    ] {
        decision_queue::raise_permission(
            &mut state,
            PermissionPayload::new(
                RequestId::new(),
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                tool.into(),
                risk,
                PermissionInput::Ready(Arc::new(input)),
            ),
            Responder::Local,
        );
    }
    assert_eq!(state.decisions.count(DecisionKind::Permission), 2);
    for (tool, remaining) in [("Bash", 1), ("Write", 0)] {
        let item = decision_queue::current(&state, &LogFacts::default()).unwrap();
        assert!(matches!(&item.subject, DecisionSubject::Permission(p) if p.tool_name == tool));
        let layout = rustain::adapters::tui::layout::compute_layout(
            ratatui::layout::Rect::new(0, 0, 80, 24),
            &state.theme,
            "",
            1,
            false,
        )
        .unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                let shell = rustain::adapters::tui::presentation::Shell::new(frame.area(), &layout);
                painted = rustain::adapters::tui::presentation::compose(
                    frame,
                    &state,
                    &shell,
                    rustain::adapters::tui::keymap::Host::Local,
                    &LogFacts::default(),
                    &mut rustain::adapters::tui::presentation::occlusion::OpaqueRects::new(
                        shell.chat_pane,
                    ),
                    |_| {},
                );
            })
            .unwrap();
        assert!(painted);
        decision_queue::note_painted(&mut state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        assert!(matches!(
            decision_queue::handle_key(&mut state, PaneKey::Char('n'), &LogFacts::default()),
            PaneOutcome::Effect(_)
        ));
        assert_eq!(state.decisions.count(DecisionKind::Permission), remaining);
    }
    assert!(state.decisions.is_empty());
}
