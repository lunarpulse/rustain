#![allow(clippy::empty_line_after_doc_comments)] // AI-12.1: doc formatting
//! Snapshot tests for Story 16.4 parts-aware render at 80/120/200 columns.
//!
//! All snapshots use `Theme::dark()` (Theme has no Default impl — the spec's
//! `Theme::default()` notation referred to the canonical dark theme).
//! for deterministic elapsed-time math (P0-6 Quinn).
//!
//! # Snapshot count
//! 10 fixtures × selective widths = 14 snapshots (P0-7 trim from naive 30).
//! Width 80 is the only width where truncation/clamp behavior diverges visibly;
//! 120 and 200 are added only for fixtures where mid/wide-spacing materially differs
//! (live-stream rail layout, collapsed-tier1 separator placement, error-expand, reasoning style).
//!
//! # Updating snapshots
//! ```sh
//! cargo test --test render_snapshots
//! cargo insta accept   # after reviewing .snap.new files
//! ```

use crate::render_helpers as common;

use common::*;
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    Conversation, InvocationStatus, MessageRole, StopReason, Turn, ViewState,
};

// ---------------------------------------------------------------------------
// Test helpers (fixture builders live in common::)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Snapshot fixtures (AC12 table)
// ---------------------------------------------------------------------------

/// Fixture helpers — each builds the conversation + turn + message for a fixture.

fn fixture_1_live_streaming() -> (Conversation, Turn) {
    let turn = make_turn(
        "f1",
        vec![
            prose("Let me check the codebase."),
            tool("Read", InvocationStatus::Running),
            prose("Now let me run the tests."),
            tool("Bash", InvocationStatus::Running),
        ],
        None,
    );
    let msg = make_msg("f1", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_2_post_collapse_tier1() -> (Conversation, Turn) {
    let turn = make_turn(
        "f2",
        vec![
            prose("Let me find all the relevant files and examine them carefully."),
            tool("Read", InvocationStatus::Success),
            tool("Grep", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f2", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_3_post_collapse_tier2_after_zs_toggle() -> (Conversation, ViewState, Turn) {
    let turn = make_turn(
        "f3",
        vec![
            prose("Let me check the auth module files."),
            tool_with_path("Read", "src/auth/login.rs", InvocationStatus::Success),
            tool_with_path("Read", "src/auth/jwt.rs", InvocationStatus::Success),
            tool_with_path("Read", "src/auth/session.rs", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f3", MessageRole::Assistant);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), true);
    vs.summary_tier = rustain::domain::models::SummaryTier::Tier2;
    (make_conversation(vec![msg], vec![turn.clone()]), vs, turn)
}

fn fixture_4_expanded_one_tool() -> (Conversation, Turn) {
    let turn = make_turn(
        "f4",
        vec![
            prose("Let me read the config file."),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f4", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_5_expanded_user_toggled() -> (Conversation, ViewState, Turn) {
    let turn = make_turn(
        "f5",
        vec![
            prose("Analyzing."),
            tool("Read", InvocationStatus::Success),
            tool("Grep", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
            tool("Edit", InvocationStatus::Success),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f5", MessageRole::Assistant);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), false); // user explicit expand
    (make_conversation(vec![msg], vec![turn.clone()]), vs, turn)
}

fn fixture_6_failed_auto_expanded() -> (Conversation, Turn) {
    let turn = make_turn(
        "f6",
        vec![
            prose("Let me try building this."),
            tool("Read", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Error),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f6", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_7_prose_only() -> (Conversation, Turn) {
    let turn = make_turn("f7", vec![prose("hello world")], Some(StopReason::EndTurn));
    let msg = make_msg("f7", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_8_tool_only_no_prose() -> (Conversation, Turn) {
    let turn = make_turn(
        "f8",
        vec![
            tool("Read", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f8", MessageRole::Assistant);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), true);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_9_mixed_with_reasoning() -> (Conversation, Turn) {
    let turn = make_turn(
        "f9",
        vec![
            prose("Let me analyze this structure."),
            reasoning(
                "The design uses a hexagonal architecture pattern which separates domain from adapters.",
            ),
            tool("Read", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f9", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_10_cancelled_respects_collapse() -> (Conversation, ViewState, Turn) {
    let turn = make_turn(
        "f10",
        vec![
            prose("Running a long batch."),
            tool("Bash", InvocationStatus::Cancelled),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("f10", MessageRole::Assistant);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), true);
    (make_conversation(vec![msg], vec![turn.clone()]), vs, turn)
}

// ---------------------------------------------------------------------------
// Snapshot tests — 10 fixtures × selective widths = 14 snapshots
// ---------------------------------------------------------------------------

// Fixture 1: live_streaming_with_two_running_tools_and_prose (w80, w120, w200)
// MockClock frame pinned to 3 per AC12 table.

#[test]
fn live_streaming_two_running_tools_w80() {
    let (conv, turn) = fixture_1_live_streaming();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        80,
        60,
        None,
    );
    insta::assert_snapshot!(text);
}

#[test]
fn live_streaming_two_running_tools_w120() {
    let (conv, turn) = fixture_1_live_streaming();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        120,
        60,
        None,
    );
    insta::assert_snapshot!(text);
}

#[test]
fn live_streaming_two_running_tools_w200() {
    let (conv, turn) = fixture_1_live_streaming();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        200,
        60,
        None,
    );
    insta::assert_snapshot!(text);
}

// Fixture 2: post_collapse_tier1_default (w80, w120, w200)
// Predicate auto-collapses — 3 tools, no user toggle.

#[test]
fn post_collapse_tier1_default_w80() {
    let (conv, _turn) = fixture_2_post_collapse_tier1();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn post_collapse_tier1_default_w120() {
    let (conv, _turn) = fixture_2_post_collapse_tier1();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 120, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn post_collapse_tier1_default_w200() {
    let (conv, _turn) = fixture_2_post_collapse_tier1();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 200, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 3: post_collapse_tier2_after_zs_toggle (w80, w120, w200)
// Completed turn with 3×Read(Success) under src/auth/ — Tier-2 shows "3 reads in src/auth/".
// This fixture was deferred from S16.4 (Task 10.3) because Tier-2 was a stub identical to Tier-1.

#[test]
fn post_collapse_tier2_after_zs_toggle_w80() {
    let (conv, vs, _turn) = fixture_3_post_collapse_tier2_after_zs_toggle();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn post_collapse_tier2_after_zs_toggle_w120() {
    let (conv, vs, _turn) = fixture_3_post_collapse_tier2_after_zs_toggle();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 120, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn post_collapse_tier2_after_zs_toggle_w200() {
    let (conv, vs, _turn) = fixture_3_post_collapse_tier2_after_zs_toggle();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 200, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 4: expanded_one_tool_turn (w80 only)

#[test]
fn expanded_one_tool_turn_w80() {
    let (conv, _turn) = fixture_4_expanded_one_tool();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 5: expanded_user_toggled_against_default (w80 only)

#[test]
fn expanded_user_toggled_against_default_w80() {
    let (conv, vs, _turn) = fixture_5_expanded_user_toggled();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 6: failed_invocation_auto_expanded (w80, w120, w200)

#[test]
fn failed_invocation_auto_expanded_w80() {
    let (conv, _turn) = fixture_6_failed_auto_expanded();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn failed_invocation_auto_expanded_w120() {
    let (conv, _turn) = fixture_6_failed_auto_expanded();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 120, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn failed_invocation_auto_expanded_w200() {
    let (conv, _turn) = fixture_6_failed_auto_expanded();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 200, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 7: prose_only_turn_no_tools (w80 only)

#[test]
fn prose_only_turn_w80() {
    let (conv, _turn) = fixture_7_prose_only();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 8: tool_only_turn_no_prose (w80 only)
// Collapsed line is "▸ 2 tools ✓" — no leading separator (P0-9).

#[test]
fn tool_only_turn_no_prose_w80() {
    let (conv, _turn) = fixture_8_tool_only_no_prose();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 9: mixed_with_reasoning_part (w80, w120, w200)
// Reasoning renders italic fg_secondary per P2-2.

#[test]
fn mixed_with_reasoning_w80() {
    let (conv, _turn) = fixture_9_mixed_with_reasoning();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn mixed_with_reasoning_w120() {
    let (conv, _turn) = fixture_9_mixed_with_reasoning();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 120, 60, None);
    insta::assert_snapshot!(text);
}

#[test]
fn mixed_with_reasoning_w200() {
    let (conv, _turn) = fixture_9_mixed_with_reasoning();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &ViewState::default(), &clock, 200, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 10: cancelled_invocation_respects_user_collapse (w80 only)

#[test]
fn cancelled_invocation_respects_user_collapse_w80() {
    let (conv, vs, _turn) = fixture_10_cancelled_respects_collapse();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// ==========================================================================
// S16.7 Phase B — New snapshot fixtures (Tasks 1-3)
// ==========================================================================

// Shared 3-turn builder for AC2 (zM) and AC3 (zR)
fn fixture_11a_three_turn_setup() -> (
    Conversation,
    Turn, // turn-A
    Turn, // turn-B
    Turn, // turn-C
) {
    let turn_a = make_turn(
        "a",
        vec![
            prose("Let me check the relevant files."),
            tool("Read", InvocationStatus::Success),
            tool("Grep", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
            prose("Found the relevant sections."),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_b = make_turn(
        "b",
        vec![
            prose("Now I will analyze the architecture and make changes."),
            reasoning(
                "The hexagonal architecture separates domain from adapters - I need to add the new model to domain/models/ and wire it through the ports.",
            ),
            tool("Read", InvocationStatus::Success),
            tool("Read", InvocationStatus::Success),
            tool("Edit", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
            tool("Grep", InvocationStatus::Success),
            prose("All changes applied and tests pass."),
        ],
        Some(StopReason::EndTurn),
    );
    let turn_c = make_turn(
        "c",
        vec![
            prose("Here is the summary of everything done."),
            tool("Bash", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg_a = make_msg("a", MessageRole::Assistant);
    let msg_b = make_msg("b", MessageRole::Assistant);
    let msg_c = make_msg("c", MessageRole::Assistant);
    let conv = make_conversation(
        vec![msg_a, msg_b, msg_c],
        vec![turn_a.clone(), turn_b.clone(), turn_c.clone()],
    );
    (conv, turn_a, turn_b, turn_c)
}

// Fixture 11: zM global-collapsed (AC2)
// AC2 - `zM` global-collapsed lock. 3 turns x Tier-1 default.
// Width 80 only - collapsed lines at any width are visually identical
// (truncation point shifts but format is fixed).

#[allow(non_snake_case)]
#[test]
fn zM_global_collapsed_three_turn_conversation_w80() {
    let (conv, turn_a, turn_b, turn_c) = fixture_11a_three_turn_setup();
    let mut vs = ViewState::default();
    vs.collapse_all(&[turn_a, turn_b, turn_c]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// Fixture 12: zR global-expanded (AC3)
// AC3 - `zR` global-expanded lock. Same 3-turn fixture as Task 1;
// expanded across full viewport. Locks UX-DR-GUTTER (single left-border
// per turn) plus prose/tool spacing rules.

#[allow(non_snake_case)]
#[test]
fn zR_global_expanded_three_turn_conversation_w80() {
    let (conv, turn_a, turn_b, turn_c) = fixture_11a_three_turn_setup();
    let mut vs = ViewState::default();
    vs.expand_all(&[turn_a, turn_b, turn_c]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 80, 100, None);
    insta::assert_snapshot!(text);
}

// Fixture 13: prose_interrupted_by_tool (AC4)
// AC4 - interleaved-order regression lock. The output MUST show
// prose1/tool1/prose2/tool2/prose3 vertically. If a future change
// re-collapses prose runs (resurrecting the 2026-04-22 bug), this
// snapshot fails. See architecture-amendment-epic4-kimi-learnings.md.

fn fixture_12_prose_tool_prose_tool_prose() -> (Conversation, Turn) {
    let turn = make_turn(
        "p12",
        vec![
            prose("Let me check the codebase."),
            tool("Read", InvocationStatus::Success),
            prose("Now let me run the tests."),
            tool("Bash", InvocationStatus::Success),
            prose("Done - all green."),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("p12", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

#[test]
fn prose_interrupted_by_tool_completed_w80() {
    let (conv, turn) = fixture_12_prose_tool_prose_tool_prose();
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), false); // explicitly expanded
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_to_string(&conv, None, &vs, &clock, 80, 60, None);
    insta::assert_snapshot!(text);
}

// ==========================================================================
// S16.7 Phase D - Live rail S16.9-OFF baseline (Task 5)
// ==========================================================================

// Fixture 14: live_rail_no_progress (AC6 + AC13)
// AC6 + AC13 - S16.9-OFF baseline. Rail format `⠸ Bash` (frame=3 of
// BRAILLE_FRAMES). When S16.9 lands, sibling `live_rail_with_progress_w80`
// will lock `⠸ Bash (3/10)` and divergence-assert against this snapshot.
// See [16-9-tool-progress-stdout-tail.md] DoD when ratcheted.

fn fixture_13_live_rail_running_no_progress() -> (Conversation, Turn) {
    let turn = make_turn(
        "lr13",
        vec![
            prose("Let me run the build."),
            tool("Bash", InvocationStatus::Running),
        ],
        None, // no stop_reason → running turn
    );
    let msg = make_msg("lr13", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

#[test]
fn live_rail_no_progress_w80() {
    let (conv, turn) = fixture_13_live_rail_running_no_progress();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    let text = render_to_string(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        80,
        60,
        None,
    );
    insta::assert_snapshot!(text);
}

// ==========================================================================
// S16.9 — Live rail with progress + tail
// ==========================================================================

// Fixture 15: live_rail_running_with_progress. Same prose+Bash shape as
// fixture 13, wrapped by a LivenessSnapshot constructed by the test.
// AC13 carry-forward from S16.7: exactly 3 new snapshots, doc-commented
// width rationale per S16.7 discipline.

use rustain::domain::models::LivenessSnapshot;

fn fixture_15_liveness(progress: Option<(u64, u64)>, tail: Option<&str>) -> LivenessSnapshot {
    LivenessSnapshot {
        active_tool_name: Some("Bash".to_string()),
        progress,
        tail: tail.map(String::from),
    }
}

// live_rail_with_progress_w80 — AC2 divergence-lock.
// Width 80: baseline where truncation matters; asserts the (k/n) counter is
// visible and `assert_ne!` guards against regression with the S16.7-OFF baseline.
#[test]
fn live_rail_with_progress_w80() {
    let (conv, turn) = fixture_13_live_rail_running_no_progress();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    let liveness = fixture_15_liveness(Some((3, 10)), None);
    let text_with = render_to_string_ext(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        80,
        60,
        None,
        Some(&liveness),
    );
    let text_without = render_to_string(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        80,
        60,
        None,
    );
    assert_ne!(
        text_with, text_without,
        "S16.9 visible delta lock: (3/10) progress suffix must change rendered output"
    );
    insta::assert_snapshot!("live_rail_with_progress_w80", text_with);
}

// live_rail_with_tail_w80 — AC3 tail rendering.
// Width 80: 4 indented tail lines under the rail; truncation applies for
// lines wider than 78 chars minus 2-space indent.
#[test]
fn live_rail_with_tail_w80() {
    let (conv, turn) = fixture_13_live_rail_running_no_progress();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    let liveness = fixture_15_liveness(Some((4, 4)), Some("line1\nline2\nline3\nline4"));
    let text = render_to_string_ext(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        80,
        60,
        None,
        Some(&liveness),
    );
    insta::assert_snapshot!("live_rail_with_tail_w80", text);
}

// live_rail_with_tail_w120 — mid-width verification.
// Width 120: wider terminal allows tail lines to render without truncation.
// Uses longer lines than the w80 fixture so truncation behavior differs.
#[test]
fn live_rail_with_tail_w120() {
    let (conv, turn) = fixture_13_live_rail_running_no_progress();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    let liveness = fixture_15_liveness(
        Some((4, 4)),
        Some(
            "line1_is_longer_than_eighty_characters_so_it_will_be_truncated_in_w80_but_not_here\nline2\nline3\nline4",
        ),
    );
    let text = render_to_string_ext(
        &conv,
        Some(&turn),
        &ViewState::default(),
        &clock,
        120,
        60,
        None,
        Some(&liveness),
    );
    insta::assert_snapshot!("live_rail_with_tail_w120", text);
}

// ---------------------------------------------------------------------------
// Story 21.11 — mixed-width (CJK / ZWJ emoji / combining) representative
// snapshots at 80 cells. The semantic K2-K7 assertions live in the
// conformance module; these pin the whole rendered surface for review.
// ---------------------------------------------------------------------------

mod mixed_width {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::layout::compute_layout;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::{input_box, status_bar};
    use rustain::domain::models::{FocusState, StatusState};
    use unicode_width::UnicodeWidthStr;

    const MIXED: &str = "界界界 e\u{301}\u{301} 👨\u{200d}👩\u{200d}👧 family and 日本語のテキストが続きます, then a very long tail of ordinary words that must be clipped at a cell boundary";

    fn glyph_rows(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < buffer.area.width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += u16::try_from(symbol.width().max(1)).unwrap();
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn mixed_width_stage_collapsed_and_expanded_w80() {
        let collapsed = make_turn(
            "mw1",
            vec![
                prose(&format!("{MIXED}.")),
                tool("Read", InvocationStatus::Success),
                tool("Grep", InvocationStatus::Success),
                tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let expanded = make_turn(
            "mw2",
            vec![prose(MIXED), tool("Read", InvocationStatus::Success)],
            Some(StopReason::EndTurn),
        );
        let conv = make_conversation(
            vec![
                make_msg("mw1", MessageRole::Assistant),
                make_msg("mw2", MessageRole::Assistant),
            ],
            vec![collapsed, expanded],
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let text = render_to_string(&conv, None, &ViewState::default(), &clock, 80, 24, None);
        // The collapsed summary budgets its glyph gap and marker, so the tier
        // label and status glyph at its end survive instead of being clipped.
        let collapsed_row = text
            .lines()
            .find(|row| row.contains("▸"))
            .expect("collapsed summary row");
        assert!(
            collapsed_row.trim_end().ends_with("3 tools ✓"),
            "{collapsed_row:?}"
        );
        insta::assert_snapshot!("mixed_width_stage_w80", text);
    }

    #[test]
    fn mixed_width_shell_and_composer_w80() {
        let theme = Theme::dark();
        let draft = format!("{MIXED} {MIXED}");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let layout = compute_layout(
                    area,
                    &theme,
                    &draft,
                    1,
                    false,
                    rustain::domain::models::visual::DensityMode::Focus,
                )
                .unwrap();
                status_bar::render(
                    frame,
                    layout.status_bar,
                    "模型-mini",
                    None,
                    &StatusState::Idle,
                    &theme,
                    0,
                    &[],
                    0,
                    layout.chat_pane.height,
                    rustain::domain::models::PermissionMode::Normal,
                    None,
                    0,
                    false,
                    None,
                    false,
                    true,
                    0,
                    Some("界 help"),
                    0,
                    None,
                    None,
                    None,
                    false,
                    None,
                    None,
                    rustain::domain::models::visual::DensityMode::Focus,
                    false,
                    None,
                    rustain::adapters::tui::state::LogAwareness::default(),
                    0,
                );
                input_box::render(
                    frame,
                    layout.input_area,
                    &draft,
                    draft.chars().count(),
                    FocusState::Input,
                    &theme,
                    false,
                    input_box::tail_scroll(&draft, layout.input_area),
                    None,
                );
            })
            .unwrap();
        let text = glyph_rows(terminal.backend().buffer());
        for row in text.lines() {
            assert!(row.width() <= 80, "shell row wider than 80 cells: {row:?}");
        }
        insta::assert_snapshot!("mixed_width_shell_composer_w80", text);
    }
}

// ---------------------------------------------------------------------------
// Story 21.12 (slices 12-A, 12-B, 12-C) — markdown structure at inner widths
// 40/80/120, the table fixture and the literal user message. Blank rows are
// kept (the shared helper drops them) because vertical rhythm is part of what
// is under review.
// ---------------------------------------------------------------------------

mod markdown_structure {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use rustain::adapters::tui::state::TabRenderState;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::StreamingState;
    use rustain::domain::services::message_builder::{
        ResolvedFileContext, build_file_context_prefix,
    };
    use std::collections::BTreeMap;
    use unicode_width::UnicodeWidthStr;

    const EVERY_ELEMENT: &str = "# Release notes

## What changed

### Details

A paragraph with **bold**, *italic*, ~~gone~~, `inline  code`, a [link](https://example.test/docs) and ![diagram](x.png). Soft
breaks join; a hard break  
splits. Literal text stays: SELECT * FROM t, 2 \\* 3, Vec<String>.

- top
  - second
    - third
      - fourth
- [ ] todo
- [x] done

3. third
4. fourth

- loose one

- loose two

> quoted **text**
>
> > nested quote

---

```rust
fn main() {
\tprintln!(\"tab\");
}
```

<div>
  html block
</div>";

    /// Paint the whole conversation and return every row (blank rows kept) up
    /// to the reported content height, with the right edge trimmed.
    fn painted(conversation: &Conversation, width: u16) -> String {
        let height = 120;
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut total = 0usize;
        terminal
            .draw(|frame| {
                let mut tab = TabRenderState::default();
                total = chat_pane::render_with_search(
                    frame,
                    Rect::new(0, 0, width, height),
                    conversation,
                    None,
                    &StreamingState::default(),
                    &ViewState::default(),
                    &clock,
                    0,
                    true,
                    &Theme::dark(),
                    &mut tab,
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    &[],
                    None,
                    None,
                    None,
                    None,
                )
                .total_content_height;
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..usize::from(height))
            .take(total)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < width {
                    let symbol = buffer[(x, y as u16)].symbol();
                    row.push_str(symbol);
                    x += u16::try_from(symbol.width().max(1)).unwrap();
                }
                row.trim_end().to_owned()
            })
            .collect();
        for row in &rows {
            assert!(
                row.width() <= usize::from(width),
                "row wider than pane: {row:?}"
            );
        }
        rows.join("\n")
    }

    fn every_element(inner: u16) -> String {
        let turn = make_turn("md", vec![prose(EVERY_ELEMENT)], Some(StopReason::EndTurn));
        let conv = make_conversation(vec![make_msg("md", MessageRole::Assistant)], vec![turn]);
        painted(&conv, inner + 2)
    }

    /// Stage links (slice 12-C, OR-21-12-1): label plus dim host suffix, the
    /// suffix rules, an autolink, a wide label that wraps and unsafe targets.
    const LINKS: &str = "## Links

See [claude.ai](https://claude.ai@evil.test/) and [the docs](https://www.example.com/guide) or [example.com](https://www.example.com/x).

- [notevil.test](https://evil.test) and <https://example.com/a>
- [例え](https://例え.test/) then [local](http://127.0.0.1:8080/p)

> quoted [a label that wraps across the pane width easily](https://docs.example.org/very/long/path)

[日本語のテキスト表示確認とても長いリンクラベル](https://example.com/p)

[unsafe](javascript:alert(1)) and [relative](docs/readme.md).";

    fn links(inner: u16) -> String {
        let turn = make_turn("lk", vec![prose(LINKS)], Some(StopReason::EndTurn));
        let conv = make_conversation(vec![make_msg("lk", MessageRole::Assistant)], vec![turn]);
        painted(&conv, inner + 2)
    }

    #[test]
    fn markdown_structure_links_w40() {
        insta::assert_snapshot!("markdown_structure_links_w40", links(40));
    }

    #[test]
    fn markdown_structure_links_w80() {
        insta::assert_snapshot!("markdown_structure_links_w80", links(80));
    }

    #[test]
    fn markdown_structure_links_w120() {
        insta::assert_snapshot!("markdown_structure_links_w120", links(120));
    }

    /// Natural, aligned, wide-glyph, shrinking and collapsing tables (D10).
    const TABLES: &str = "## Tables

| Name | Qty | Note |
|:-----|----:|:----:|
| apple | 3 | fresh |
| kiwi | 12 | `a  b` and **bold** |

| Command | Description |
|---|---|
| cargo test | Runs the full test suite for the whole workspace |

| 名前 | 説明 |
|---|---|
| 界界界 | e\u{301}e\u{301} 👨\u{200d}👩\u{200d}👧 |

| alpha | bravo | delta | gamma | omega | sigma | theta |
|---|---|---|---|---|---|---|
| 11111 | 22222 | 33333 | 44444 | 55555 | 66666 | 77777 |

| a | b |
|---|---|
| 1 | 2 | 3 |";

    fn tables(inner: u16) -> String {
        let turn = make_turn("tb", vec![prose(TABLES)], Some(StopReason::EndTurn));
        let conv = make_conversation(vec![make_msg("tb", MessageRole::Assistant)], vec![turn]);
        painted(&conv, inner + 2)
    }

    #[test]
    fn markdown_structure_tables_w40() {
        insta::assert_snapshot!("markdown_structure_tables_w40", tables(40));
    }

    #[test]
    fn markdown_structure_tables_w80() {
        insta::assert_snapshot!("markdown_structure_tables_w80", tables(80));
    }

    #[test]
    fn markdown_structure_tables_w120() {
        insta::assert_snapshot!("markdown_structure_tables_w120", tables(120));
    }

    #[test]
    fn markdown_structure_every_element_w40() {
        insta::assert_snapshot!("markdown_structure_every_element_w40", every_element(40));
    }

    #[test]
    fn markdown_structure_every_element_w80() {
        insta::assert_snapshot!("markdown_structure_every_element_w80", every_element(80));
    }

    #[test]
    fn markdown_structure_every_element_w120() {
        insta::assert_snapshot!("markdown_structure_every_element_w120", every_element(120));
    }

    #[test]
    fn markdown_structure_literal_user_message_w80() {
        let prefix = build_file_context_prefix(&[ResolvedFileContext {
            path: "notes/auth.md".to_owned(),
            content: "# Auth\n\nsecret body that must not be shown".to_owned(),
            ..ResolvedFileContext::default()
        }]);
        let mut user = make_msg("u1", MessageRole::User);
        user.content = format!(
            "{prefix}literal **x** <T> and `y`\n  indented\tafter tab\n\n# not a heading\n{}",
            "a long unbroken-tail ".repeat(6)
        );
        let conv = make_conversation(vec![user], vec![]);
        insta::assert_snapshot!(
            "markdown_structure_literal_user_message_w80",
            painted(&conv, 80)
        );
    }
}
