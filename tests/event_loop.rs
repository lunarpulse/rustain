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
