#![allow(clippy::field_reassign_with_default)] // AI-12.1: test setup
//! Integration test: AnchorMode round-trip (AC6, AC8, AC9).
//!
//! Tests the full three-mode round-trip from `Following` → `Reading` →
//! `Pinned` → back to `Following` via `Submit`. Also tests `Pinned` survival
//! across `StreamAppend` reflow and defensive degradation when the pinned turn
//! is no longer in the layout.

use rustain::domain::models::turn::TurnId;
use rustain::domain::models::view_state::{
    AnchorMode, AnchorRef, LayoutMetrics, ScrollDelta, ViewEvent, ViewState,
};

fn make_test_layout(turns: &[(&str, usize)]) -> LayoutMetrics {
    let turn_top_offsets: Vec<(TurnId, usize)> = turns
        .iter()
        .map(|(id, off)| (TurnId(id.to_string()), *off))
        .collect();
    let total = turn_top_offsets.last().map(|(_, off)| off + 5).unwrap_or(0);
    LayoutMetrics {
        viewport_height: 10,
        total_content_height: total,
        turn_top_offsets,
        focused_turn_top: None,
    }
}

#[test]
fn three_mode_round_trip() {
    let mut vs = ViewState::default();
    assert_eq!(vs.mode, AnchorMode::Following);

    // Following → Reading via LineUp
    let layout = make_test_layout(&[("t0", 0), ("t1", 10), ("t2", 20), ("t3", 30)]);
    vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
    assert_eq!(vs.mode, AnchorMode::Reading);
    assert_eq!(vs.scroll_offset, 1);

    // Reading → Pinned via JumpTurn
    vs.reconcile(
        Some(ViewEvent::JumpTurn {
            turn_id: TurnId("t2".into()),
        }),
        &layout,
    );
    assert!(matches!(vs.mode, AnchorMode::Pinned(_)));
    // t2 is at lines-from-top=20, viewport=10, max_offset=35-10=25
    // offset = 25 - 20 = 5
    assert_eq!(vs.scroll_offset, 5);

    // StreamAppend extends layout — pinned turn survives reflow.
    let extended = make_test_layout(&[("t0", 0), ("t1", 10), ("t2", 20), ("t3", 30), ("t4", 40)]);
    vs.reconcile(
        Some(ViewEvent::StreamAppend { appended_lines: 2 }),
        &extended,
    );
    assert!(matches!(vs.mode, AnchorMode::Pinned(_)));
    // t2 still at 20, new max_offset = 45-10=35, offset = 35-20 = 15
    assert_eq!(vs.scroll_offset, 15);
    // pending_append_lines accumulates
    assert_eq!(vs.pending_append_lines, 2);

    // Submit → Following
    vs.reconcile(Some(ViewEvent::Submit), &extended);
    assert_eq!(vs.mode, AnchorMode::Following);
    assert_eq!(vs.scroll_offset, 0);
    assert_eq!(vs.pending_append_lines, 0);
}

#[test]
fn pinned_survives_reflow() {
    // Pinned turn at position 20. New content above pushes turn_top_offsets
    // but pinned turn_id stays at the same lines-from-top value (because
    // the new content was appended below, not above).
    let layout_before = make_test_layout(&[("t0", 0), ("t1", 10)]);
    let mut vs = ViewState::default();
    vs.mode = AnchorMode::Pinned(AnchorRef {
        turn_id: TurnId("t1".into()),
        line_in_turn: 0,
    });
    // t1 at 10, max_offset = 15-10=5, offset = 5 - 10 (neg) → clamp to 0
    // Actually let me recalculate: total = 10 (last offset) + 5 = 15.
    // max_offset = 15 - 10 = 5. offset = 5.saturating_sub(10) = 0.
    let initial = vs.reconcile(None, &layout_before);

    // StreamAppend adds new turn below, pushing total to 20.
    let layout_after = make_test_layout(&[("t0", 0), ("t1", 10), ("t2", 15)]);
    // t1 still at 10, max_offset = 20-10=10, offset = 10 - 10 = 0.
    let after = vs.reconcile(
        Some(ViewEvent::StreamAppend { appended_lines: 3 }),
        &layout_after,
    );

    assert!(matches!(vs.mode, AnchorMode::Pinned(_)));
    assert_eq!(vs.pending_append_lines, 3);
    // scroll_offset should be resolved to 0 (pinned turn's top is at viewport top or above).
    assert_eq!(after, 0);
    let _ = initial;
}

#[test]
fn pinned_degrades_when_turn_removed() {
    let layout = make_test_layout(&[("t0", 0), ("t1", 10)]);
    let mut vs = ViewState::default();
    vs.mode = AnchorMode::Pinned(AnchorRef {
        turn_id: TurnId("ghost".into()),
        line_in_turn: 0,
    });
    vs.scroll_offset = 5;

    vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 1 }), &layout);

    // Turn not found → degrade to Reading.
    assert_eq!(vs.mode, AnchorMode::Reading);
    // scroll_offset 5 clamped to max (15-10=5).
    assert_eq!(vs.scroll_offset, 5);
    // pending still accumulates.
    assert_eq!(vs.pending_append_lines, 1);
}
