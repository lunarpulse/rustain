//! K9 / AC10: marker pixels are a post-pass, never a new Stage layout.
use super::support::*;
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use rustain::adapters::tui::{
    app::InputAction,
    keymap::{self, CommandId, Context, Key, table_chat::ChatCmd},
    state::TabRenderState,
    widgets::chat_pane,
};
use rustain::domain::models::{FocusState, InvocationStatus as S, Turn, WriteDiffState as D};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

pub(super) fn cue() -> String {
    let binding = keymap::table()
        .iter()
        .find(|b| b.context == Context::Chat && b.command == CommandId::Chat(ChatCmd::Inspect))
        .expect("the real Inspect binding");
    let text = format!(
        "{} {}",
        keymap::render::rail(Context::Chat, ChatCmd::Inspect),
        binding.label
    );
    assert_eq!(text, "[Enter] Inspect");
    text
}

fn header(buffer: &Buffer, area: Rect, needle: &str) -> (u16, u16) {
    let y = (area.y..area.bottom())
        .find(|&y| row(buffer, area, y).contains(needle))
        .unwrap_or_else(|| panic!("no visible header for {needle:?}"));
    let x = (area.x..area.right())
        .find(|&x| matches!(buffer[(x, y)].symbol(), "┄" | "┃" | "▸"))
        .expect("tool's two-cell lead, after the turn gutter");
    (x, y)
}

fn stage_text(buffer: &Buffer, area: Rect) -> String {
    (area.y..area.bottom())
        .map(|y| row(buffer, area, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn fixture(width: u16, height: u16, full: bool, status: S, command: &str) -> Fixture {
    let output = if status == S::Error {
        "ERROR-CONTROL"
    } else {
        "界 e\u{301} 🙂\r\noutput-control"
    };
    let (mut f, id) = Fixture::single(
        width,
        height,
        "Bash",
        json!({"command": command}),
        status,
        Some((output, D::NotAWrite)),
    );
    if full {
        f.h.state.input_buffer = ["draft 界 e\u{301}"; 8].join("\n");
        f.h.state.cursor_position = 5;
    }
    f.h.state.session_count = 1;
    f.h.state.theme.timing.status_hint_fade_sessions = 5;
    f.focus_call(&id);
    // A second real paint settles the current owning ViewState before snapshots.
    f.paint();
    assert_eq!(f.h.state.focused_tool_id.as_deref(), Some(id.as_str()));
    assert_eq!(f.buffer().area, Rect::new(0, 0, width, height));
    assert_eq!(f.layout().input_area.height, if full { 10 } else { 3 });
    f
}

/// Compare every cell, allowing exactly the replaced lead and the appended cue.
/// The expected content comes from a same-width production Input-focus paint.
fn assert_post_pass(f: &Fixture, plain: &Buffer, needle: &str, fits: bool) {
    let area = f.layout().chat_pane;
    let (x, y) = header(plain, area, needle);
    let actual = f.buffer();
    assert_eq!(actual[(x, y)].symbol(), "▸", "positive marker pixel");
    assert_eq!(actual[(x + 1, y)].symbol(), " ");
    for lead_x in [x, x + 1] {
        let cell = &actual[(lead_x, y)];
        assert_eq!(cell.fg, f.h.state.theme.colors.accent);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }
    let hint = cue();
    let last = (area.x..area.right())
        .rev()
        .find(|&x| !plain[(x, y)].symbol().trim().is_empty())
        .expect("nonempty header");
    // The blank separator and the whole hint must fit after the original row.
    assert_eq!(last + 2 + hint.len() as u16 <= area.right(), fits);
    let mut restored = actual.clone();
    restored[(x, y)] = plain[(x, y)].clone();
    restored[(x + 1, y)] = plain[(x + 1, y)].clone();
    if fits {
        assert!(row(actual, area, y).trim_end().ends_with(&hint));
        assert_eq!(actual[(last + 1, y)].symbol(), " ");
        for (offset, ch) in hint.chars().enumerate() {
            let hint_x = last + 2 + offset as u16;
            assert_eq!(actual[(hint_x, y)].symbol(), ch.to_string());
            assert!(actual[(hint_x, y)].modifier.contains(Modifier::DIM));
            restored[(hint_x, y)] = plain[(hint_x, y)].clone();
        }
        restored[(last + 1, y)] = plain[(last + 1, y)].clone();
    } else {
        let painted = row(actual, area, y);
        assert!(!painted.contains("Inspect") && !painted.contains("[Enter]"));
    }
    assert_eq!(
        pixels(&restored, area),
        pixels(plain, area),
        "all other cells, rows, gutter and header text must remain identical"
    );
    assert_eq!(stage_text(actual, area).matches('▸').count(), 1);
}

#[test]
fn focused_header_replaces_both_leads_without_rows_cache_or_view_churn() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            for status in [S::Success, S::Error] {
                let mut f = fixture(width, height, full, status.clone(), "ok");
                f.press(Key::Char('i'));
                assert_eq!(f.h.state.focus, FocusState::Input);
                f.paint();
                let plain = f.buffer().clone();
                let area = f.layout().chat_pane;
                let (x, y) = header(&plain, area, "Bash");
                assert_eq!(
                    plain[(x, y)].symbol(),
                    if status == S::Error { "┃" } else { "┄" }
                );
                assert!(!stage_text(&plain, area).contains('▸'));
                assert!(!stage_text(&plain, area).contains(&cue()));
                let cache = f.cache();
                assert!(!cache.0.is_empty(), "real CachedTurnLayout entries exist");
                let view = f.view();
                let geometry = (
                    f.h.state.total_content_height,
                    f.h.state.viewport_height,
                    f.h.state.block_boundaries.clone(),
                    f.h.state.message_boundaries.clone(),
                );
                let draft = (
                    f.h.state.input_buffer.clone(),
                    f.h.state.cursor_position,
                    f.h.state.input_scroll_offset,
                );
                let transcript = serde_json::to_vec(f.conversation()).unwrap();
                f.press(Key::Esc);
                assert_eq!(f.h.state.focus, FocusState::Chat);
                f.paint();
                assert!(f.h.state.modal_stack.is_empty());
                assert_post_pass(&f, &plain, "Bash", true);
                let marked = pixels(f.buffer(), area);
                let selection = (
                    f.h.state.focused_tool_id.clone(),
                    f.h.state.selected_tool_id.clone(),
                );
                for _ in 0..3 {
                    f.paint();
                    assert_eq!(pixels(f.buffer(), area), marked);
                    assert_eq!(
                        f.cache(),
                        cache,
                        "including every height key and cached offset"
                    );
                    assert_eq!(
                        f.view(),
                        view,
                        "no scroll, anchor, collapse or mode movement"
                    );
                    assert_eq!(
                        (
                            f.h.state.total_content_height,
                            f.h.state.viewport_height,
                            f.h.state.block_boundaries.clone(),
                            f.h.state.message_boundaries.clone()
                        ),
                        geometry
                    );
                    assert_eq!(
                        (
                            f.h.state.focused_tool_id.clone(),
                            f.h.state.selected_tool_id.clone()
                        ),
                        selection
                    );
                    assert_eq!(
                        (
                            f.h.state.input_buffer.clone(),
                            f.h.state.cursor_position,
                            f.h.state.input_scroll_offset
                        ),
                        draft
                    );
                    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
                }
                // Negative Input guard paired with the exact same fixture's positive paint.
                f.press(Key::Char('i'));
                f.paint();
                assert_eq!(pixels(f.buffer(), area), pixels(&plain, area));
                assert_eq!(f.cache(), cache);
            }
        }
    }
}

#[test]
fn long_unicode_summary_omits_the_entire_inline_cue_only_when_it_cannot_fit() {
    let command = format!("printf 界e\u{301}🙂 {}", "界e\u{301}🙂".repeat(24));
    for (width, height, fits) in [(60, 16, false), (130, 30, true)] {
        let mut f = fixture(width, height, false, S::Success, &command);
        f.press(Key::Char('i'));
        f.paint();
        let plain = f.buffer().clone();
        let area = f.layout().chat_pane;
        assert!(stage_text(&plain, area).contains("printf 界"));
        let cache = f.cache();
        f.press(Key::Esc);
        f.paint();
        assert_post_pass(&f, &plain, "Bash", fits);
        assert_eq!(f.cache(), cache);
        assert!(
            f.h.state.current_hint.as_deref().unwrap().contains(&cue()),
            "the tip keeps discovery even when the inline cue cannot fit"
        );
    }
}

#[test]
fn inline_cue_fits_at_the_exact_last_cell_but_never_partially_overflows() {
    let mut probe = fixture(80, 24, false, S::Success, "x");
    probe.press(Key::Char('i'));
    probe.paint();
    let area = probe.layout().chat_pane;
    let (_, y) = header(probe.buffer(), area, "Bash");
    let last = (area.x..area.right())
        .rev()
        .find(|&x| !probe.buffer()[(x, y)].symbol().trim().is_empty())
        .unwrap();
    let extra = area.right() - (last + 2 + cue().len() as u16);
    for (additional, fits) in [(0, true), (1, false)] {
        // ASCII grows by exactly one cell; neither case reaches Bash's 60-cell
        // summary cap. The production paint below proves the fit precondition.
        let command = "x".repeat(usize::from(extra) + 1 + additional);
        assert!(command.len() < 60);
        let mut f = fixture(80, 24, false, S::Success, &command);
        f.press(Key::Char('i'));
        f.paint();
        let plain = f.buffer().clone();
        f.press(Key::Esc);
        f.paint();
        assert_post_pass(&f, &plain, "Bash", fits);
        if fits {
            assert_eq!(f.buffer()[(area.right() - 1, y)].symbol(), "t");
        }
    }
}

#[test]
fn only_the_render_focused_invocation_receives_marker_or_inline_cue() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"first"}),
        S::Success,
        Some(("first\r\n界", D::NotAWrite)),
    );
    call(
        &mut turn,
        "Read",
        json!({"file_path":"src/界-e\u{301}-🙂.rs"}),
        S::Success,
        Some(("second\r\noutput", D::NotAWrite)),
    );
    call(
        &mut turn,
        "mcp__工具__long_unicode_source_name",
        json!({}),
        S::Success,
        Some(("third\r\noutput", D::NotAWrite)),
    );
    let mut f = Fixture::new(130, 30, turn);
    f.paint();
    assert_eq!(f.h.state.focused_tool_id.as_deref(), Some(first.as_str()));
    let area = f.layout().chat_pane;
    f.press(Key::Char('i'));
    f.paint();
    let plain = f.buffer().clone();
    let other_rows = [
        header(&plain, area, "Read").1,
        header(&plain, area, "long_unicode_source_name").1,
    ];
    f.press(Key::Esc);
    f.paint();
    assert_post_pass(&f, &plain, "Bash", true);
    for y in other_rows {
        assert_eq!(
            pixels(f.buffer(), Rect::new(area.x, y, area.width, 1)),
            pixels(&plain, Rect::new(area.x, y, area.width, 1))
        );
        assert!(!row(f.buffer(), area, y).contains(&cue()));
    }
}

#[test]
fn covered_stage_removes_marker_and_hint_even_on_exposed_underlay_rows() {
    // A wide centered frame leaves the top Stage header and both rails exposed.
    // Full-sheet negative pixels alone would only prove opaque occlusion.
    for full in [false, true] {
        let mut f = fixture(130, 30, full, S::Success, "echo covered-control");
        f.press(Key::Char('i'));
        f.paint();
        let plain = f.buffer().clone();
        let area = f.layout().chat_pane;
        let (_, y) = header(&plain, area, "Bash");
        f.press(Key::Esc);
        f.paint();
        assert_post_pass(&f, &plain, "Bash", true);
        let cache = f.cache();
        let view = f.view();
        f.open();
        assert!(!f.h.state.modal_stack.is_empty());
        assert_eq!(f.h.state.focus, FocusState::Chat, "base focus is retained");
        let frame = f.frame_rect();
        assert!(
            y < frame.y,
            "the entire header must actually remain exposed"
        );
        let exposed = Rect::new(area.x, y, area.width, 1);
        let mut expected = pixels(&plain, exposed);
        for cell in &mut expected {
            cell.modifier.insert(Modifier::DIM);
        }
        assert_eq!(
            pixels(f.buffer(), exposed),
            expected,
            "covered Stage must revert the header, not merely hide it behind a sheet"
        );
        assert!(!row(f.buffer(), area, y).contains(&cue()));
        assert_eq!(f.cache(), cache);
        assert_eq!(f.view(), view);
        assert_eq!(f.press(Key::Esc), InputAction::Consumed);
        f.paint();
        assert_post_pass(&f, &plain, "Bash", true);
    }
}

#[test]
fn actual_attached_renderer_keeps_the_same_unmarked_header() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut f = fixture(width, height, false, S::Success, "ok");
        f.press(Key::Char('i'));
        f.paint();
        let plain = f.buffer().clone();
        let area = f.layout().chat_pane;
        let (_, local_y) = header(&plain, area, "Bash");
        let conversation = f.conversation().clone();
        let streaming = f.tabs.active_tab().streaming.clone();
        let view = f.tabs.active_tab().view_state.clone();
        let theme = f.h.state.theme.clone();
        let mut cache = TabRenderState::default();
        // The attached host intentionally adds terminal provenance. Capture its
        // own unmarked baseline rather than equating local/attached geometry.
        let mut paint_attached = |f: &mut Fixture| {
            let mut total = 0;
            f.h.terminal
                .draw(|frame| {
                    total = chat_pane::render_attached(
                        frame,
                        area,
                        &conversation,
                        &streaming,
                        &view,
                        &theme,
                        &mut cache,
                        &BTreeMap::new(),
                        &HashMap::new(),
                    )
                    .total_content_height;
                })
                .unwrap();
            total
        };
        let baseline_height = paint_attached(&mut f);
        let attached = f.buffer().clone();
        let (attached_x, attached_y) = header(&attached, area, "Bash");
        let attached_text = stage_text(&attached, area);
        assert!(
            attached_text.contains("[terminal]"),
            "real attached provenance"
        );
        assert!(!stage_text(&plain, area).contains("[terminal]"));
        assert_eq!(baseline_height, f.h.state.total_content_height + 1);
        assert_ne!(attached_y, local_y, "origin prefix adds an observable row");
        assert_eq!(attached[(attached_x, attached_y)].symbol(), "┄");
        assert_eq!(row(&attached, area, attached_y), row(&plain, area, local_y));
        assert_eq!(
            pixels(&attached, Rect::new(area.x, attached_y, area.width, 1)),
            pixels(&plain, Rect::new(area.x, local_y, area.width, 1)),
        );
        assert!(!attached_text.contains('▸') && !attached_text.contains(&cue()));

        // Same conversation, same width: the local runtime must visibly mark
        // the target before the attached negative can count as a witness.
        f.press(Key::Esc);
        f.paint();
        assert_post_pass(&f, &plain, "Bash", true);
        assert_eq!(paint_attached(&mut f), baseline_height);
        assert_eq!(pixels(f.buffer(), area), pixels(&attached, area));
        assert_eq!(header(f.buffer(), area, "Bash"), (attached_x, attached_y));
        assert!(!stage_text(f.buffer(), area).contains('▸'));
        assert!(!stage_text(f.buffer(), area).contains(&cue()));
    }
}
