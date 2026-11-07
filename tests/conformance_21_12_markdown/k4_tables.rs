//! K4 — tables through the chat pane (Story 21.12, AC3, D10; baseline B4).
//!
//! Natural, shrunk and collapsed tables, alignment, CJK cells and inline
//! formatting, observed as painted cells. The expected rows are written out by
//! hand; the collapse line must paint no `[key]`.

use super::support::*;

use ratatui::style::Modifier;
use rustain::adapters::tui::markdown::{self, RenderOptions};
use rustain::adapters::tui::theme::Theme;

use crate::render_helpers::make_turn;

const NATURAL: &str = "| Name | Qty |\n|:-----|----:|\n| apple | 3 |\n| kiwi | 12 |";

#[test]
fn k4_natural_table_paints_box_chrome_alignment_and_padding() {
    assert_eq!(
        turn_rows(NATURAL, 40),
        [
            "┌───────┬─────┐",
            "│ Name  │ Qty │",
            "├───────┼─────┤",
            "│ apple │   3 │",
            "│ kiwi  │  12 │",
            "└───────┴─────┘",
            "",
        ]
    );
    // Centre: the left pad is the smaller half.
    assert_eq!(
        trimmed(turn_rows("| H |\n|:-:|\n| abcd |", 40)),
        ["┌──────┐", "│  H   │", "├──────┤", "│ abcd │", "└──────┘"]
    );
}

#[test]
fn k4_wide_cells_are_measured_in_cells_not_characters() {
    assert_eq!(
        trimmed(turn_rows("| 名前 | n |\n|---|---|\n| 界界界 | 1 |", 40)),
        [
            "┌────────┬───┐",
            "│ 名前   │ n │",
            "├────────┼───┤",
            "│ 界界界 │ 1 │",
            "└────────┴───┘"
        ]
    );
}

#[test]
fn k4_shrunk_table_fills_the_width_exactly_and_ends_cut_cells_in_an_ellipsis() {
    let source = "| Command | Description |\n|---|---|\n| cargo test | Runs the full test suite for the whole workspace |";
    assert_eq!(
        trimmed(turn_rows(source, 40)),
        [
            "┌────────┬─────────────────────────────┐",
            "│ Comma… │ Description                 │",
            "├────────┼─────────────────────────────┤",
            "│ cargo… │ Runs the full test suite f… │",
            "└────────┴─────────────────────────────┘",
        ]
    );
}

#[test]
fn k4_every_table_row_fits_the_width_at_the_stage_widths() {
    let source = format!(
        "{NATURAL}\n\n| `a  b` | **xyz** | 界界 |\n|---|:-:|--:|\n| ✓ | long cell text that has to shrink | e\u{301}e\u{301} |"
    );
    // The pipeline's own lines, not the clipped pane: an overflowing border
    // would otherwise be cut off by the buffer edge and go unnoticed.
    for inner in [25usize, 30, 40, 80, 120] {
        let rendered =
            markdown::render(&source, inner, &Theme::dark(), &RenderOptions::completed());
        for line in &rendered.lines {
            let row: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= inner,
                "{row:?} wider than {inner}"
            );
        }
    }
}

#[test]
fn k4_a_table_too_narrow_for_its_columns_collapses_without_a_key() {
    let source = "| aaa | bbb | ccc | ddd |\n|---|---|---|---|\n| 111 | 222 | 333 | 444 |\n| 555 | 666 | 777 | 888 |";
    let collapsed = turn_rows(source, 24);
    assert_eq!(collapsed, ["table · 4 cols × 2 rows", ""]);
    assert!(
        collapsed
            .iter()
            .all(|r| !r.contains('[') && !r.contains(']')),
        "the collapse line must not advertise a key: {collapsed:?}"
    );
    // One cell more and the same table paints as a box.
    assert_eq!(turn_rows(source, 25)[0], "┌─────┬─────┬─────┬─────┐");
}

#[test]
fn k4_a_body_row_with_more_cells_than_the_header_keeps_every_cell() {
    assert_eq!(
        trimmed(turn_rows("| a | b |\n|---|---|\n| 1 | 2 | 3 |", 40)),
        [
            "┌───┬───┬───┐",
            "│ a │ b │   │",
            "├───┼───┼───┤",
            "│ 1 │ 2 │ 3 │",
            "└───┴───┴───┘"
        ]
    );
}

#[test]
fn k4_edge_shapes_paint_empty_short_and_contained_tables_exactly() {
    // Header only: the box closes right under the header rule.
    assert_eq!(
        trimmed(turn_rows("| Name | Qty |\n|---|---|", 40)),
        [
            "┌──────┬─────┐",
            "│ Name │ Qty │",
            "├──────┼─────┤",
            "└──────┴─────┘"
        ]
    );
    // An empty cell (`|a||c|`) and a short row paint blank cells in the grid.
    assert_eq!(
        trimmed(turn_rows(
            "| h1 | h2 | h3 |\n|---|---|---|\n|a||c|\n| x |",
            40
        )),
        [
            "┌────┬────┬────┐",
            "│ h1 │ h2 │ h3 │",
            "├────┼────┼────┤",
            "│ a  │    │ c  │",
            "│ x  │    │    │",
            "└────┴────┴────┘"
        ]
    );
    // Containers shrink the table's width by their prefix and keep it inside it.
    let grid = [
        "┌───┬───┐",
        "│ a │ b │",
        "├───┼───┤",
        "│ 1 │ 2 │",
        "└───┴───┘",
    ];
    let quoted = trimmed(turn_rows("> | a | b |\n> |---|---|\n> | 1 | 2 |", 40));
    assert_eq!(
        quoted,
        grid.iter().map(|r| format!("│ {r}")).collect::<Vec<_>>()
    );
    let listed = trimmed(turn_rows("- | a | b |\n  |---|---|\n  | 1 | 2 |", 40));
    assert_eq!(
        listed,
        grid.iter()
            .enumerate()
            .map(|(i, r)| format!("{}{r}", if i == 0 { "• " } else { "  " }))
            .collect::<Vec<_>>()
    );
    // The collapse boundary moves by the prefix: 25 cells of table fit, 24 do not.
    let wide = "| aaa | bbb | ccc | ddd |\n|---|---|---|---|\n| 111 | 222 | 333 | 444 |";
    let quote = |inner: usize| {
        let source: String = wide.lines().map(|l| format!("> {l}\n")).collect();
        trimmed(turn_rows(&source, inner))
    };
    assert_eq!(quote(27)[0], "│ ┌─────┬─────┬─────┬─────┐");
    assert_eq!(quote(26), ["│ table · 4 cols × 1 row"]);
    // Header cells are bold; body cells are not.
    let pane = paint_local(
        &turn_conversation(NATURAL),
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        62,
        30,
    );
    assert!(is_bold(&pane.buffer, "Name"));
    assert!(is_bold(&pane.buffer, "Qty"));
    assert!(!is_bold(&pane.buffer, "apple"));
}

#[test]
fn k4_inline_formatting_survives_inside_cells() {
    let pane = paint_local(
        &turn_conversation("| `a  b` | **xyz** | ~~gone~~ |\n|---|---|---|\n| 1 | 2 | 3 |"),
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        62,
        30,
    );
    let text = rows(&pane.buffer);
    assert!(
        text.iter().any(|r| r.contains("│ a  b │ xyz │")),
        "{text:?}"
    );
    assert!(is_bold(&pane.buffer, "xyz"));
    assert!(
        style_of(&pane.buffer, "gone")
            .is_some_and(|s| s.add_modifier.contains(Modifier::CROSSED_OUT))
    );
}

#[test]
fn k4_a_streaming_row_closes_formatting_inside_its_last_cell_only() {
    let open = make_turn("open", vec![], None);
    let pane = paint_local(
        &crate::render_helpers::make_conversation(vec![], vec![]),
        Some(&open),
        &Default::default(),
        Some("| a | b |\n|---|---|\n| *x | **bold te"),
        &Theme::dark(),
        62,
        30,
    );
    let text = rows(&pane.buffer);
    assert!(
        text.iter().any(|r| r.contains("│ *x │ bold te │")),
        "{text:?}"
    );
    assert!(is_bold(&pane.buffer, "bold te"));
    assert!(
        !text.iter().any(|r| r.contains("**")),
        "no closer may leak into the painted table: {text:?}"
    );
}
