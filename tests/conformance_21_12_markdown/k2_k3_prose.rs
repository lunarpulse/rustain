//! K2 — literal prose (no marker or HTML stripping) and K3 — whitespace
//! preservation, wrapping without loss and tab-aware code blocks, all read
//! back from painted cells (Story 21.12, AC2/AC5; baseline B5, B6, B8, B9).

use super::support::*;

use unicode_width::UnicodeWidthStr;

#[test]
fn k2_completed_prose_keeps_every_commonmark_visible_character() {
    // DF-081 / B5 / B6: `*`, backticks, `~~` and tag-shaped text were deleted.
    let source =
        "SELECT * FROM t\n\n2 \\* 3\n\n**not closed\n\nVec<String>\n\n<div>x</div>\n\na ~~ b";
    let expected = [
        "SELECT * FROM t",
        "",
        "2 * 3",
        "",
        "**not closed",
        "",
        "Vec<String>",
        "",
        "<div>x</div>",
        "",
        "a ~~ b",
        "",
    ];
    for inner in [40usize, 80, 120] {
        assert_eq!(turn_rows(source, inner), expected, "inner width {inner}");
    }
}

#[test]
fn k2_inline_html_stays_in_prose_and_in_emphasis() {
    assert_eq!(
        turn_rows("use `Vec<T>` and Option<T> or <b>bold</b> *a<b>c*", 60),
        ["use Vec<T> and Option<T> or <b>bold</b> a<b>c", ""]
    );
}

#[test]
fn k2_html_block_keeps_its_lines_and_whitespace() {
    let source = "<div>\n  indented <b>x</b>\n</div>";
    assert_eq!(
        turn_rows(source, 40),
        ["<div>", "  indented <b>x</b>", "</div>", ""]
    );
}

#[test]
fn k2_literal_prose_paints_the_same_rows_on_both_hosts() {
    // The attached host adds its origin prefix row above the turn; the
    // markdown rows below it are the same on both hosts.
    let source = "SELECT * FROM t\n\n**not closed\n\nVec<String>";
    let conversation = turn_conversation(source);
    let theme = rustain::adapters::tui::theme::Theme::dark();
    let local = content_rows(&paint_local(
        &conversation,
        None,
        &Default::default(),
        None,
        &theme,
        42,
        40,
    ));
    let attached = content_rows(&paint_attached(
        &conversation,
        &Default::default(),
        &theme,
        42,
        40,
    ));
    assert_eq!(
        local,
        ["SELECT * FROM t", "", "**not closed", "", "Vec<String>", ""]
    );
    assert_eq!(attached.len(), local.len() + 1, "{attached:?}");
    assert_eq!(attached[1..], local[..], "{attached:?}");
}

#[test]
fn k3_prose_and_code_span_whitespace_is_preserved() {
    assert_eq!(turn_rows("x  y", 40), ["x  y", ""]);
    assert_eq!(turn_rows("use `a  b` here", 40), ["use a  b here", ""]);
}

#[test]
fn k3_code_span_moves_whole_to_the_next_line_when_it_fits_there() {
    // `b  c` is 4 cells; "aaaa" + " " + 4 = 9 > 8, so the span wraps whole.
    assert_eq!(turn_rows("aaaa `b  c`", 8), ["aaaa", "b  c", ""]);
}

#[test]
fn k3_overlong_code_span_hard_breaks_at_grapheme_boundaries() {
    // DF-007: a span wider than the line breaks, it never clips.
    let rows = turn_rows("`abcdefghij`", 4);
    assert_eq!(rows, ["abcd", "efgh", "ij", ""]);
}

#[test]
fn k3_overlong_token_wraps_without_losing_a_character() {
    let token = "a".repeat(200);
    for inner in [40usize, 80, 120] {
        let rows = turn_rows(&token, inner);
        let body: Vec<&String> = rows.iter().filter(|r| !r.is_empty()).collect();
        assert_eq!(body[0].width(), inner, "first row unchanged at {inner}");
        assert!(body.iter().all(|r| r.width() <= inner));
        let joined: String = body.iter().map(|r| r.as_str()).collect();
        assert_eq!(joined, token, "inner width {inner}");
    }
}

#[test]
fn k3_soft_wrap_consumes_the_whitespace_run_at_the_break() {
    // The run of three spaces between the words is dropped at the break and
    // never leads the next row.
    assert_eq!(turn_rows("aaaa   bbbb", 5), ["aaaa", "bbbb", ""]);
}

#[test]
fn k3_tab_indented_code_block_keeps_indentation_and_a_straight_border() {
    let source = "```go\nfunc f() {\n\tx := 1\n\t\ty := 2\n}\n```";
    let pane = paint_local(
        &turn_conversation(source),
        None,
        &Default::default(),
        None,
        &rustain::adapters::tui::theme::Theme::dark(),
        32,
        20,
    );
    let rows = content_rows(&pane);
    // rows[0] is the header; rows 1..=4 the code lines; rows[5] the footer.
    assert!(rows[2].starts_with("│        x := 1"), "{:?}", rows[2]);
    let deeper = format!("│{}y := 2", " ".repeat(16));
    assert!(rows[3].starts_with(&deeper), "{:?}", rows[3]);
    let inner = 30usize;
    for (i, row) in rows.iter().enumerate().take(6) {
        assert_eq!(
            row.width(),
            inner,
            "row {i} {row:?} must keep a straight border"
        );
        assert!(!row.contains('\t'));
    }
    // No painted control character anywhere.
    for y in 0..20u16 {
        assert!(!row_text(&pane.buffer, y).contains('\t'));
    }
}
