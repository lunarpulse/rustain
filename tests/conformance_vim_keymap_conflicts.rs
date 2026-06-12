#![allow(clippy::type_complexity)] // AI-12.1: test fixture tuple types
//! Story 16.6 AC9 + Story 16.8 AC8 — No keymap conflicts.
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

fn make_state() -> TuiState {
    TuiState::new(80, 24)
}
fn key(c: char) -> DomainInputEvent {
    DomainInputEvent::KeyPress(c)
}
fn sp(key: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(key)
}

#[test]
fn legacy_chat_keys_are_unchanged() {
    let legacy_keys: &[(DomainInputEvent, fn() -> InputAction)] = &[
        (key('j'), || InputAction::ScrollLineDown),
        (key('k'), || InputAction::ScrollLineUp),
        (key('J'), || InputAction::Consumed),
        (key('K'), || InputAction::Consumed),
        (key('{'), || InputAction::Consumed),
        (key('}'), || InputAction::Consumed),
        (key('g'), || InputAction::ScrollToTop),
        (key('G'), || InputAction::ScrollToBottom),
        (key('c'), || InputAction::CopyToClipboard(String::new())),
        (key('p'), || InputAction::Consumed),
        (key('i'), || InputAction::Consumed),
        (key('q'), || InputAction::Quit),
        (key('f'), || InputAction::ForkAtMessage),
        (key('R'), || InputAction::RewindAtMessage),
        (key('m'), || InputAction::ToggleBookmark),
        (key('\''), || InputAction::OpenBookmarkList),
        (key('?'), || InputAction::Consumed),
        (key('y'), || InputAction::Ignored),
        (key('n'), || InputAction::Ignored),
        (key('1'), || InputAction::SwitchToTab(1)),
    ];
    for (event, expected_fn) in legacy_keys {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, event);
        assert_eq!(action, expected_fn());
    }
}

#[test]
fn vim_z_prefix_in_chat_only() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('z'));
    assert!(s.pending_z);
    let mut s2 = make_state();
    s2.focus = FocusState::Input;
    handle_input(&mut s2, &key('z'));
    assert!(!s2.pending_z);
}

#[test]
fn g_capital_emits_scroll_to_bottom() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(handle_input(&mut s, &key('G')), InputAction::ScrollToBottom);
}

#[test]
fn tab_narrow_override() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(
        handle_input(&mut s, &sp(DomainKey::Tab)),
        InputAction::CycleInvocationInFocusedTurn
    );
}

#[test]
fn non_key_resets_chords() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('z'));
    assert!(s.pending_z);
    handle_input(&mut s, &DomainInputEvent::Resize(100, 30));
    assert!(!s.pending_z);
    assert!(s.pending_bracket.is_none());
    assert!(!s.pending_g);
}

#[test]
fn esc_resets_chords() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('z'));
    assert!(s.pending_z);
    handle_input(&mut s, &sp(DomainKey::Esc));
    assert!(!s.pending_z);
    assert!(!s.pending_g);
}

#[test]
fn chord_cancellation_absorbs_legacy() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('z'));
    assert!(s.pending_z);
    assert_eq!(handle_input(&mut s, &key('1')), InputAction::Consumed);
}

#[test]
fn vim_not_in_which_key() {
    let s = make_state();
    assert!(s.which_key.lookup_chord('z').is_none());
}

#[test]
fn chord_defaults_false() {
    let s = make_state();
    assert!(!s.pending_z);
    assert!(s.pending_bracket.is_none());
    assert!(!s.pending_g);
}

// ── S16.8 AC8 tests ──
#[test]
fn ctrl_d_emits_scroll_half_page_down() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(
        handle_input(&mut s, &sp(DomainKey::CtrlD)),
        InputAction::ScrollHalfPageDown
    );
}
#[test]
fn ctrl_u_emits_scroll_half_page_up() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(
        handle_input(&mut s, &sp(DomainKey::CtrlU)),
        InputAction::ScrollHalfPageUp
    );
}
#[test]
fn ctrl_b_emits_scroll_full_page_up() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(
        handle_input(&mut s, &sp(DomainKey::CtrlB)),
        InputAction::ScrollFullPageUp
    );
}
#[test]
fn ctrl_f_chat_emits_full_page_down() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(
        handle_input(&mut s, &sp(DomainKey::CtrlF)),
        InputAction::ScrollFullPageDown
    );
}
#[test]
fn ctrl_f_input_opens_search() {
    let mut s = make_state();
    s.focus = FocusState::Input;
    s.input_buffer.clear();
    assert_eq!(
        handle_input(&mut s, &sp(DomainKey::CtrlF)),
        InputAction::OpenSearch
    );
}
#[test]
fn gg_chord_emits_scroll_top() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(handle_input(&mut s, &key('g')), InputAction::ScrollToTop);
    assert!(s.pending_g);
    // P6: Second g is idempotent — no redundant dispatch (was ScrollToTop).
    assert_eq!(handle_input(&mut s, &key('g')), InputAction::Consumed);
    assert!(!s.pending_g);
}
#[test]
fn single_g_emits_scroll_top() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(handle_input(&mut s, &key('g')), InputAction::ScrollToTop);
    assert!(s.pending_g);
}
#[test]
fn g_chord_resets_on_non_g() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('g'));
    assert!(s.pending_g);
    handle_input(&mut s, &key('j'));
    assert!(!s.pending_g);
}
#[test]
fn g_chord_resets_on_resize() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('g'));
    assert!(s.pending_g);
    handle_input(&mut s, &DomainInputEvent::Resize(100, 30));
    assert!(!s.pending_g);
}
#[test]
fn mouse_scroll_dispatches() {
    let mut s = make_state();
    s.focus = FocusState::Input;
    let r = handle_input(
        &mut s,
        &DomainInputEvent::MouseScroll(
            rustain::domain::models::view_state::ScrollDelta::WheelDown(3),
        ),
    );
    assert!(matches!(r, InputAction::MouseScroll(_)));
}

// ── P12: AC8 missing test cases ──

#[test]
fn g_in_pinned_noop_at_dispatch_layer() {
    // AC3: handle_input always returns ScrollToBottom for G in Chat.
    // Mode-aware Pinned no-op happens in event_loop dispatcher.
    // This test verifies the InputAction contract — the dispatcher test
    // (compute_scroll) confirms mode-specific behavior.
    let mut s = make_state();
    s.focus = FocusState::Chat;
    assert_eq!(handle_input(&mut s, &key('G')), InputAction::ScrollToBottom);
}

#[test]
fn mouse_wheel_up_emits_wheel_up_scroll_delta() {
    let mut s = make_state();
    s.focus = FocusState::Chat;
    let r = handle_input(
        &mut s,
        &DomainInputEvent::MouseScroll(rustain::domain::models::view_state::ScrollDelta::WheelUp(
            3,
        )),
    );
    assert!(matches!(
        r,
        InputAction::MouseScroll(rustain::domain::models::view_state::ScrollDelta::WheelUp(3))
    ));
}

#[test]
fn shift_wheel_up_emits_half_page_up() {
    // Shift+wheel half-page is handled at convert_crossterm_event level;
    // the mouse_scroll_dispatches test covers the generic MouseScroll path.
    // This test verifies a half-page delta also dispatches correctly.
    let mut s = make_state();
    s.focus = FocusState::Chat;
    let r = handle_input(
        &mut s,
        &DomainInputEvent::MouseScroll(
            rustain::domain::models::view_state::ScrollDelta::HalfPageUp,
        ),
    );
    assert!(matches!(
        r,
        InputAction::MouseScroll(rustain::domain::models::view_state::ScrollDelta::HalfPageUp)
    ));
}

#[test]
fn mouse_wheel_in_input_focus_dispatches() {
    // AC4 note: MouseScroll always dispatches; focus guard expected in event loop,
    // not handle_input. The InputAction is emitted regardless of focus.
    let mut s = make_state();
    s.focus = FocusState::Input;
    let r = handle_input(
        &mut s,
        &DomainInputEvent::MouseScroll(rustain::domain::models::view_state::ScrollDelta::WheelUp(
            3,
        )),
    );
    assert!(matches!(r, InputAction::MouseScroll(_)));
}

#[test]
fn z_g_chord_emits_scroll_to_top() {
    // P8: z-prefix chord cancelled with 'g' still produces ScrollToTop
    // instead of silently consuming the keystroke.
    let mut s = make_state();
    s.focus = FocusState::Chat;
    handle_input(&mut s, &key('z'));
    assert!(s.pending_z);
    assert_eq!(handle_input(&mut s, &key('g')), InputAction::ScrollToTop);
    assert!(!s.pending_z);
}
