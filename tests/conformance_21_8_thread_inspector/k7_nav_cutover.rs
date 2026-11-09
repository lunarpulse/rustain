//! K7 (local half, AC3/AC5/AC9): the body keys clamp against the painted rows, `p`/`l`
//! page through the real page loader keeping the root and the opened item, no omitted
//! page means no painted or live paging key, a peer arrival never moves the viewer, and
//! the layout cache is built once per (width, content).
//!
//! The K6 cutover gate (no `RecipientSurface` / `recipient_surface` residue anywhere) is
//! owned by `tests/**/k6_gate.rs`; it is deliberately not duplicated here.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;

use super::support::*;

fn consumed(action: InputAction) -> bool {
    matches!(action, InputAction::Consumed | InputAction::Ignored)
}

#[tokio::test]
async fn k7_keys_clamp_and_the_clamped_row_count_is_the_painted_row_count() {
    for width in [60u16, 80, 100, 130] {
        let mut rig = Rig::plain(long_workspace().await, width, 24).await;
        rig.view(NEWEST).await;
        rig.paint();
        let content = rig.inspector().content.clone();

        // The rows the viewer clamps against are exactly the rows paged through the paint.
        let painted = painted_body(&mut rig);
        let (rows, viewport) = rig.fit();
        assert_eq!(
            painted.len(),
            rows,
            "width {width}: clamped rows == painted rows"
        );
        assert_eq!(
            compact(&painted.join("\n")),
            compact(&content),
            "width {width}: the painted rows are the whole page, nothing more"
        );
        let max = rows - viewport;
        assert!(
            max > viewport,
            "width {width}: the thread spans several pages"
        );

        let at = |rig: &mut Rig, key: Key, expected: usize| {
            assert!(consumed(rig.key(key)), "width {width}: {key:?}");
            rig.paint();
            assert_eq!(rig.viewer_top(), expected, "width {width}: {key:?}");
            let shown = body_rows(rig);
            assert_eq!(
                shown[0], painted[expected],
                "width {width}: {key:?} first row"
            );
            assert_eq!(
                shown[viewport - 1],
                painted[expected + viewport - 1],
                "width {width}: {key:?} last row"
            );
        };
        // End clamps: nothing scrolls past the last painted row.
        at(&mut rig, Key::Char('G'), max);
        for key in [
            Key::PageDown,
            Key::PageDown,
            Key::Char('j'),
            Key::Down,
            Key::Ctrl('d'),
        ] {
            at(&mut rig, key, max);
        }
        // Start clamps: nothing scrolls above the first.
        at(&mut rig, Key::Char('g'), 0);
        for key in [Key::PageUp, Key::Char('k'), Key::Up, Key::Ctrl('u')] {
            at(&mut rig, key, 0);
        }
        // Positive control: the same keys do move away from the ends.
        at(&mut rig, Key::PageDown, (viewport - 1).max(1));
    }
}

#[tokio::test]
async fn k7_p_and_l_page_keeping_the_root_and_the_opened_item() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.view(NEWEST).await;
    rig.paint();
    let title = rig.f.h.state.modal_stack.top().unwrap().title().to_owned();
    let newest_content = rig.inspector().content.clone();
    let newest_before = rig.inspector().omitted_before;
    let newest_first = rig.inspector().first_non_pinned_ordinal;
    assert!(
        newest_before > 0,
        "the 60-turn newest page omits older turns"
    );
    assert_eq!(rig.inspector().omitted_after, 0);
    let pinned = |rig: &Rig| {
        header_rows(rig)
            .iter()
            .any(|row| row.trim() == "thread root · ri_000")
    };
    assert!(pinned(&rig), "the root is pinned in the identity rows");

    // p: the host is asked; the page loader returns the earlier page.
    rig.keys(&[Key::PageDown]);
    assert_eq!(
        rig.key(Key::Char('p')),
        InputAction::ThreadPage { earlier: true }
    );
    rig.navigate(true).await;
    rig.paint();
    assert!(rig.is_open(), "paging retargets the one frame");
    assert_eq!(rig.f.h.state.modal_stack.top().unwrap().title(), title);
    let inspector = rig.inspector();
    assert!(
        inspector.omitted_before < newest_before,
        "older turns were loaded"
    );
    assert!(inspector.omitted_after > 0, "later turns are now omitted");
    assert!(
        inspector.first_non_pinned_ordinal < newest_first,
        "the window moved earlier"
    );
    assert_eq!(rig.viewer_top(), 0, "a new page starts at the top");
    let lines: Vec<String> = inspector
        .content
        .lines()
        .map(|l| l.trim().to_owned())
        .collect();
    for kept in ["ri_000 · received", "ri_059 · received"] {
        assert!(
            lines.iter().any(|line| line.starts_with(kept)),
            "{kept} (the root / the opened item) stays on every page"
        );
    }
    assert!(pinned(&rig), "the root row stays pinned");
    assert!(
        header_rows(&rig)[0].trim() == "item actions · ri_059",
        "the opened item stays the pinned subject"
    );
    assert!(compact(&footer_rows(&rig).join("\n")).contains("[l]Newer"));

    // l returns to the newest page, byte for byte.
    assert_eq!(
        rig.key(Key::Char('l')),
        InputAction::ThreadPage { earlier: false }
    );
    rig.navigate(false).await;
    rig.paint();
    assert!(rig.is_open());
    assert_eq!(
        rig.inspector().content,
        newest_content,
        "the newest page is restored"
    );
    assert_eq!(rig.inspector().omitted_before, newest_before);
    assert_eq!(rig.inspector().omitted_after, 0);
    assert!(!compact(&footer_rows(&rig).join("\n")).contains("Newer"));
}

#[tokio::test]
async fn k7_without_an_omitted_page_p_and_l_are_neither_painted_nor_live() {
    let mut rig = Rig::plain(consultation_workspace().await, 100, 30).await;
    rig.view("ri_c").await;
    rig.paint();
    assert_eq!(
        (
            rig.inspector().omitted_before,
            rig.inspector().omitted_after
        ),
        (0, 0)
    );
    let rail = compact(&footer_rows(&rig).join("\n"));
    assert!(
        !rail.contains("Earlier") && !rail.contains("Newer"),
        "{rail}"
    );
    assert!(
        rail.contains("Close"),
        "positive control: the rail is painted"
    );
    let content = rig.inspector().content.clone();
    for key in [Key::Char('p'), Key::Char('l')] {
        let action = rig.key(key);
        let shown = format!("{action:?}");
        assert!(
            consumed(action),
            "{key:?} must not ask the host for a page: {shown}"
        );
        rig.paint();
        assert!(rig.is_open());
        assert_eq!(rig.inspector().content, content);
    }
    // Positive control: a thread that does omit a page paints and acts on `p`.
    let mut long = Rig::plain(long_workspace().await, 100, 30).await;
    long.view(NEWEST).await;
    long.paint();
    assert!(compact(&footer_rows(&long).join("\n")).contains("[p]Earlier"));
    assert_eq!(
        long.key(Key::Char('p')),
        InputAction::ThreadPage { earlier: true }
    );
}

#[tokio::test]
async fn k7_a_peer_arrival_does_not_move_the_viewer() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.view(NEWEST).await;
    rig.paint();
    rig.keys(&[Key::PageDown, Key::PageDown]);
    rig.paint();
    let viewer = rig.inspector().viewer;
    let content = rig.inspector().content.clone();
    let counts = (
        rig.inspector().omitted_before,
        rig.inspector().omitted_after,
    );
    let body = body_rows(&rig);
    assert!(viewer.top > 0);

    // A peer's follow-up arrives in the journal while the window is open.
    rig.ws
        .receive(
            &Item::new(
                "ri_060",
                rustain::domain::models::SemanticMessageType::Consultation,
                "late arrival",
            )
            .reply_to(NEWEST),
        )
        .await;
    rig.paint();
    assert_eq!(rig.inspector().viewer, viewer, "the viewer did not move");
    assert_eq!(
        rig.inspector().content,
        content,
        "the open page did not change under the reader"
    );
    assert_eq!(
        (
            rig.inspector().omitted_before,
            rig.inspector().omitted_after
        ),
        counts
    );
    assert_eq!(body_rows(&rig), body, "the painted rows did not move");
    assert!(rig.is_open());

    // Positive control: the arrival is real; reopening the thread reads it.
    rig.view(NEWEST).await;
    rig.paint();
    assert!(
        rig.inspector().content.contains("ri_060"),
        "a fresh page of the same thread carries the arrival"
    );
    assert_eq!(rig.viewer_top(), 0, "a fresh page starts at the top");
}

#[tokio::test]
async fn k7_repaints_at_one_width_build_the_layout_once_and_a_width_change_rebuilds() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.view(NEWEST).await;
    let start = rig.inspector().layouts_built.get();
    assert_eq!(start, 0, "nothing is laid out before the first paint");
    for _ in 0..3 {
        rig.paint();
    }
    assert_eq!(
        rig.inspector().layouts_built.get(),
        start + 1,
        "three repaints at one width lay the body out exactly once"
    );
    // Scrolling re-slices the cached rows: no new layout.
    rig.key(Key::PageDown);
    rig.paint();
    assert_eq!(rig.inspector().layouts_built.get(), start + 1);

    // A changed content generation is a new layout key (the width key is exercised by the
    // clamp test at four widths: each rig lays out once at its own width).
    rig.f
        .h
        .state
        .thread_inspector
        .as_mut()
        .expect("open")
        .invalidate();
    rig.paint();
    assert_eq!(
        rig.inspector().layouts_built.get(),
        start + 2,
        "a new content generation lays out again"
    );
    rig.paint();
    rig.paint();
    assert_eq!(
        rig.inspector().layouts_built.get(),
        start + 2,
        "and is cached again"
    );
}

fn fill_composer(rig: &mut Rig) {
    rig.f.h.state.input_buffer = (0..12)
        .map(|n| format!("composer line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    rig.f.h.state.cursor_position = rig.f.h.state.input_buffer.len();
}

/// 8A-L1-01 (local half): a pager key acts exactly when its hint is PAINTED. The hints are
/// read from the painted rail cells (not from state); `rail_live` agrees with them; the
/// passive sentence is dropped, never half-painted, when it does not fit with the reader
/// rail; the title, a body row and the reader rail (`Close`) stay painted at the floor.
#[tokio::test]
async fn k7_pager_keys_page_exactly_when_their_hint_is_painted_at_the_floor_and_above() {
    for (width, height, full) in [
        (60u16, 16u16, true),
        (60, 16, false),
        (80, 24, true),
        (80, 24, false),
    ] {
        let mut rig = Rig::plain(long_workspace().await, width, height).await;
        if full {
            fill_composer(&mut rig);
        }
        rig.view(NEWEST).await;
        rig.paint();
        for stage in 0..2 {
            if stage == 1 {
                rig.navigate(true).await;
                rig.paint();
            }
            let label = format!("{width}x{height} full={full} stage={stage}");
            let (before, after) = (
                rig.inspector().omitted_before > 0,
                rig.inspector().omitted_after > 0,
            );
            assert!(before || after, "{label}: a page is omitted");
            let (live_info, live_pager) = rig.inspector().rail_live.get();
            let (p, l) = painted_pager_hints(&rig);
            assert_eq!(p, before && live_pager, "{label}: [p] painted == live");
            assert_eq!(l, after && live_pager, "{label}: [l] painted == live");
            let footer = footer_rows(&rig).join("\n");
            assert!(compact(&footer).contains("Close"), "{label}: {footer}");
            assert_eq!(
                compact(&footer).contains("localviewonly"),
                live_info,
                "{label}: the passive sentence is painted whole exactly when it is live"
            );
            assert!(title_row(&rig.f).contains("Thread · "), "{label}");
            assert!(geometry(&rig).body >= 1, "{label}: a body row");
            let page_p = rig.key(Key::Char('p')) == InputAction::ThreadPage { earlier: true };
            let page_l = rig.key(Key::Char('l')) == InputAction::ThreadPage { earlier: false };
            assert_eq!(page_p, p, "{label}: p pages iff [p] is painted");
            assert_eq!(page_l, l, "{label}: l pages iff [l] is painted");
            if !full && width == 80 {
                // Positive control: roomy terminal, empty composer: everything is painted.
                assert!(live_info && live_pager, "{label}");
                assert!(p == before && l == after, "{label}");
                assert!(page_p || page_l, "{label}: a pager key does page");
            }
        }
        if full && width == 60 {
            assert!(
                !rig.inspector().rail_live.get().0,
                "the floor with a full composer cannot hold the passive sentence and the \
                 reader rail: the sentence is not painted"
            );
        }
    }
}

/// Scroll position of the last paint compared with what is painted: after `paint` the body
/// rows shown are the clamped slice of every row the page can show (no blank frame).
fn assert_window_is_the_clamped_slice(rig: &mut Rig, label: &str) -> (usize, usize) {
    rig.paint();
    let shown = body_rows(rig);
    let top = rig.viewer_top();
    let (rows, viewport) = rig.fit();
    let painted = painted_body(rig);
    assert_eq!(painted.len(), rows, "{label}: clamped rows == painted rows");
    assert!(
        shown.iter().any(|row| !row.trim().is_empty()),
        "{label}: the frame is not blank"
    );
    let start = top.min(rows.saturating_sub(viewport));
    let end = (start + viewport).min(rows);
    assert_eq!(
        shown[..end - start],
        painted[start..end],
        "{label}: the painted body is the clamped slice (top {top}, rows {rows}, viewport {viewport})"
    );
    (rows, viewport)
}

/// 8A-L2-03(i): in ONE open frame the terminal is resized 60 -> 130 -> 60. The rig really
/// re-lays out at the new width, the cache rebuilds once per width key, and the viewer is
/// clamped to the painted rows after each change.
#[tokio::test]
async fn k7_a_same_frame_resize_rebuilds_per_width_and_the_viewer_clamps_to_painted_rows() {
    let mut rig = Rig::plain(long_workspace().await, 60, 24).await;
    rig.view(NEWEST).await;
    let start = rig.inspector().layouts_built.get();
    rig.paint();
    rig.key(Key::Char('G'));
    rig.paint();
    assert_eq!(rig.inspector().layouts_built.get(), start + 1);
    let (rows_60, _) = assert_window_is_the_clamped_slice(&mut rig, "60 at the end");
    assert_eq!(
        rig.inspector().layouts_built.get(),
        start + 1,
        "checking the painted rows at one width rebuilds nothing"
    );

    rig.key(Key::Char('G'));
    rig.paint();
    rig.resize(130, 24);
    rig.paint();
    assert_eq!(
        rig.inspector().layouts_built.get(),
        start + 2,
        "a new width is a new layout key"
    );
    let (rows_130, _) = assert_window_is_the_clamped_slice(&mut rig, "130 after widening");
    assert!(
        rows_130 < rows_60,
        "positive control: the wider terminal wraps into fewer rows ({rows_130} < {rows_60})"
    );

    rig.resize(60, 24);
    rig.paint();
    assert_eq!(rig.inspector().layouts_built.get(), start + 3);
    let (rows_back, _) = assert_window_is_the_clamped_slice(&mut rig, "60 after narrowing");
    assert_eq!(rows_back, rows_60, "the same width lays out the same rows");
    rig.paint();
    assert_eq!(
        rig.inspector().layouts_built.get(),
        start + 3,
        "repaints at the restored width reuse the layout"
    );
    assert!(rig.is_open(), "the same frame survived every resize");

    // A mid-thread position survives the same round trip clamped, never past the end.
    let mut rig = Rig::plain(long_workspace().await, 60, 24).await;
    rig.view(NEWEST).await;
    rig.paint();
    rig.keys(&[Key::PageDown, Key::PageDown, Key::PageDown]);
    rig.paint();
    assert!(
        rig.viewer_top() > 0,
        "positive control: scrolled mid-thread"
    );
    rig.resize(130, 24);
    assert_window_is_the_clamped_slice(&mut rig, "mid-thread at 130");
    rig.resize(60, 24);
    assert_window_is_the_clamped_slice(&mut rig, "mid-thread back at 60");
}

/// 8A-L2-03(iii) local: a crossterm wheel event through the host converter scrolls the thread
/// body by the clamp and reaches neither the composer, the focus nor the Stage; with no frame
/// open the same wheel is the Stage scroll request.
#[tokio::test]
async fn k7_the_wheel_scrolls_the_thread_body_by_the_clamp_and_reaches_nothing_else() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.f.h.state.input_buffer = "draft".into();
    rig.f.h.state.cursor_position = 3;
    rig.view(NEWEST).await;
    rig.paint();
    let (rows, viewport) = rig.fit();
    let max = rows - viewport;
    assert!(max > 1, "the thread spans several pages");
    let notch = usize::from(
        rustain::domain::models::MouseConfig::default()
            .wheel_lines
            .max(1),
    );
    let others = |rig: &Rig| {
        let s = &rig.f.h.state;
        format!(
            "{:?} {:?} {:?}",
            (&s.input_buffer, s.cursor_position, s.input_scroll_offset),
            (&s.focus, &s.focused_item_id),
            rig.f.tabs.active_tab().view_state.scroll_offset,
        )
    };
    let untouched = others(&rig);
    let first = body_rows(&rig);

    assert!(consumed(rig.wheel(true)), "a wheel step is state-only");
    rig.paint();
    assert_eq!(
        rig.viewer_top(),
        notch.min(max),
        "one notch scrolls by wheel_lines"
    );
    assert_ne!(body_rows(&rig), first, "the painted body moved");
    for _ in 0..max + 2 {
        assert!(consumed(rig.wheel(true)));
    }
    rig.paint();
    assert_eq!(
        rig.viewer_top(),
        max,
        "the wheel clamps at the last painted row"
    );
    assert!(consumed(rig.wheel(false)));
    rig.paint();
    assert_eq!(rig.viewer_top(), max.saturating_sub(notch));
    for _ in 0..max + 2 {
        assert!(consumed(rig.wheel(false)));
    }
    rig.paint();
    assert_eq!(rig.viewer_top(), 0, "the wheel clamps at the first row");
    assert_eq!(
        others(&rig),
        untouched,
        "composer, focus and Stage were not touched"
    );
    assert!(rig.is_open());

    // Positive control: with no frame open the same wheel is the Stage scroll request.
    rig.key(Key::Esc);
    rig.paint();
    assert_eq!(rig.depth(), 0);
    assert!(
        matches!(rig.wheel(true), InputAction::MouseScroll(_)),
        "with no frame open the wheel scrolls the Stage"
    );
}
