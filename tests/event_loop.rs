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

// ── Task 19.9: PermissionQueue push/pop (new pub/sub architecture) ──
//
// In the new ApprovalRuntime pub/sub architecture, PermissionQueue is a simple
// VecDeque of PendingPermission structs (no oneshot channels). Session-allow
// sweep is handled automatically by the ApprovalRuntime fast path.

#[test]
fn test_permission_queue_push_pop() {
    use rustain::adapters::tui::state::{PendingPermission, PermissionQueue};
    use rustain::domain::models::tool_call::{ApprovalSource, RequestId};
    use rustain::domain::models::ToolRisk;

    let mut queue = PermissionQueue::default();

    queue.push(PendingPermission {
        id: RequestId::new(),
        source: ApprovalSource::ForegroundTurn { conversation_id: "c1".into() },
        tool_name: "Bash".to_string(),
        tool_input: "ls".to_string(),
        risk: ToolRisk::Elevated,
    });
    queue.push(PendingPermission {
        id: RequestId::new(),
        source: ApprovalSource::ForegroundTurn { conversation_id: "c1".into() },
        tool_name: "Write".to_string(),
        tool_input: "a.rs".to_string(),
        risk: ToolRisk::Standard,
    });
    assert_eq!(queue.len(), 2);

    let first = queue.pop().unwrap();
    assert_eq!(first.tool_name, "Bash");
    assert_eq!(queue.len(), 1);

    let second = queue.pop().unwrap();
    assert_eq!(second.tool_name, "Write");
    assert!(queue.pop().is_none());
}
