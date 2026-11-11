//! K3 — toast geometry, displacement on screen and Stage invariance (AC3, A5).
//! Both hosts paint through the one `toast_layer::render`; the attached draw
//! and the local `render` call sites are covered in-lib (`story21_1_` tests).

use ratatui::layout::Rect;
use rustain::adapters::tui::notifications::{self, NoticeDraft, NoticeSeverity, NoticeSource};
use unicode_width::UnicodeWidthStr;

use super::support::{clocked, differing_cells, draw, layout_of, notify, row_text, screen};

const SIZES: [(u16, u16); 3] = [(60, 16), (80, 24), (130, 30)];

#[test]
fn k3_a_toast_sits_on_the_chat_pane_bottom_row_directly_above_the_mission_strip() {
    for (w, h) in SIZES {
        let (mut state, _c) = clocked(w, h);
        let layout = layout_of(&state, w, h);
        // The Stage ends where the mission strip begins (21-7 A22); the strip is directly above
        // the status row at all three supported sizes.
        let strip = layout.mission_strip.expect("an empty composer leaves room");
        assert_eq!(layout.chat_pane.bottom(), strip.y, "{w}x{h}");
        assert_eq!(strip.bottom(), layout.status_bar.y, "{w}x{h}");
        notify(&mut state, NoticeSeverity::Success, "Bookmark added");
        let buffer = draw(&mut state, w, h, true);
        let pane = layout.chat_pane;
        if w < 100 {
            // Narrow: ONE borderless row, width = painted text cells, right-aligned.
            let y = pane.bottom() - 1;
            let text = row_text(&buffer, y);
            let toast = "Success Bookmark added";
            assert!(text.ends_with(toast), "{w}x{h}: {text:?}");
            assert_eq!(text.width(), usize::from(pane.width));
            let painted = text.trim_start_matches(|c: char| c != 'S');
            assert_eq!(painted.width(), toast.width(), "no padding beyond the text");
        } else {
            // Wide: bordered box, at most half the pane, newest at the bottom.
            let rows: Vec<String> = (pane.top()..pane.bottom())
                .map(|y| row_text(&buffer, y))
                .collect();
            let text_row = rows
                .iter()
                .position(|r| r.contains("Success Bookmark added"))
                .expect("wide toast text row");
            assert_eq!(
                text_row,
                rows.len() - 2,
                "text row sits above the bottom border"
            );
            assert!(rows[rows.len() - 1].contains('┘') || rows[rows.len() - 1].contains('╯'));
            let box_width = rows[text_row]
                .find('│')
                .map(|at| rows[text_row][at..].width())
                .expect("left border");
            assert!(box_width <= usize::from(pane.width / 2), "{box_width}");
        }
    }
}

#[test]
fn k3_toasts_change_no_cell_outside_their_own_rows_and_never_touch_status_or_composer() {
    for (w, h) in SIZES {
        let (mut state, _c) = clocked(w, h);
        let layout = layout_of(&state, w, h);
        let baseline = draw(&mut state, w, h, false);
        notify(
            &mut state,
            NoticeSeverity::Error,
            "first line of an error\nsecond line hidden",
        );
        notify(&mut state, NoticeSeverity::Warning, "a warning");
        let after = draw(&mut state, w, h, true);
        let pane = layout.chat_pane;
        let rows_used: u16 = if w < 100 { 2 } else { 6 };
        let toast_zone = Rect::new(pane.x, pane.bottom() - rows_used, pane.width, rows_used);
        let changed = differing_cells(&baseline, &after);
        assert!(!changed.is_empty(), "{w}x{h}: the toasts painted something");
        for (x, y) in changed {
            assert!(
                toast_zone.contains((x, y).into()),
                "{w}x{h}: cell ({x},{y}) outside the toast rows changed"
            );
        }
        // Layout is a pure function of the terminal, never of the store.
        let again = layout_of(&state, w, h);
        assert_eq!(
            (again.chat_pane, again.status_bar, again.input_area),
            (layout.chat_pane, layout.status_bar, layout.input_area)
        );
        let text = screen(&after);
        assert!(text.contains("status-row-sentinel") && text.contains("composer-sentinel"));
    }
}

#[test]
fn k3_multiline_messages_paint_the_first_line_and_an_ellipsis() {
    let (mut state, _c) = clocked(80, 24);
    notify(
        &mut state,
        NoticeSeverity::Info,
        "Logged out of API key #1.\nNo credentials remain.",
    );
    let screen = screen(&draw(&mut state, 80, 24, true));
    assert!(
        screen.contains("Info Logged out of API key #1.…"),
        "{screen}"
    );
    assert!(
        !screen.contains("No credentials remain"),
        "second line stays in the log"
    );
}

#[test]
fn k3_long_messages_and_wide_glyphs_are_clipped_to_the_pane_on_whole_graphemes() {
    for (w, h) in SIZES {
        let (mut state, _c) = clocked(w, h);
        let layout = layout_of(&state, w, h);
        let long = format!(
            "{}界👨\u{200d}👩\u{200d}👧{}",
            "x".repeat(usize::from(w)),
            "y".repeat(50)
        );
        notify(&mut state, NoticeSeverity::Warning, &long);
        let buffer = draw(&mut state, w, h, true);
        for y in layout.chat_pane.top()..layout.chat_pane.bottom() {
            let text = row_text(&buffer, y);
            assert!(
                text.width() <= usize::from(layout.chat_pane.width),
                "{w}x{h} row {y}"
            );
            assert!(!text.ends_with('\u{200d}'), "no dangling ZWJ at row {y}");
        }
        assert!(screen(&buffer).contains('…'), "{w}x{h}: the clip is marked");
    }
}

/// Literal `(width, height, cap)` table; the expected caps are not computed
/// from the formula under test. The chat pane is the terminal height minus the
/// mission strip row, the status row and the composer (5 rows here): 60x16 ->
/// 11 rows (cap 2, the strip row cost one toast), 80x24 -> 19, 130x30 -> 25 —
/// the larger sizes hold the full three. The lower steps of the formula (pane
/// heights 0..11) are pinned by the in-file `capacity_for` test, because layout
/// is only defined from the 60x16 floor up.
#[test]
fn k3_the_visible_cap_follows_the_pane_height() {
    for (w, h, cap) in [(60u16, 16u16, 2usize), (80, 24, 3), (130, 30, 3)] {
        let (mut state, _c) = clocked(w, h);
        for i in 0..6 {
            notify(
                &mut state,
                NoticeSeverity::Error,
                &format!("error number {i}"),
            );
        }
        assert_eq!(state.notifications.visible().count(), cap, "{w}x{h}");
        let painted = screen(&draw(&mut state, w, h, true))
            .lines()
            .filter(|l| l.contains("error number"))
            .count();
        assert_eq!(painted, cap, "{w}x{h}: one painted row per visible toast");
    }
}

#[test]
fn k3_background_conversation_notices_name_their_conversation() {
    let (mut state, _c) = clocked(80, 24);
    notifications::notify_draft(
        &mut state,
        NoticeDraft::new(
            NoticeSource::System,
            NoticeSeverity::Warning,
            "provider failed",
        )
        .conversation("conv-bg-1234"),
    );
    let buffer = super::support::draw_attributed(
        &mut state,
        80,
        24,
        Some("conv-active"),
        &[("conv-bg-1234".into(), "Research".into())],
    );
    assert!(
        screen(&buffer).contains("Warning [Research] provider failed"),
        "{}",
        screen(&buffer)
    );
    // The active conversation's own notice carries no attribution.
    notifications::notify_draft(
        &mut state,
        NoticeDraft::new(NoticeSource::System, NoticeSeverity::Info, "mine")
            .conversation("conv-active"),
    );
    let buffer = super::support::draw_attributed(&mut state, 80, 24, Some("conv-active"), &[]);
    assert!(screen(&buffer).contains("Info mine"), "{}", screen(&buffer));
}
