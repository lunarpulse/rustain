//! K2: one Stage focus over tool calls and committed code/table blocks (D7, D8).
use std::collections::{BTreeMap, HashMap};

use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::{
    app::InputAction,
    handlers::tool_focus::{CycleOutcome, cycle_item},
    keymap::Key,
    state::TabRenderState,
    theme::Theme,
    widgets::chat_pane,
};
use rustain::domain::models::{
    AnchorMode, InvocationStatus, StreamingState, Turn, ViewState, WriteDiffState,
    turn::block_item_id_for,
};
use serde_json::json;

use super::support::{Fixture, call, code, focus, mixed_turn, part};

fn tab(f: &mut Fixture) -> CycleOutcome {
    assert_eq!(f.press(Key::Tab), InputAction::CycleInvocationInFocusedTurn);
    let active = f.tabs.active_tab_mut();
    let outcome = cycle_item(
        &mut f.h.state,
        &mut active.view_state,
        &active.conversation.turns,
        active.reducer.open_turn.as_ref(),
    );
    if let CycleOutcome::Cycled { item_id } = &outcome {
        // Exactly the host's select_item_explicitly + scroll snapshots.
        f.h.state.select_item_explicitly(item_id.clone());
        f.h.state.set_scroll_offset(active.view_state.scroll_offset);
        f.h.state
            .set_auto_scroll(matches!(active.view_state.mode, AnchorMode::Following));
        f.h.state.needs_redraw = true;
    }
    outcome
}

fn cycled(f: &mut Fixture, expected: &str) {
    match tab(f) {
        CycleOutcome::Cycled { item_id } => assert_eq!(item_id, expected),
        CycleOutcome::Fallthrough => panic!("Tab failed to reach {expected}"),
    }
    f.paint();
    assert_eq!(
        f.h.state.focused_item_id.as_deref(),
        Some(expected),
        "the next real render must not reseat the selection"
    );
}

fn marker_row(f: &Fixture) -> String {
    let screen = super::support::screen_rows(f);
    let rows: Vec<&String> = screen.iter().filter(|l| l.contains('▸')).collect();
    assert_eq!(
        rows.len(),
        1,
        "exactly one focused item paints: {screen:#?}"
    );
    rows[0].trim_end().to_owned()
}

#[test]
fn each_item_takes_focus_and_only_the_focused_item_paints_marker_and_cue() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &ids[0]);
    let code_row = marker_row(&f);
    assert!(code_row.starts_with("▸ ┌─ rust "), "{code_row}");
    assert!(code_row.ends_with(" [Enter] Inspect ─┐"), "{code_row}");
    focus(&mut f, &ids[1]);
    let tool_row = marker_row(&f);
    assert!(tool_row.contains("Bash"), "{tool_row}");
    focus(&mut f, &ids[2]);
    let table_row = marker_row(&f);
    // A natural-width table has no free dash run: the cue is omitted, Enter works.
    assert!(table_row.starts_with("▸ ┌"), "{table_row}");
    assert!(!table_row.contains("Inspect"), "{table_row}");
}

#[test]
fn tab_cycles_tools_and_blocks_in_render_order() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &ids[0]);
    cycled(&mut f, &ids[1]);
    cycled(&mut f, &ids[2]);
    cycled(&mut f, &ids[0]);
}

#[test]
fn a_lone_tool_falls_through_but_a_tool_with_a_block_cycles() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let tool = call(
        &mut turn,
        "Bash",
        json!({"command":"ls"}),
        InvocationStatus::Success,
        Some(("output", WriteDiffState::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &tool);
    assert!(matches!(tab(&mut f), CycleOutcome::Fallthrough));

    let mut turn = Turn::new("fixture-model".into(), 0);
    let block = part(&mut turn, &code(3));
    let block = block_item_id_for(&turn.id, block, 0);
    let tool = call(
        &mut turn,
        "Bash",
        json!({"command":"ls"}),
        InvocationStatus::Success,
        Some(("output", WriteDiffState::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &block);
    cycled(&mut f, &tool);
    cycled(&mut f, &block);
}

#[test]
fn tab_reaches_an_item_below_a_tall_block_and_keeps_it() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let tall = part(&mut turn, &code(60));
    let tall = block_item_id_for(&turn.id, tall, 0);
    let tool = call(
        &mut turn,
        "Bash",
        json!({"command":"ls"}),
        InvocationStatus::Success,
        Some(("output", WriteDiffState::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &tall);
    cycled(&mut f, &tool);
    assert!(f.h.screen_text().contains("Bash"));
    cycled(&mut f, &tall);
}

#[test]
fn the_attached_host_never_focuses_blocks() {
    let (turn, _) = mixed_turn();
    let f = Fixture::new(80, 24, turn);
    let conversation = f.conversation().clone();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut render_state = TabRenderState::default();
    let mut focused = None;
    terminal
        .draw(|frame| {
            focused = chat_pane::render_attached(
                frame,
                frame.area(),
                &conversation,
                &StreamingState::default(),
                &ViewState::default(),
                &Theme::dark(),
                &mut render_state,
                &BTreeMap::new(),
                &HashMap::new(),
            )
            .focused_item_id;
        })
        .unwrap();
    assert!(
        render_state
            .item_boundaries
            .iter()
            .all(|(_, id)| id.starts_with("tc_"))
    );
    assert!(focused.is_none_or(|id| id.starts_with("tc_")));
    // Positive control: the local host owns the same blocks.
    let local =
        f.h.state
            .tab_render_states
            .get(&f.tabs.active_tab_id())
            .unwrap();
    assert!(
        local
            .item_boundaries
            .iter()
            .any(|(_, id)| id.starts_with("md_"))
    );
}

fn block_ids(f: &Fixture) -> Vec<String> {
    f.h.state
        .tab_render_states
        .get(&f.tabs.active_tab_id())
        .map(|render| {
            render
                .item_boundaries
                .iter()
                .filter(|(_, id)| id.starts_with("md_"))
                .map(|(_, id)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn only_committed_assistant_prose_takes_block_focus() {
    let fence = "```rust\nlet hidden = 1;\n```\n";
    let mut turn = Turn::new("fixture-model".into(), 0);
    // Reasoning code, and a plain committed prose part with no block at all.
    turn.push_part(|id| rustain::domain::models::TurnPart::Reasoning {
        id,
        text: fence.into(),
        duration_ms: None,
        tokens: None,
    });
    part(&mut turn, "Plain answer, no block.");
    let mut f = Fixture::new(80, 24, turn);
    // Story 21.9: the Stage folds reasoning by default, which would paint no
    // reasoning code at all; the pin is about the code it paints when shown.
    f.h.state.reasoning_display = rustain::domain::models::ReasoningDisplay::Expanded;
    // The open streaming prose, a user message and a system message all carry code.
    f.tabs.active_tab_mut().reducer.open_prose = Some(fence.into());
    for (id, role) in [
        ("u1", rustain::domain::models::MessageRole::User),
        ("s1", rustain::domain::models::MessageRole::System),
    ] {
        f.tabs
            .active_tab_mut()
            .conversation
            .messages
            .push(rustain::domain::models::ChatMessage {
                id: id.into(),
                role,
                content: fence.into(),
                ..Default::default()
            });
    }
    f.paint();
    assert!(block_ids(&f).is_empty(), "{:?}", block_ids(&f));
    assert!(
        f.h.screen_text().contains("let hidden"),
        "control: the code is painted"
    );
    // Positive control: the same fence in a committed assistant prose part focuses.
    let mut turn = Turn::new("fixture-model".into(), 0);
    part(&mut turn, fence);
    let f = Fixture::new(80, 24, turn);
    assert_eq!(block_ids(&f).len(), 1);
}
