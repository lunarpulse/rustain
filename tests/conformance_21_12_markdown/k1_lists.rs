//! K1 — nested, loose, ordered and task lists through the real chat-pane front
//! door (Story 21.12, AC1; baseline defects B1-B3).
//!
//! Expected rows are written out from the CommonMark meaning of the source, not
//! produced by the layout code.

use super::support::*;
use ratatui::style::Modifier;
use rustain::adapters::tui::theme::Theme;
use rustain::infrastructure::terminal_info::ColorCapability;

const WIDTHS: [usize; 3] = [40, 80, 120];

fn rows_at(markdown: &str, inner: usize) -> Vec<String> {
    turn_rows(markdown, inner)
}

/// Paint `markdown` as a committed assistant turn under `theme`.
fn paint_themed(markdown: &str, theme: &Theme) -> Pane {
    paint_local(
        &turn_conversation(markdown),
        None,
        &Default::default(),
        None,
        theme,
        42,
        30,
    )
}

#[test]
fn k1_five_level_tight_list_paints_in_source_order_with_depth_indent() {
    let source = "- a\n  - b\n    - c\n      - d\n        - e\n- f";
    let expected = [
        "• a",
        "  • b",
        "    • c",
        "      • d",
        "        • e",
        "• f",
        "",
    ];
    for inner in WIDTHS {
        assert_eq!(rows_at(source, inner), expected, "inner width {inner}");
    }
}

#[test]
fn k1_parent_text_is_not_merged_into_the_child_item() {
    // Baseline B1: the parent `a` was glued to the child (`ab`) and the parent
    // item painted as an empty bullet.
    let rows = rows_at("- parent\n  - child", 40);
    assert_eq!(rows, ["• parent", "  • child", ""]);
    assert!(
        rows.iter().all(|r| !r.contains("parentchild")),
        "parent and child text merged: {rows:?}"
    );
}

#[test]
fn k1_loose_list_keeps_items_together_with_a_blank_line_between() {
    // Baseline B2: the paragraphs left the list and two empty bullets followed.
    let expected = ["• a", "", "• b", ""];
    for inner in WIDTHS {
        assert_eq!(
            rows_at("- a\n\n- b", inner),
            expected,
            "inner width {inner}"
        );
    }
}

#[test]
fn k1_ordered_list_numbers_from_its_start_value() {
    // Baseline B3: `3.` painted as `1.`.
    for inner in WIDTHS {
        assert_eq!(
            rows_at("3. x\n4. y\n5. z", inner),
            ["3. x", "4. y", "5. z", ""],
            "inner width {inner}"
        );
    }
    // Positive control: the default start is unchanged.
    assert_eq!(rows_at("1. one\n2. two", 40), ["1. one", "2. two", ""]);
}

#[test]
fn k1_task_items_show_their_check_state() {
    for inner in WIDTHS {
        assert_eq!(
            rows_at("- [ ] todo\n- [x] done", inner),
            ["• [ ] todo", "• [x] done", ""],
            "inner width {inner}"
        );
    }
}

#[test]
fn k1_ordered_item_children_align_to_the_text_column() {
    assert_eq!(
        rows_at("1. first\n   - inner\n2. second", 40),
        ["1. first", "   • inner", "2. second", ""]
    );
}

#[test]
fn k1_item_wraps_with_a_hanging_indent() {
    let rows = rows_at("- alpha beta gamma delta", 14);
    assert_eq!(rows, ["• alpha beta", "  gamma delta", ""]);
}

#[test]
fn k1_list_with_a_code_block_and_a_quote_stays_inside_its_item() {
    let source = "- item\n  ```rust\n  let x = 1;\n  ```\n  > quoted\n- next";
    for inner in [40usize, 80] {
        let pane = paint_local(
            &turn_conversation(source),
            None,
            &Default::default(),
            None,
            &rustain::adapters::tui::theme::Theme::dark(),
            u16::try_from(inner + GUTTER).unwrap(),
            60,
        );
        let rows = content_rows(&pane);
        assert_eq!(rows[0], "• item", "inner {inner}: {rows:?}");
        assert!(
            rows[1].starts_with("  ┌─ rust ") && rows[1].ends_with('┐'),
            "inner {inner}: header {:?}",
            rows[1]
        );
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(rows[1].as_str()),
            inner
        );
        assert!(rows[2].starts_with("  │let x = 1;") && rows[2].ends_with('│'));
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(rows[2].as_str()),
            inner
        );
        assert!(rows[3].starts_with("  └") && rows[3].ends_with('┘'));
        assert_eq!(rows[4], "  │ quoted", "inner {inner}: {rows:?}");
        assert_eq!(rows[5], "• next", "inner {inner}: {rows:?}");
        assert_eq!(rows[6], "");
        assert_eq!(rows.len(), 7, "{rows:?}");
    }
}

#[test]
fn k1_nested_block_quotes_paint_one_rail_per_level() {
    assert_eq!(
        rows_at("> outer\n>\n> > inner", 40),
        ["│ outer", "│", "│ │ inner", ""]
    );
}

#[test]
fn k1_quote_text_paints_italic_in_the_primary_foreground() {
    let theme = Theme::dark();
    let pane = paint_themed("> outer\n>\n> > inner **b**", &theme);
    for needle in ["outer", "inner"] {
        let style = style_of(&pane.buffer, needle).unwrap();
        assert!(style.add_modifier.contains(Modifier::ITALIC), "{needle}");
        assert_eq!(style.fg, Some(theme.colors.fg_primary), "{needle}");
    }
    let bold = style_of(&pane.buffer, "b").unwrap();
    assert!(
        bold.add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC)
    );
    // Positive control: prose outside a quote is not italic.
    let plain = paint_themed("outer", &theme);
    let style = style_of(&plain.buffer, "outer").unwrap();
    assert!(!style.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn k1_headings_rules_and_rhythm() {
    let source = "# Title\n\nbody\n\n## Sub\n\n---\n\nafter";
    let rows = rows_at(source, 20);
    assert_eq!(
        rows,
        [
            "Title",
            "────────────────────",
            "",
            "body",
            "",
            "Sub",
            "",
            "────────────────────",
            "",
            "after",
            "",
        ]
    );
}

#[test]
fn k1_heading_rule_is_accent_and_monochrome_h2_is_underlined() {
    let dark = Theme::dark();
    let pane = paint_themed("# Title\n\n## Sub\n\n### Third", &dark);
    let rule = style_of(&pane.buffer, "────").unwrap();
    assert_eq!(rule.fg, Some(dark.colors.accent), "H1 rule is accent");
    let h2 = style_of(&pane.buffer, "Sub").unwrap();
    assert_eq!(h2.fg, Some(dark.colors.accent));
    assert!(!h2.add_modifier.contains(Modifier::UNDERLINED));

    let mono = Theme::for_capability(ColorCapability::Monochrome);
    let pane = paint_themed("## Sub\n\n### Third", &mono);
    let h2 = style_of(&pane.buffer, "Sub").unwrap();
    let h3 = style_of(&pane.buffer, "Third").unwrap();
    assert!(h2.add_modifier.contains(Modifier::UNDERLINED), "{h2:?}");
    assert!(!h3.add_modifier.contains(Modifier::UNDERLINED), "{h3:?}");
}

#[test]
fn k1_soft_breaks_join_with_a_space_and_hard_breaks_split() {
    assert_eq!(rows_at("one\ntwo", 40), ["one two", ""]);
    assert_eq!(rows_at("one  \ntwo", 40), ["one", "two", ""]);
}

#[test]
fn k1_strikethrough_and_images_and_links() {
    let pane = paint_local(
        &turn_conversation("~~gone~~ ![alt text](http://x.test/i.png) [lbl](https://e.test/p)"),
        None,
        &Default::default(),
        None,
        &rustain::adapters::tui::theme::Theme::dark(),
        62,
        20,
    );
    let rows = content_rows(&pane);
    assert_eq!(
        rows[0], "gone [image: alt text] lbl (https://e.test/p)",
        "{rows:?}"
    );
    let style = style_of(&pane.buffer, "gone").unwrap();
    assert!(
        style
            .add_modifier
            .contains(ratatui::style::Modifier::CROSSED_OUT)
    );
}
