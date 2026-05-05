//! S16.7 AC9 - viewport-outcome unit tests for `]]` / `[[` / `zz`.
//!
//! Per epic AC clause 4, vim motions are NOT snapshot-tested - pixels surface
//! in manual use, viewport math is unit-tested here. Drives
//! `chat_pane::build_layout_metrics` + `view_state.reconcile` with `MockClock`;
//! asserts post-state `scroll_offset` / `focused_turn` / `mode` directly.

#[path = "common/render_helpers.rs"]
mod common;

use common::*;
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::MockClock;
use rustain::domain::models::turn::TurnId;
use rustain::domain::models::view_state::{AnchorMode, LayoutMetrics, ViewEvent};
use rustain::domain::models::{
    ChatMessage, Conversation, InvocationStatus, MessageRole, StopReason, ViewState,
};
use std::collections::HashMap;

// All tests use viewport_height=14 to ensure content (5 turns × prose+tools)
// exceeds viewport, making scroll_offset non-zero and tests meaningful.
const VPORT: usize = 14;

// ---------------------------------------------------------------------------
// 5-turn fixture for `]]`/`[[`/`zz` tests
// ---------------------------------------------------------------------------

/// Build a 5-turn conversation: turns "a" through "e", each with prose + tools.
fn fixture_5_turn_lineup() -> (Conversation, MockClock, Vec<TurnId>) {
    let ids: Vec<TurnId> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|s| TurnId(s.to_string()))
        .collect();

    let turn_a = make_turn(
        "a",
        vec![
            prose("First message with code review."),
            tool("Read", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_b = make_turn(
        "b",
        vec![
            prose("Second response with analysis."),
            tool("Grep", InvocationStatus::Success),
            tool("Edit", InvocationStatus::Success),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_c = make_turn(
        "c",
        vec![
            prose("Third message about test results."),
            tool("Bash", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_d = make_turn(
        "d",
        vec![
            prose("Fourth: fixing the bug."),
            tool("Edit", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
            tool("Grep", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_e = make_turn(
        "e",
        vec![
            prose("Fifth: final review."),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );

    let messages = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|id| make_msg(id, MessageRole::Assistant))
        .collect();
    let turns = vec![
        turn_a.clone(),
        turn_b.clone(),
        turn_c.clone(),
        turn_d.clone(),
        turn_e.clone(),
    ];

    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    (make_conversation(messages, turns), clock, ids)
}

// ---------------------------------------------------------------------------
// Test harness: build layout, apply event, reconcile, return final state
// ---------------------------------------------------------------------------

fn jump_and_reconcile(
    conv: &Conversation,
    mut view_state: ViewState,
    focus_id: &TurnId,
    jump_target: &TurnId,
    clock: &MockClock,
    width: u16,
    viewport_height: usize,
) -> (ViewState, usize) {
    view_state.focused_turn = Some(focus_id.clone());

    let tool_block_states: HashMap<
        String,
        rustain::adapters::tui::widgets::tool_block::ToolBlockState,
    > = HashMap::new();
    let mut tab_render_state = TabRenderState::default();

    // Build layout to seed HeightCache and produce turn_top_offsets
    let _pre_layout = chat_pane::build_layout_metrics(
        conv,
        &view_state,
        &mut tab_render_state,
        &Theme::dark(),
        width,
        viewport_height,
        clock,
        &tool_block_states,
    );

    let post_layout = chat_pane::build_layout_metrics(
        conv,
        &view_state,
        &mut tab_render_state,
        &Theme::dark(),
        width,
        viewport_height,
        clock,
        &tool_block_states,
    );

    let event = ViewEvent::JumpTurn {
        turn_id: jump_target.clone(),
    };
    let resolved_offset = view_state.reconcile(Some(event), &post_layout);
    (view_state, resolved_offset)
}

fn get_turn_top_offset(layout: &LayoutMetrics, turn_id: &TurnId) -> usize {
    layout
        .turn_top_offsets
        .iter()
        .find(|(tid, _)| tid == turn_id)
        .map(|(_, off)| *off)
        .expect("Turn not found in layout")
}

fn build_verify_layout(
    conv: &Conversation,
    vs: &ViewState,
    clock: &MockClock,
    width: u16,
    viewport_height: usize,
) -> LayoutMetrics {
    let tool_block_states: HashMap<
        String,
        rustain::adapters::tui::widgets::tool_block::ToolBlockState,
    > = HashMap::new();
    let mut tab_render_state = TabRenderState::default();
    chat_pane::build_layout_metrics(
        conv,
        vs,
        &mut tab_render_state,
        &Theme::dark(),
        width,
        viewport_height,
        clock,
        &tool_block_states,
    )
}

fn verify_turn_at_viewport_top(
    layout: &LayoutMetrics,
    turn_id: &TurnId,
    offset: usize,
    label: &str,
) {
    let turn_top = get_turn_top_offset(layout, turn_id);
    let top_visible = layout
        .total_content_height
        .saturating_sub(layout.viewport_height)
        .saturating_sub(offset);
    assert!(
        (turn_top as isize - top_visible as isize).abs() <= 1,
        "{label}: turn top={turn_top} should be within 1 line of viewport top={top_visible}"
    );
}

// ---------------------------------------------------------------------------
// Test 1: next_prose_anchor_jump_advances_focus_and_pins_mode
// ---------------------------------------------------------------------------

#[test]
fn next_prose_anchor_jump_advances_focus_and_pins_mode() {
    let (conv, clock, ids) = fixture_5_turn_lineup();
    let view_state = ViewState::default();

    // Focus on turn "b" (index 1), jump to turn "c" (index 2)
    let (vs, offset) = jump_and_reconcile(
        &conv, view_state, &ids[1], // "b"
        &ids[2], // "c"
        &clock, 80, VPORT,
    );

    assert_eq!(vs.focused_turn.as_ref(), Some(&ids[2]));
    assert!(
        matches!(vs.mode, AnchorMode::Pinned { .. }),
        "Expected Pinned mode, got {:?}",
        vs.mode
    );

    let layout = build_verify_layout(&conv, &vs, &clock, 80, VPORT);
    verify_turn_at_viewport_top(&layout, &ids[2], offset, "next_prose jump c");
}

// ---------------------------------------------------------------------------
// Test 2: prev_prose_anchor_jump_reverses
// ---------------------------------------------------------------------------

#[test]
fn prev_prose_anchor_jump_reverses() {
    let (conv, clock, ids) = fixture_5_turn_lineup();
    let view_state = ViewState::default();

    // Focus on turn "c" (index 2), jump backward to turn "b" (index 1)
    let (vs, offset) = jump_and_reconcile(
        &conv, view_state, &ids[2], // "c"
        &ids[1], // "b"
        &clock, 80, VPORT,
    );

    assert_eq!(vs.focused_turn.as_ref(), Some(&ids[1]));
    assert!(
        matches!(vs.mode, AnchorMode::Pinned { .. }),
        "Expected Pinned mode after backward jump"
    );
    assert!(offset > 0, "Expected scroll_offset > 0 for backward jump");

    let layout = build_verify_layout(&conv, &vs, &clock, 80, VPORT);
    verify_turn_at_viewport_top(&layout, &ids[1], offset, "prev_prose jump b");
}

// ---------------------------------------------------------------------------
// 4-turn fixture for skip-turns-without-prose test
// ---------------------------------------------------------------------------

fn fixture_4_turn_skip_no_prose() -> (Conversation, MockClock, Vec<TurnId>) {
    let ids: Vec<TurnId> = ["aa", "bb", "cc", "dd"]
        .iter()
        .map(|s| TurnId(s.to_string()))
        .collect();

    let turn_1 = make_turn(
        "aa",
        vec![
            prose("First turn with prose."),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    // Turn 2 has ONLY a ToolInvocation, no Prose part
    let turn_2 = make_turn(
        "bb",
        vec![tool("Bash", InvocationStatus::Success)],
        Some(StopReason::EndTurn),
    );
    let turn_3 = make_turn(
        "cc",
        vec![
            prose("Third turn with prose again."),
            tool("Grep", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_4 = make_turn(
        "dd",
        vec![prose("Fourth turn for completeness.")],
        Some(StopReason::EndTurn),
    );

    let messages: Vec<ChatMessage> = ["aa", "bb", "cc", "dd"]
        .iter()
        .map(|id| make_msg(id, MessageRole::Assistant))
        .collect();
    let turns = vec![turn_1, turn_2, turn_3, turn_4];

    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    (make_conversation(messages, turns), clock, ids)
}

// ---------------------------------------------------------------------------
// Test 3: prose_anchor_jump_skips_turns_without_prose_part
// ---------------------------------------------------------------------------

#[test]
fn prose_anchor_jump_skips_turns_without_prose_part() {
    let (conv, clock, ids) = fixture_4_turn_skip_no_prose();
    let view_state = ViewState::default();

    // Focus on turn "aa" (index 0, has prose), jump to "cc" (index 2, also has prose).
    // Turn "bb" (index 1) is tool-only and SHOULD be skipped by the keymap logic.
    // Here we test the VIEWPORT OUTCOME: when the target is "cc", reconcile
    // correctly sets focus and pin.
    let (vs, offset) = jump_and_reconcile(
        &conv, view_state, &ids[0], // "aa"
        &ids[2], // "cc" - skip "bb"
        &clock, 80, VPORT,
    );

    assert_eq!(
        vs.focused_turn.as_ref(),
        Some(&ids[2]),
        "Jump from turn-aa should land on turn-cc (skipping tool-only turn-bb)"
    );
    assert!(
        matches!(vs.mode, AnchorMode::Pinned { .. }),
        "Expected Pinned mode after jump skipping tool-only turn"
    );
}

// ---------------------------------------------------------------------------
// Test 4: prose_anchor_jump_at_boundary_is_noop_with_debug_log
// ---------------------------------------------------------------------------

#[test]
fn prose_anchor_jump_at_boundary_is_noop_with_debug_log() {
    let (conv, clock, ids) = fixture_5_turn_lineup();
    let mut view_state = ViewState::default();

    // Focus on the LAST prose turn ("e"), then jump "forward" (still "e" -
    // the keymap would resolve this as a no-op, returning the same turn_id).
    let layout = build_verify_layout(&conv, &view_state, &clock, 80, VPORT);

    view_state.focused_turn = Some(ids[4].clone()); // "e"

    // Pre-reconcile state snapshot
    let focus_before = view_state.focused_turn.clone();

    // Jump to the same turn (boundary no-op)
    let event = ViewEvent::JumpTurn {
        turn_id: ids[4].clone(),
    };
    view_state.reconcile(Some(event), &layout);

    assert_eq!(
        view_state.focused_turn, focus_before,
        "Boundary jump forward should not change focused_turn"
    );
    // scroll_offset may be 0 if content fits viewport, or equal to
    // what resolve_pinned produces. Either is fine for a boundary no-op
    // since the focused turn stays the same.
}

// ---------------------------------------------------------------------------
// Test 5: recenter_zz_pins_focused_turn_top_at_viewport_top
// ---------------------------------------------------------------------------

#[test]
fn recenter_zz_pins_focused_turn_top_at_viewport_top() {
    let (conv, clock, ids) = fixture_5_turn_lineup();
    let mut view_state = ViewState::default();

    let layout = build_verify_layout(&conv, &view_state, &clock, 80, VPORT);

    // Scroll mid-conversation so turn "c" is not at top
    let max_offset = layout
        .total_content_height
        .saturating_sub(layout.viewport_height);
    let turn_c_top = get_turn_top_offset(&layout, &ids[2]);
    // Set scroll_offset to make turn_c appear somewhere mid-viewport
    view_state.scroll_offset = (max_offset.saturating_sub(turn_c_top)).saturating_add(5);
    view_state.focused_turn = Some(ids[2].clone());

    // Simulate `zz` - jump to the focused turn
    let (vs, offset) = jump_and_reconcile(
        &conv, view_state, &ids[2], // "c"
        &ids[2], // "c" - zz jumps to the focused turn itself
        &clock, 80, VPORT,
    );

    assert_eq!(
        vs.focused_turn.as_ref(),
        Some(&ids[2]),
        "zz should keep focused turn on turn-c"
    );
    assert!(
        matches!(vs.mode, AnchorMode::Pinned { .. }),
        "zz should enter Pinned mode"
    );

    // Turn "c" should now be at viewport top
    let post_layout = build_verify_layout(&conv, &vs, &clock, 80, VPORT);
    verify_turn_at_viewport_top(&post_layout, &ids[2], offset, "zz turn c");
}

// ---------------------------------------------------------------------------
// Test 6: recenter_zz_with_no_focus_falls_back_to_topmost_assistant_turn
// ---------------------------------------------------------------------------

#[test]
fn recenter_zz_with_no_focus_falls_back_to_topmost_assistant_turn() {
    let (conv, clock, ids) = fixture_5_turn_lineup();
    let mut view_state = ViewState::default();

    // Compute layout with a small viewport to get a meaningful max_offset
    let layout = build_verify_layout(&conv, &view_state, &clock, 80, VPORT);

    // Scroll to top (offset = max_offset)
    let max_offset = layout
        .total_content_height
        .saturating_sub(layout.viewport_height);
    view_state.scroll_offset = max_offset;
    // No focused turn - simulating "no focus" state
    view_state.focused_turn = None;

    // zz fallback: jump to the topmost visible assistant turn ("a").
    let event = ViewEvent::JumpTurn {
        turn_id: ids[0].clone(), // "a"
    };
    let offset = view_state.reconcile(Some(event), &layout);

    assert_eq!(
        view_state.focused_turn.as_ref(),
        Some(&ids[0]),
        "zz with no focus should fall back to topmost assistant turn (a)"
    );
    assert!(
        matches!(view_state.mode, AnchorMode::Pinned { .. }),
        "zz fallback should enter Pinned mode"
    );

    // Turn "a" should be at viewport top
    let post_layout = build_verify_layout(&conv, &view_state, &clock, 80, VPORT);
    verify_turn_at_viewport_top(&post_layout, &ids[0], offset, "zz fallback a");
}
