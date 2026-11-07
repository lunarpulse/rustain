//! K3: `Enter` on a focused block opens S-BLOCK through the production key, host
//! effect and compositor (D9); a tool still opens S-TOOL; a vanished target is honest.
use rustain::adapters::tui::{
    app::InputAction,
    block_viewer,
    keymap::Key,
    presentation::{self, Surface},
};
use rustain::domain::models::{FocusState, InvocationStatus, Turn, WriteDiffState};
use serde_json::json;

use super::support::{
    Fixture, TABLE, block, body_row, call, code, focus, mixed_turn, open, parts, rail_row,
    screen_row, single, status_row,
};

fn title(f: &Fixture) -> String {
    f.h.state.modal_stack.top().unwrap().title().to_owned()
}

#[test]
fn enter_on_a_focused_block_opens_the_block_viewer() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    open(&mut f, &ids[0]);
    f.compose();
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::BlockViewer(_)
    ));
    assert_eq!(title(&f), "Block · rust #1");
    assert!(screen_row(&f, f.frame_rect().y).contains("Block · rust #1"));
    let width = parts(&f).body.width;
    assert_eq!(
        status_row(&f),
        format!("code 1/2 · rust · 12 lines · Rendered · col 1-{width}/122")
    );
    assert!(
        body_row(&f, 0).starts_with("┌─ rust "),
        "{}",
        body_row(&f, 0)
    );
    assert!(body_row(&f, 1).starts_with("│fn line_01() {}"));
    assert!(rail_row(&f).starts_with("[Esc] Back"), "{}", rail_row(&f));
    // Opening selected the block for Chat copy, like a tool.
    assert_eq!(f.h.state.selected_item_id.as_deref(), Some(ids[0].as_str()));
    // The second block of the turn is the table, numbered 2 of 2.
    assert_eq!(f.press(Key::Esc), InputAction::Consumed);
    open(&mut f, &ids[2]);
    f.compose();
    assert_eq!(title(&f), "Block · table #2");
    assert_eq!(status_row(&f), "table 2/2 · 2 cols × 2 rows · Rendered");
}

#[test]
fn every_block_shape_opens_with_its_own_title_and_status() {
    let shrunk = "| c0 | c1 | c2 | c3 |\n|---|---|---|---|\n| aaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbb | cccccccccccccccccccc | dddddddddddddddddddd |\n";
    let collapsed = format!(
        "|{}\n|{}\n|{}\n",
        (0..20).map(|n| format!(" c{n} |")).collect::<String>(),
        "---|".repeat(20),
        (0..20).map(|n| format!(" {n} |")).collect::<String>(),
    );
    let nested_code = "> ```rust\n> let x = 1;\n> ```\n";
    let nested_list = "- item\n\n  ```\n  plain\n  ```\n";
    for (text, expected_title, expected_status) in [
        (
            shrunk,
            "Block · table #1",
            "table 1/1 · 4 cols × 1 row · Rendered · col 1-",
        ),
        (
            collapsed.as_str(),
            "Block · table #1",
            "table 1/1 · 20 cols × 1 row · Rendered · col 1-",
        ),
        (
            nested_code,
            "Block · rust #1",
            "code 1/1 · rust · 1 line · Rendered",
        ),
        (
            nested_list,
            "Block · code #1",
            "code 1/1 · 1 line · Rendered",
        ),
    ] {
        let (mut f, id) = single(60, 24, text);
        // The Stage item exists before it opens (shrunk or collapsed included).
        focus(&mut f, &id);
        open(&mut f, &id);
        f.compose();
        assert_eq!(title(&f), expected_title, "{text}");
        assert!(
            status_row(&f).starts_with(expected_status),
            "{text}: {}",
            status_row(&f)
        );
        assert!(!body_row(&f, 0).trim().is_empty());
    }
}

#[test]
fn enter_on_a_focused_tool_still_opens_the_tool_inspector_and_nothing_opens_without_focus() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &ids[1]);
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    let tab = f.tabs.active_tab();
    assert!(block_viewer::inspect_focused(
        &mut f.h.state,
        &tab.conversation,
        tab.reducer.open_turn.as_ref()
    ));
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::ToolInspector(_)
    ));
    assert!(f.h.state.block_viewers.is_empty());

    let mut turn = Turn::new("fixture-model".into(), 0);
    super::support::part(&mut turn, "No blocks here.");
    let mut f = Fixture::new(80, 24, turn);
    f.h.state.focus = FocusState::Chat;
    assert_eq!(f.press(Key::Enter), InputAction::Consumed);
    let tab = f.tabs.active_tab();
    assert!(!block_viewer::inspect_focused(
        &mut f.h.state,
        &tab.conversation,
        tab.reducer.open_turn.as_ref()
    ));
    assert!(f.h.state.modal_stack.is_empty());
}

#[test]
fn a_vanished_target_shows_the_notice_and_only_back_acts() {
    let (mut f, id) = single(80, 24, &code(3));
    open(&mut f, &id);
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert!(block(&f).missing);
    assert!(
        body_row(&f, 0).contains("This block is no longer in the conversation."),
        "{}",
        body_row(&f, 0)
    );
    for key in [
        Key::Char('j'),
        Key::Char('l'),
        Key::Char('c'),
        Key::Char('/'),
    ] {
        assert_eq!(f.press(key), InputAction::Consumed, "{key:?}");
    }
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::BlockViewer(_)
    ));
    assert_eq!(f.press(Key::Esc), InputAction::Consumed);
    assert!(f.h.state.modal_stack.is_empty() && f.h.state.block_viewers.is_empty());
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn the_frame_fits_the_smallest_terminal_with_a_full_composer() {
    let (mut f, id) = single(60, 16, &format!("{}\n{TABLE}", code(12)));
    f.h.state.input_buffer = (0..10)
        .map(|n| format!("draft {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    f.h.state.cursor_position = 3;
    f.paint();
    open(&mut f, &id);
    f.compose();
    let rect = f.frame_rect();
    let parts = parts(&f);
    assert!(parts.status.is_some() && parts.body.height >= 1 && parts.rail.is_some());
    assert!(rect.bottom() <= 16 && rect.right() <= 60);
    assert!(screen_row(&f, rect.y).contains("Block · rust #1"));
    assert!(status_row(&f).starts_with("code 1/2 · rust · 12 lines"));
    assert!(rail_row(&f).starts_with("[Esc] Back"));
    assert_eq!(
        f.h.state.input_buffer.lines().count(),
        10,
        "the draft is untouched"
    );
}

#[test]
fn a_tool_focus_control_keeps_the_block_fixture_honest() {
    // Positive control for the fixtures above: a tool exists in this turn.
    let mut turn = Turn::new("fixture-model".into(), 0);
    let tool = call(
        &mut turn,
        "Bash",
        json!({"command":"ls"}),
        InvocationStatus::Success,
        Some(("output", WriteDiffState::NotAWrite)),
    );
    let f = Fixture::new(80, 24, turn);
    assert!(f.h.state.focused_item_id.as_deref() == Some(tool.as_str()));
}

/// Offline desk-check export for `tests_tui/test_story_21_13_block_viewer.py`:
/// real paints of the scene's reply, only when `RUSTAIN_STORY_21_13_FRAME_DIR` is set.
#[test]
#[ignore = "offline desk-check export; set RUSTAIN_STORY_21_13_FRAME_DIR and run with --ignored"]
fn smoke_frames_export() {
    let Some(dir) = std::env::var_os("RUSTAIN_STORY_21_13_FRAME_DIR") else {
        return;
    };
    let scene: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests_tui/fixtures/scenes/block_viewer.json"
    ))
    .unwrap();
    let text = scene["personas"]["scene-ci-block-viewer"]["turns"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    let write = |name: &str, f: &Fixture| {
        let screen = super::support::screen_rows(f).join("\n");
        std::fs::write(
            std::path::Path::new(&dir).join(format!("{name}.json")),
            serde_json::json!({ "screen": screen }).to_string(),
        )
        .unwrap();
    };
    for (width, height) in [(80, 24), (60, 16), (130, 30)] {
        let (mut f, id) = single(width, height, text);
        focus(&mut f, &id);
        f.paint();
        write(&format!("{width}x{height}-marked"), &f);
        super::support::open_focused(&mut f);
        f.compose();
        write(&format!("{width}x{height}-opened"), &f);
        f.press(Key::Char('$'));
        f.compose();
        write(&format!("{width}x{height}-tail"), &f);
        f.press(Key::Char('0'));
        f.press(Key::Char('G'));
        f.compose();
        write(&format!("{width}x{height}-end"), &f);
        // Leg B: the capped Stage, then numbers, the table neighbour and its source.
        f.press(Key::Esc);
        f.paint();
        // The Stage's scroll offset counts up from the bottom: 0 shows the end.
        f.h.state.focused_item_id = None;
        f.tabs.active_tab_mut().view_state.scroll_offset = 0;
        f.h.state.set_scroll_offset(0);
        f.paint();
        write(&format!("{width}x{height}-stage-bottom"), &f);
        focus(&mut f, &id);
        super::support::open_focused(&mut f);
        f.press(Key::Char('#'));
        f.press(Key::Char('G'));
        f.compose();
        write(&format!("{width}x{height}-numbers-end"), &f);
        if let InputAction::StepBlock { forward } = f.press(Key::Char(']')) {
            let tab = f.tabs.active_tab();
            block_viewer::step(
                &mut f.h.state,
                &tab.conversation,
                tab.reducer.open_turn.as_ref(),
                forward,
            );
        }
        f.compose();
        write(&format!("{width}x{height}-next"), &f);
        f.press(Key::Char('v'));
        f.compose();
        write(&format!("{width}x{height}-source"), &f);
    }
}
