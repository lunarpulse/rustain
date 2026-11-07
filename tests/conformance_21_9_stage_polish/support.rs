//! Shared fixtures of the Story 21.9 keystones: the owning manager, the real
//! event-loop renderer and the production key decoder are the witnesses.
use std::cell::RefCell;

use rustain::adapters::tui::{
    app::InputAction, block_viewer, clipboard::ClipboardResult, handlers::clipboard, keymap::Key,
};
use rustain::domain::models::{PartId, Turn, TurnPart, turn::reasoning_item_id_for};

pub use crate::conformance_21_5_tool_inspector::support::call;
pub use crate::conformance_21_13_block_viewer::support::{
    Fixture, block, body_row, code, focus, open, open_focused, part, parts, rail_row, screen_rows,
    status_row, tab,
};

/// A reasoning part with the given facts; returns its Stage item id.
pub fn reasoning(
    turn: &mut Turn,
    text: &str,
    duration_ms: Option<u64>,
    tokens: Option<u32>,
) -> String {
    let part: PartId = turn.push_part(|id| TurnPart::Reasoning {
        id,
        text: text.into(),
        duration_ms,
        tokens,
    });
    reasoning_item_id_for(&turn.id, part)
}

/// `[reasoning(facts), prose]` with the item id of the reasoning part.
pub fn thinking_turn(text: &str, duration_ms: Option<u64>, tokens: Option<u32>) -> (Turn, String) {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let id = reasoning(&mut turn, text, duration_ms, tokens);
    part(&mut turn, "Answer.");
    (turn, id)
}

/// A fixture with composer focus: no Stage item carries the `▸` marker, so the
/// thinking row paints in its resting `◇` form.
pub fn unfocused(turn: Turn) -> Fixture {
    let mut f = Fixture::new(80, 24, turn);
    f.h.state.focus = rustain::domain::models::FocusState::Input;
    f.paint();
    f
}

/// Every painted screen row without its trailing blanks and without the
/// focused-turn arrow (`▶ `) the Stage puts before a turn's first row.
pub fn rows(f: &Fixture) -> Vec<String> {
    screen_rows(f)
        .into_iter()
        .map(|row| {
            let row = row.trim_end();
            row.strip_prefix("▶ ").unwrap_or(row).to_owned()
        })
        .collect()
}

/// The first row starting with `prefix`.
pub fn row_starting(rows: &[String], prefix: &str) -> Option<usize> {
    rows.iter().position(|row| row.starts_with(prefix))
}

/// Height of the single cached turn layout of the active tab.
pub fn cached_height(f: &Fixture) -> usize {
    let cache = &f.h.state.tab_render_states[&f.tabs.active_tab_id()].height_cache;
    assert_eq!(cache.entries.len(), 1, "one expanded turn is cached");
    cache.entries.values().next().unwrap().height
}

/// Press `key` and run the production copy effect with an injected copier.
pub fn copy(f: &mut Fixture, key: Key) -> Vec<String> {
    let action = f.press(key);
    let copied = RefCell::new(Vec::new());
    let copier = |text: &str| {
        copied.borrow_mut().push(text.to_owned());
        ClipboardResult::Osc52Success
    };
    let tab = f.tabs.active_tab();
    match action {
        InputAction::CopyBlockSource => {
            assert!(block_viewer::copy_source(
                &mut f.h.state,
                &tab.conversation,
                tab.reducer.open_turn.as_ref(),
                &copier,
            ));
        }
        InputAction::CopyToClipboard(content) => clipboard::requested_in(
            &mut f.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
            content,
            &copier,
        ),
        other => panic!("{key:?} must request a copy, got {other:?}"),
    }
    copied.into_inner()
}
