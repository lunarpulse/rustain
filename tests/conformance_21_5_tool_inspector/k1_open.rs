//! K1 / M1: visible focus, not stale explicit selection, owns Enter.
use super::support::*;
use rustain::adapters::tui::{app::InputAction, hints, keymap::Key, presentation};
use rustain::domain::models::{FocusState, InvocationStatus, Turn, WriteDiffState};
use serde_json::json;

#[test]
fn enter_opens_the_render_focused_tool_not_a_stale_selection() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"echo first"}),
        InvocationStatus::Success,
        Some(("FIRST-OUTPUT", WriteDiffState::NotAWrite)),
    );
    prose(&mut turn, &rows(24));
    let second = call(
        &mut turn,
        "Edit",
        json!({"file_path":"notes.txt","old_string":"old","new_string":"new"}),
        InvocationStatus::Success,
        Some(("SECOND-OUTPUT", WriteDiffState::NotAWrite)),
    );
    prose(&mut turn, &rows(24));
    call(
        &mut turn,
        "Write",
        json!({"file_path":"fresh.txt","content":"fresh"}),
        InvocationStatus::Success,
        Some(("THIRD-OUTPUT", WriteDiffState::NewFile)),
    );
    let mut f = Fixture::new(80, 24, turn);
    f.focus_call(&second);
    f.h.state.selected_tool_id = Some(first.clone());
    assert_eq!(f.h.state.focused_tool_id.as_deref(), Some(second.as_str()));
    f.open();
    assert_eq!(f.h.state.modal_stack.len(), 1);
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().title(),
        "Tool · Edit #2"
    );
    let screen = f.h.screen_text();
    assert!(screen.contains("Tool · Edit #2"));
    assert!(screen.contains("[Output]"));
    assert!(screen.contains("SECOND-OUTPUT"));
    assert!(!screen.contains("FIRST-OUTPUT"));
    assert_eq!(f.h.state.selected_tool_id.as_deref(), Some(second.as_str()));
    let rect = f.frame_rect();
    assert_eq!(f.buffer()[(rect.x, rect.y)].symbol(), "┌");
    assert_eq!(
        f.buffer()[(rect.right() - 1, rect.bottom() - 1)].symbol(),
        "┘"
    );
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let payload = format!("{:?}", f.tool());
    assert!(matches!(
        f.press(Key::Enter),
        InputAction::Consumed | InputAction::Ignored
    ));
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top(),
        Some(&frame),
        "Enter inside the frame cannot reopen"
    );
    assert_eq!(format!("{:?}", f.tool()), payload);
    assert_eq!(f.h.state.tool_inspectors.len(), 1);
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn no_render_focused_tool_is_inert_but_input_enter_still_submits() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(&mut turn, "No tool invocation here.");
    let mut f = Fixture::new(80, 24, turn);
    assert!(f.h.state.focused_tool_id.is_none());
    let before = serde_json::to_vec(f.conversation()).unwrap();
    assert!(matches!(
        f.press(Key::Enter),
        InputAction::Consumed | InputAction::Ignored
    ));
    assert!(!f.inspect());
    assert!(f.h.state.modal_stack.is_empty());
    assert!(f.h.state.tool_inspectors.is_empty());
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), before);
    f.h.state.focus = FocusState::Input;
    f.h.state.input_buffer = "submit this draft".into();
    f.h.state.cursor_position = 6;
    assert!(
        matches!(f.press(Key::Enter), InputAction::SubmitMessage(_)),
        "Input Enter must remain submit"
    );
    assert!(f.h.state.tool_inspectors.is_empty());
}

#[test]
fn inspect_tip_is_true_only_for_a_chat_stage_target() {
    let (mut f, _) = Fixture::single(
        80,
        24,
        "Bash",
        json!({"command":"echo ok"}),
        InvocationStatus::Success,
        Some(("ok", WriteDiffState::NotAWrite)),
    );
    let hint = |f: &Fixture| {
        let tab = f.tabs.active_tab();
        let target = rustain::adapters::tui::tool_inspector::stage_target(
            &f.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
        );
        hints::contextual_hint(
            &f.h.state.focus,
            1,
            5,
            false,
            &f.h.state.modal_stack,
            f.h.state.pending_leader,
            target,
        )
        .unwrap_or_default()
    };
    assert!(
        f.h.state.focused_tool_id.is_some(),
        "production-rendered positive control"
    );
    assert!(hint(&f).contains("[Enter] Inspect"));
    let mut no_tool = Turn::new("fixture-model".into(), 0);
    prose(&mut no_tool, "No visible tool.");
    let absent = Fixture::new(80, 24, no_tool);
    assert!(absent.h.state.focused_tool_id.is_none());
    assert!(!hint(&absent).contains("[Enter] Inspect"));
    f.h.state.focus = FocusState::Input;
    assert!(!hint(&f).contains("[Enter] Inspect"));
    f.h.state.focus = FocusState::Chat;
    f.open();
    assert!(!hint(&f).contains("[Enter] Inspect"));
}
