#![allow(clippy::field_reassign_with_default, dead_code)] // AI-12.1: test setup + scaffolding
//! Integration test: fold-toggle anchor preservation (AC7).
//!
//! Constructs a 5-turn fixture with deterministic line heights, toggles a fold
//! on turn[1] (above the focused turn), and asserts the focused turn's top edge
//! stays at the same screen row before and after the toggle.

use rustain::domain::models::turn::TurnId;
use rustain::domain::models::view_state::{
    AnchorMode, AnchorRef, LayoutMetrics, ViewEvent, ViewState,
};

/// Build `LayoutMetrics` from a 5-turn fixture.
///
/// Heights: turn[0] = 5, turn[1] = 8, turn[2] = 6, turn[3] = 10, turn[4] = 4.
/// Total = 33 lines. Viewport = 10.
/// `turn_top_offsets` = [(id0, 0), (id1, 5), (id2, 13), (id3, 19), (id4, 29)].
fn make_five_turn_layout(focused_turn_top: Option<usize>) -> LayoutMetrics {
    LayoutMetrics {
        viewport_height: 10,
        total_content_height: 33,
        turn_top_offsets: vec![
            (TurnId("t0".into()), 0),
            (TurnId("t1".into()), 5),
            (TurnId("t2".into()), 13),
            (TurnId("t3".into()), 19),
            (TurnId("t4".into()), 29),
        ],
        focused_turn_top,
    }
}

/// Same fixture AFTER collapsing turn[1] (height 8 → 1, delta = -7).
///
/// New total = 33 - 7 = 26. Viewport = 10.
/// `turn_top_offsets` = [(id0, 0), (id1, 5), (id2, 6), (id3, 12), (id4, 22)].
fn make_five_turn_layout_after_fold(focused_turn_top: Option<usize>) -> LayoutMetrics {
    LayoutMetrics {
        viewport_height: 10,
        total_content_height: 26,
        turn_top_offsets: vec![
            (TurnId("t0".into()), 0),
            (TurnId("t1".into()), 5),
            (TurnId("t2".into()), 6),
            (TurnId("t3".into()), 12),
            (TurnId("t4".into()), 22),
        ],
        focused_turn_top,
    }
}

#[test]
fn reading_with_focus_preserves_screen_row() {
    // Per AC7 worked example:
    // viewport=10, max_offset_before = 33-10 = 23, S_old = 15
    // T_old(turn[2]) = 13, T_new = 6
    // top_visible_before = 23 - 15 = 8
    // focused_screen_row = 13 - 8 = 5
    // new_top_visible = 6 - 5 = 1
    // new_max_offset = 26-10 = 16
    // new_scroll_offset = 16 - 1 = 15

    let mut vs = ViewState::default();
    vs.mode = AnchorMode::Reading;
    vs.focused_turn = Some(TurnId("t2".into()));
    vs.scroll_offset = 15;

    // The layout AFTER toggle.
    let layout_after = make_five_turn_layout_after_fold(Some(6));
    let prev_max_offset = 33 - 10; // 23
    let t_old = 13usize;

    let result = vs.reconcile(
        Some(ViewEvent::FoldToggle {
            turn_id: TurnId("t1".into()),
            prev_focused_turn_top: Some(t_old),
            prev_max_offset,
        }),
        &layout_after,
    );

    // Focused turn top should stay at screen row 5.
    // Offsets: max_offset_after=16, new_top_visible=1, result=15.
    assert_eq!(result, 15);
    assert_eq!(vs.scroll_offset, 15);
    assert_eq!(vs.mode, AnchorMode::Reading);

    // Sanity check: top_visible_after = max_offset_after - scroll_offset = 16 - 15 = 1
    // focused turn top = T_new = 6
    // screen row = 6 - 1 = 5 — matches.
}

#[test]
fn pinned_with_focus_preserves_screen_row() {
    // Same fixture, but mode is Pinned. Result should be identical to reading test.
    let mut vs = ViewState::default();
    vs.mode = AnchorMode::Pinned(AnchorRef {
        turn_id: TurnId("t2".into()),
        line_in_turn: 0,
    });
    vs.focused_turn = Some(TurnId("t2".into()));
    vs.scroll_offset = 15;

    let layout_after = make_five_turn_layout_after_fold(Some(6));
    let prev_max_offset = 33 - 10; // 23

    let result = vs.reconcile(
        Some(ViewEvent::FoldToggle {
            turn_id: TurnId("t1".into()),
            prev_focused_turn_top: Some(13),
            prev_max_offset,
        }),
        &layout_after,
    );

    assert_eq!(result, 15);
    assert_eq!(vs.scroll_offset, 15);
    // Mode should remain Pinned.
    assert!(matches!(vs.mode, AnchorMode::Pinned(_)));
}

#[test]
fn no_focus_fallback_clamps_to_new_max() {
    let mut vs = ViewState::default();
    vs.focused_turn = None;
    vs.scroll_offset = 20;
    vs.mode = AnchorMode::Reading;

    let layout_after = make_five_turn_layout_after_fold(None);
    let prev_max_offset = 33 - 10; // 23

    let result = vs.reconcile(
        Some(ViewEvent::FoldToggle {
            turn_id: TurnId("t1".into()),
            prev_focused_turn_top: None,
            prev_max_offset,
        }),
        &layout_after,
    );

    // new_max_offset = 26 - 10 = 16. scroll_offset 20 > 16 → clamp to 16.
    assert_eq!(result, 16);
    assert_eq!(vs.scroll_offset, 16);
}

#[test]
fn following_mode_foldtoggle_is_noop() {
    // Following + FoldToggle → Reading per transition table (view_state.rs:109).
    // The offset is anchor-preserved, not snap-to-bottom. Since the focused
    // turn is t2 at top=6 in the post-fold layout, max_offset=16, and
    // scroll_offset = max_offset - focused_top = 16 - 6 = 10.
    let mut vs = ViewState::default();
    vs.mode = AnchorMode::Following;
    vs.scroll_offset = 0;
    vs.focused_turn = Some(TurnId("t2".into()));

    let layout_after = make_five_turn_layout_after_fold(Some(6));
    let prev_max_offset = 33 - 10; // 23

    let result = vs.reconcile(
        Some(ViewEvent::FoldToggle {
            turn_id: TurnId("t1".into()),
            prev_focused_turn_top: Some(13),
            prev_max_offset,
        }),
        &layout_after,
    );

    assert_eq!(vs.mode, AnchorMode::Reading);
    assert_eq!(result, 10);
    assert_eq!(vs.scroll_offset, 10);
}
