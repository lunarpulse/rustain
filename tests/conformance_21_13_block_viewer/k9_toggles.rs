//! K9 (AC10, AC11): the S-BLOCK toggles (`#`, `w`, `[`, `]`, `v`), their
//! applicability (a hint never promises a key that does nothing) and the rule
//! that copy is the source in every presentation.
use rustain::adapters::tui::{
    app::InputAction, block_viewer, keymap::Key, markdown::BlockMode, presentation,
};
use rustain::domain::models::{Turn, turn::block_item_id_for};

use super::k5_copy::copy;
use super::support::{
    Fixture, TABLE, block, body_row, code, mixed_turn, open, part, rail_row, screen_rows, single,
    status_row,
};

/// Press each key; a step request runs the production host effect.
fn press(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        match f.press(*key) {
            InputAction::StepBlock { forward } => {
                let tab = f.tabs.active_tab();
                block_viewer::step(
                    &mut f.h.state,
                    &tab.conversation,
                    tab.reducer.open_turn.as_ref(),
                    forward,
                );
            }
            InputAction::Consumed => {}
            other => panic!("{key:?} produced {other:?}"),
        }
    }
    f.paint();
}

fn title(f: &Fixture) -> String {
    let frame = f.h.state.modal_stack.top().unwrap();
    frame.title().to_owned()
}

#[test]
fn numbers_toggle_on_code_in_both_modes_and_never_reach_copy() {
    let (mut f, id) = single(80, 24, &code(12));
    open(&mut f, &id);
    assert!(!status_row(&f).contains("numbers"));
    assert!(rail_row(&f).contains("[#] Numbers"), "{}", rail_row(&f));
    let plain = body_row(&f, 1);
    press(&mut f, &[Key::Char('#')]);
    assert!(status_row(&f).contains("· numbers"), "{}", status_row(&f));
    assert!(
        body_row(&f, 0).starts_with("   │ ┌─ rust"),
        "{}",
        body_row(&f, 0)
    );
    assert!(
        body_row(&f, 1).starts_with(" 1 │ │fn line_01() {}"),
        "{}",
        body_row(&f, 1)
    );
    assert_ne!(body_row(&f, 1), plain, "observable change");
    // The viewport loses the gutter cells: 78 − 5.
    assert!(
        status_row(&f).contains("col 1-73/122"),
        "{}",
        status_row(&f)
    );
    // Source mode numbers the raw lines, no chrome rows.
    press(&mut f, &[Key::Char('v')]);
    assert!(
        body_row(&f, 0).starts_with(" 1 │ fn line_01() {}"),
        "{}",
        body_row(&f, 0)
    );
    assert!(
        body_row(&f, 11).starts_with("12 │ fn line_12() {}"),
        "{}",
        body_row(&f, 11)
    );
    // The gutter is chrome: copy is the source.
    let expected = (1..=12)
        .map(|n| match n {
            7 => format!("let w = \"{}TAIL120\";", "x".repeat(102)),
            n => format!("fn line_{n:02}() {{}}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(copy(&mut f, Key::Char('c')), [expected]);
    // Off again.
    press(&mut f, &[Key::Char('#')]);
    assert!(!status_row(&f).contains("numbers"));
    assert!(body_row(&f, 0).starts_with("fn line_01() {}"));
}

#[test]
fn a_rendered_table_has_no_numbers_but_its_source_does() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    open(&mut f, &ids[2]);
    let before = screen_rows(&f);
    assert!(!rail_row(&f).contains("Numbers"), "{}", rail_row(&f));
    press(&mut f, &[Key::Char('#')]);
    assert_eq!(screen_rows(&f), before, "inapplicable: no effect, no toast");
    assert!(!block(&f).numbers);
    press(&mut f, &[Key::Char('v')]);
    assert!(rail_row(&f).contains("[#] Numbers"), "{}", rail_row(&f));
    press(&mut f, &[Key::Char('#')]);
    assert!(
        body_row(&f, 0).starts_with("1 │ | name | value |"),
        "{}",
        body_row(&f, 0)
    );
    assert!(
        body_row(&f, 3).starts_with("4 │ | beta | 2 |"),
        "{}",
        body_row(&f, 3)
    );
}

#[test]
fn wrap_applies_to_code_only_and_replaces_horizontal_scrolling() {
    let (mut f, id) = single(80, 24, &code(12));
    open(&mut f, &id);
    assert!(status_row(&f).contains("col 1-78/122"));
    let unwrapped = screen_rows(&f).iter().any(|r| r.contains("TAIL120"));
    assert!(!unwrapped, "control: the clipped row hides its tail");
    press(&mut f, &[Key::Char('l'), Key::Char('l')]);
    assert_eq!(block(&f).viewer.h_off, 16);
    press(&mut f, &[Key::Char('w')]);
    assert_eq!(block(&f).viewer.h_off, 0, "wrap resets the offset");
    assert!(status_row(&f).contains("· wrap"), "{}", status_row(&f));
    assert!(!status_row(&f).contains("col "), "{}", status_row(&f));
    assert!(
        screen_rows(&f).iter().any(|r| r.contains("TAIL120")),
        "wrapped tail is visible"
    );
    assert!(!rail_row(&f).contains("Columns"), "{}", rail_row(&f));
    press(&mut f, &[Key::Char('l'), Key::Char('$')]);
    assert_eq!(
        block(&f).viewer.h_off,
        0,
        "no horizontal move while wrapped"
    );
    // Source mode wraps the raw lines too, and the source line numbers carry.
    press(&mut f, &[Key::Char('v'), Key::Char('#')]);
    let rows = screen_rows(&f);
    assert!(rows.iter().any(|r| r.contains("TAIL120")));
    assert!(
        rows.iter().any(|r| r.contains(" 8 │ fn line_08")),
        "{rows:#?}"
    );
    press(&mut f, &[Key::Char('w')]);
    assert!(!status_row(&f).contains("wrap"));
    // A table has no wrap: no effect and no hint.
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    open(&mut f, &ids[2]);
    let before = screen_rows(&f);
    assert!(!rail_row(&f).contains("Wrap"), "{}", rail_row(&f));
    press(&mut f, &[Key::Char('w')]);
    assert_eq!(screen_rows(&f), before);
    assert!(!block(&f).wrap);
}

#[test]
fn neighbours_stay_in_the_turn_keep_their_view_and_hide_at_the_ends() {
    // [code (20 lines, one 120 cells wide), table] in one turn.
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = part(&mut turn, &code(20));
    part(&mut turn, TABLE);
    let first_id = block_item_id_for(&turn.id, first, 0);
    let mut f = Fixture::new(80, 24, turn);
    open(&mut f, &first_id);
    assert_eq!(title(&f), "Block · rust #1");
    press(
        &mut f,
        &[
            Key::Char('v'),
            Key::Char('#'),
            Key::Char('j'),
            Key::Char('j'),
            Key::Char('$'),
        ],
    );
    assert!(
        block(&f).viewer.top > 0 && block(&f).viewer.h_off > 0,
        "control: the view moved"
    );
    let frames = f.h.state.modal_stack.len();
    press(&mut f, &[Key::Char(']')]);
    assert_eq!(title(&f), "Block · table #2");
    assert_eq!(f.h.state.modal_stack.len(), frames, "no push");
    let view = block(&f);
    assert_eq!(
        (view.mode, view.numbers),
        (BlockMode::Source, true),
        "mode and numbers kept"
    );
    assert_eq!(
        (view.viewer.top, view.viewer.h_off),
        (0, 0),
        "position reset"
    );
    assert!(status_row(&f).contains("table 2/2"), "{}", status_row(&f));
    // The last block of the turn: `]` does nothing.
    press(&mut f, &[Key::Char(']')]);
    assert_eq!(title(&f), "Block · table #2");
    // `[` from the table returns to the first block.
    press(&mut f, &[Key::Char('[')]);
    assert_eq!(title(&f), "Block · rust #1");
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn neighbours_never_cross_into_another_turn() {
    let mut first = Turn::new("fixture-model".into(), 0);
    let first_part = part(&mut first, &code(3));
    let first_id = block_item_id_for(&first.id, first_part, 0);
    let mut second = Turn::new("fixture-model".into(), 0);
    let second_part = part(&mut second, &code(3));
    let second_id = block_item_id_for(&second.id, second_part, 0);
    let mut f = Fixture::new(80, 24, first);
    f.tabs.active_tab_mut().conversation.turns.push(second);
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.load();
    f.paint();
    // Control: both turns' blocks exist in the conversation.
    let turns = &f.tabs.active_tab().conversation.turns;
    assert!(block_viewer::contains_block(turns, None, &first_id));
    assert!(block_viewer::contains_block(turns, None, &second_id));
    // Each turn's block opened through the production effect on its canonical id
    // (the Stage harness focuses by scrolling, which cannot isolate a later turn).
    for (id, turn_at) in [(&first_id, 0), (&second_id, 1)] {
        f.h.state.focused_item_id = Some(id.clone());
        let tab = f.tabs.active_tab();
        assert!(block_viewer::inspect_focused(
            &mut f.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
        ));
        f.paint();
        press(&mut f, &[Key::Char('['), Key::Char(']')]);
        assert_eq!(title(&f), "Block · rust #1", "no turn boundary is crossed");
        assert!(
            block(&f).target.turn_id == f.tabs.active_tab().conversation.turns[turn_at].id,
            "still turn {turn_at}'s block"
        );
        press(&mut f, &[Key::Esc]);
    }
}

#[test]
fn neighbour_hints_exist_only_for_the_neighbours_that_exist() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(130, 30, turn);
    open(&mut f, &ids[0]);
    assert!(rail_row(&f).contains("] Next"), "{}", rail_row(&f));
    assert!(!rail_row(&f).contains("Previous"), "{}", rail_row(&f));
    press(&mut f, &[Key::Char(']')]);
    assert!(rail_row(&f).contains("[[] Previous"), "{}", rail_row(&f));
    assert!(!rail_row(&f).contains("Next"), "{}", rail_row(&f));
    // A lone block has neither.
    let (mut f, id) = single(130, 30, &code(3));
    open(&mut f, &id);
    let rail = rail_row(&f);
    assert!(
        !rail.contains("Next") && !rail.contains("Previous"),
        "{rail}"
    );
}

#[test]
fn v_switches_the_presentation_and_copy_is_the_source_in_both() {
    let (mut f, id) = single(80, 24, "```rust\n\tlet a = 1;\u{1}b\n  c\n```\n");
    open(&mut f, &id);
    assert!(status_row(&f).contains("Rendered"));
    assert!(
        body_row(&f, 0).starts_with("┌─ rust"),
        "{}",
        body_row(&f, 0)
    );
    let rendered = copy(&mut f, Key::Char('c'));
    press(&mut f, &[Key::Char('v')]);
    assert_eq!(block(&f).mode, BlockMode::Source);
    assert!(status_row(&f).contains("Source"), "{}", status_row(&f));
    // Source display: the tab expands, the control character is not painted.
    assert!(
        body_row(&f, 0).starts_with("        let a = 1;b"),
        "{}",
        body_row(&f, 0)
    );
    assert!(body_row(&f, 1).starts_with("  c"));
    let source = copy(&mut f, Key::Char('c'));
    assert_eq!(rendered, source, "copy never depends on the presentation");
    assert_eq!(source, ["\tlet a = 1;\u{1}b\n  c"]);
    assert!(rail_row(&f).contains("[v] Rendered"), "{}", rail_row(&f));
    press(&mut f, &[Key::Char('v')]);
    assert!(body_row(&f, 0).starts_with("┌─ rust"));
    // A table: natural-width grid ↔ markdown source lines.
    let (mut f, id) = single(80, 24, TABLE);
    open(&mut f, &id);
    assert!(body_row(&f, 0).starts_with('┌'), "{}", body_row(&f, 0));
    press(&mut f, &[Key::Char('v')]);
    assert!(
        body_row(&f, 0).starts_with("| name | value |"),
        "{}",
        body_row(&f, 0)
    );
    assert!(
        body_row(&f, 1).starts_with("|---|---|"),
        "{}",
        body_row(&f, 1)
    );
}

#[test]
fn narrow_sheets_cut_whole_rail_items_and_mark_a_cut_status() {
    for (width, height) in [(60, 16), (80, 24)] {
        let (mut f, id) = single(width, height, &code(45));
        open(&mut f, &id);
        press(&mut f, &[Key::Char('#')]);
        let rail = rail_row(&f);
        // Every item is whole (`[key] Label`) and a cut is marked by `…` alone.
        for item in rail.split("  ") {
            let whole = item.starts_with('[')
                && item
                    .rsplit_once("] ")
                    .is_some_and(|(_, label)| !label.is_empty());
            assert!(
                whole || item == "…",
                "{width}x{height}: {item:?} in {rail:?}"
            );
        }
        assert!(rail.starts_with("[Esc] Back"), "{rail}");
        assert!(rail.ends_with('…'), "control: it really is cut: {rail}");
        // A status that does not fit says so instead of ending mid-token.
        let status = status_row(&f);
        assert!(
            status.ends_with('…') || status.ends_with("122"),
            "{width}x{height}: {status:?}"
        );
    }
}

/// Keys with no paint between them: the host paints once per tick.
fn batch(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        f.press(*key);
    }
    f.paint();
}

#[test]
fn a_horizontal_key_right_after_a_toggle_acts_on_the_toggled_rows() {
    // `w` off again, then `$`, in one batch: the unwrapped rows are wide.
    let (mut f, id) = single(80, 24, &code(12));
    open(&mut f, &id);
    press(&mut f, &[Key::Char('w')]);
    assert_eq!(block(&f).viewer.h_off, 0);
    batch(&mut f, &[Key::Char('w'), Key::Char('$')]);
    assert_eq!(
        block(&f).viewer.h_off,
        122 - 78,
        "`$` was dropped on stale rows"
    );
    // `v` back to Rendered, then `l`, in one batch: a 78-cell line fits as source
    // (80 cells boxed), so only the rendered rows scroll.
    let (mut f, id) = single(80, 24, &format!("```\n{}\n```\n", "x".repeat(78)));
    open(&mut f, &id);
    press(&mut f, &[Key::Char('v')]);
    assert!(!block(&f).wide(), "control: the source fits");
    batch(&mut f, &[Key::Char('v'), Key::Char('l')]);
    assert_eq!(block(&f).mode, BlockMode::Rendered);
    assert_eq!(block(&f).viewer.h_off, 2, "`l` was dropped on stale rows");
    // The reverse order: `l` (rendered rows scroll) and then `v` starts the source
    // at its first column.
    let (mut f, id) = single(80, 24, &format!("```\n{}\n```\n", "x".repeat(78)));
    open(&mut f, &id);
    batch(&mut f, &[Key::Char('l'), Key::Char('v')]);
    assert_eq!(
        (block(&f).mode, block(&f).viewer.h_off),
        (BlockMode::Source, 0)
    );
}

fn search(f: &mut Fixture, query: &str) {
    f.press(Key::Char('/'));
    for c in query.chars() {
        f.press(Key::Char(c));
    }
    f.press(Key::Enter);
    f.paint();
}

#[test]
fn stepping_keeps_wrap_and_clears_an_open_search() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = part(&mut turn, &code(12));
    part(&mut turn, &code(12));
    let first_id = block_item_id_for(&turn.id, first, 0);
    let mut f = Fixture::new(80, 24, turn);
    open(&mut f, &first_id);
    press(&mut f, &[Key::Char('w')]);
    search(&mut f, "line_05");
    assert!(
        !block(&f).search.matches.is_empty(),
        "control: a search is open"
    );
    press(&mut f, &[Key::Char(']')]);
    assert_eq!(title(&f), "Block · rust #2");
    assert!(block(&f).wrapped(), "wrap is part of the kept view (D14)");
    assert!(
        block(&f).search.matches.is_empty() && block(&f).search.query().is_empty(),
        "the search belongs to the block it was made in"
    );
}

#[test]
fn a_toggle_with_a_search_open_shows_the_focused_match_again() {
    let (mut f, id) = single(80, 24, &code(45));
    open(&mut f, &id);
    search(&mut f, "line_40");
    assert!(
        screen_rows(&f).iter().any(|row| row.contains("fn line_40")),
        "control: the match is shown"
    );
    press(&mut f, &[Key::Char('v')]);
    assert!(
        screen_rows(&f).iter().any(|row| row.contains("fn line_40")),
        "the focused match must stay in view after the rows changed"
    );
    press(&mut f, &[Key::Char('w')]);
    assert!(screen_rows(&f).iter().any(|row| row.contains("fn line_40")));
}
