//! S1-R3: discoverability is measured on the owning runtime's status pixels.
use super::support::*;
use rustain::adapters::tui::{keymap::Key, presentation};
use rustain::domain::models::{FocusState, InvocationStatus, Turn, WriteDiffState};
use serde_json::json;

fn target(width: u16, height: u16) -> Fixture {
    let (mut f, id) = Fixture::single(
        width,
        height,
        "Bash",
        json!({"command":"echo cue"}),
        InvocationStatus::Success,
        Some(("CUE-OUTPUT", WriteDiffState::NotAWrite)),
    );
    f.h.state.session_count = 1;
    f.h.state.theme.timing.status_hint_fade_sessions = 5;
    f.focus_call(&id);
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(id.as_str()));
    f
}

fn status(f: &mut Fixture) -> String {
    f.paint();
    let rect = f.layout().status_bar;
    row(f.buffer(), rect, rect.y)
}

fn assert_discoverable(width: u16, height: u16) {
    let mut f = target(width, height);
    let painted = status(&mut f);
    assert!(painted.contains("Ready"), "whole status fact: {painted}");
    assert!(
        painted.contains("[Enter] Inspect"),
        "render-focused tool must be discoverable at {width}: {painted}"
    );
    assert_eq!(painted.matches("[Enter] Inspect").count(), 1);
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn narrow_cue_target_keeps_ready_and_inspect_at_60() {
    assert_discoverable(60, 16);
}

#[test]
fn narrow_cue_target_keeps_ready_and_inspect_at_80() {
    assert_discoverable(80, 24);
}

#[test]
fn narrow_cue_absent_input_and_covered_stage_do_not_advertise_inspect() {
    for (width, height) in [(60, 16), (80, 24)] {
        let mut no_tool = Turn::new("fixture-model".into(), 0);
        prose(&mut no_tool, "No tool in this conversation.");
        let mut absent = Fixture::new(width, height, no_tool);
        absent.h.state.session_count = 1;
        absent.h.state.theme.timing.status_hint_fade_sessions = 5;
        let painted = status(&mut absent);
        assert!(absent.h.state.focused_item_id.is_none());
        assert!(absent.h.state.current_hint.is_some(), "hints are enabled");
        assert!(!painted.contains("Inspect"), "absent target: {painted}");

        let mut f = target(width, height);
        f.press(Key::Char('i'));
        assert_eq!(f.h.state.focus, FocusState::Input);
        let painted = status(&mut f);
        assert!(f.h.state.current_hint.is_some(), "Input hints are enabled");
        assert!(!painted.contains("Inspect"), "Input: {painted}");
        f.press(Key::Esc);
        assert_eq!(f.h.state.focus, FocusState::Chat);
        f.open();
        assert!(!f.h.state.modal_stack.is_empty());
        let painted = status(&mut f);
        assert!(f.h.state.current_hint.is_none());
        assert!(!painted.contains("Inspect"), "covered Stage: {painted}");
    }
}
