//! The owning manager, decoder, runtime renderer and compositor are the witnesses.
use ratatui::{buffer::Buffer, layout::Rect};
use rustain::adapters::tui::{
    app::InputAction,
    block_viewer::{self, BlockViewerState},
    keymap::Key,
    presentation::{self, Surface},
};
use rustain::domain::models::{
    InvocationStatus, PartId, Turn, TurnPart, WriteDiffState, turn::block_item_id_for,
};
use serde_json::json;

pub use crate::conformance_21_5_tool_inspector::support::{Fixture, call};

/// Fenced rust code of `lines` numbered lines; line 7 (when present) is 120 cells.
pub fn code(lines: usize) -> String {
    let body = (1..=lines)
        .map(|n| match n {
            7 => format!("let w = \"{}TAIL120\";", "x".repeat(102)),
            n => format!("fn line_{n:02}() {{}}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("```rust\n{body}\n```\n")
}

pub const TABLE: &str = "| name | value |\n|---|---|\n| alpha | 1 |\n| beta | 2 |\n";

/// A prose part carrying `text` verbatim.
pub fn part(turn: &mut Turn, text: &str) -> PartId {
    turn.push_part(|id| TurnPart::Prose {
        id,
        text: text.into(),
    })
}

/// `[prose(code 12 lines), tool, prose(table)]`: three focusable Stage items.
pub fn mixed_turn() -> (Turn, [String; 3]) {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = part(&mut turn, &format!("Intro paragraph.\n\n{}", code(12)));
    let tool = call(
        &mut turn,
        "Bash",
        json!({"command":"ls"}),
        InvocationStatus::Success,
        Some(("output", WriteDiffState::NotAWrite)),
    );
    let second = part(&mut turn, &format!("Closing paragraph.\n\n{TABLE}"));
    let ids = [
        block_item_id_for(&turn.id, first, 0),
        tool,
        block_item_id_for(&turn.id, second, 0),
    ];
    (turn, ids)
}

/// Seat Stage focus by painting, then open it through the production key.
pub fn open(f: &mut Fixture, wanted: &str) {
    focus(f, wanted);
    open_focused(f);
}

/// Press the real `Enter` key and run the exact host effect for it; the frame
/// is then painted by the production pipeline.
pub fn open_focused(f: &mut Fixture) {
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    let tab = f.tabs.active_tab();
    assert!(block_viewer::inspect_focused(
        &mut f.h.state,
        &tab.conversation,
        tab.reducer.open_turn.as_ref(),
    ));
    f.paint();
    assert!(presentation::consistent(&f.h.state));
}

pub fn block(f: &Fixture) -> &BlockViewerState {
    let frame = f.h.state.modal_stack.top().expect("block frame");
    let Surface::BlockViewer(key) = frame.surface else {
        panic!("not a block frame: {:?}", frame.surface);
    };
    &f.h.state.block_viewers[&key.0]
}

pub fn parts(f: &Fixture) -> block_viewer::Parts {
    block_viewer::parts(f.frame_rect())
}

/// Painted text of one row: a wide glyph's continuation cell is not text (the
/// backend diff keeps stale content there).
fn text(buffer: &Buffer, rect: Rect, y: u16) -> String {
    let mut out = String::new();
    let mut x = rect.x;
    while x < rect.right() {
        let symbol = buffer[(x, y)].symbol();
        out.push_str(symbol);
        x += u16::try_from(unicode_width::UnicodeWidthStr::width(symbol).max(1)).unwrap();
    }
    out
}

pub fn screen_row(f: &Fixture, y: u16) -> String {
    let buffer: &Buffer = f.buffer();
    text(buffer, Rect::new(0, y, buffer.area.width, 1), y)
}

pub fn body_row(f: &Fixture, offset: u16) -> String {
    let body = parts(f).body;
    text(f.buffer(), body, body.y + offset)
}

pub fn status_row(f: &Fixture) -> String {
    let status = parts(f).status.expect("status row");
    text(f.buffer(), status, status.y).trim().to_owned()
}

pub fn rail_row(f: &Fixture) -> String {
    let rail = parts(f).rail.expect("rail row");
    text(f.buffer(), rail, rail.y).trim().to_owned()
}

/// Seat Stage focus by painting alone: at every scroll position the render
/// derives the nearest visible item (the carried focus is cleared per step, so
/// the keep-while-visible rule cannot pin an earlier item).
pub fn focus(f: &mut Fixture, wanted: &str) {
    f.paint();
    let max =
        f.h.state
            .total_content_height
            .saturating_sub(usize::from(f.h.state.viewport_height));
    for offset in (0..=max).rev() {
        f.tabs.active_tab_mut().view_state.scroll_offset = offset;
        f.h.state.set_scroll_offset(offset);
        f.h.state.focused_item_id = None;
        f.paint();
        if f.h.state.focused_item_id.as_deref() == Some(wanted) {
            return;
        }
    }
    // A block below another item in every viewport is reached by Tab (rule 1).
    for _ in 0..8 {
        if tab(f).is_none() || f.h.state.focused_item_id.as_deref() == Some(wanted) {
            break;
        }
    }
    assert_eq!(
        f.h.state.focused_item_id.as_deref(),
        Some(wanted),
        "production Stage never focused {wanted}"
    );
}

/// The real Tab key plus the host's cycle effect; `Some(id)` when it cycled.
pub fn tab(f: &mut Fixture) -> Option<String> {
    use rustain::adapters::tui::handlers::tool_focus::{CycleOutcome, cycle_item};
    assert_eq!(f.press(Key::Tab), InputAction::CycleInvocationInFocusedTurn);
    let active = f.tabs.active_tab_mut();
    let outcome = cycle_item(
        &mut f.h.state,
        &mut active.view_state,
        &active.conversation.turns,
        active.reducer.open_turn.as_ref(),
    );
    let CycleOutcome::Cycled { item_id } = outcome else {
        return None;
    };
    f.h.state.select_item_explicitly(item_id.clone());
    f.h.state.set_scroll_offset(active.view_state.scroll_offset);
    f.h.state.set_auto_scroll(matches!(
        active.view_state.mode,
        rustain::domain::models::AnchorMode::Following
    ));
    f.h.state.needs_redraw = true;
    f.paint();
    Some(item_id)
}

/// Every painted screen row (wide-glyph continuation cells skipped).
pub fn screen_rows(f: &Fixture) -> Vec<String> {
    (0..f.buffer().area.height)
        .map(|y| screen_row(f, y))
        .collect()
}

/// A single-turn fixture of `[prose]` only, at `width` x `height`.
pub fn single(width: u16, height: u16, text: &str) -> (Fixture, String) {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let id = part(&mut turn, text);
    let block = block_item_id_for(&turn.id, id, 0);
    (Fixture::new(width, height, turn), block)
}
