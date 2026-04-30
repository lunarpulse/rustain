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

use rustain::adapters::tui::state::HeightCache;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    ChatMessage, Conversation, InvocationStatus, MessageRole, StopReason,
    StreamingState, ViewState, Turn,
};
use rustain::domain::models::turn::{TurnPart, PartId};

use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn make_conversation(messages: Vec<ChatMessage>, turns: Vec<Turn>) -> Conversation {
    Conversation {
        id: "test-conv".to_string(),
        title: "Test".to_string(),
        messages,
        turns,
        created_at: 1_700_000,
        updated_at: 1_700_000,
        last_response_at: Some(1_700_000),
        session_id: Some("test-session".to_string()),
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    }
}

fn make_msg(id: &str, role: MessageRole) -> ChatMessage {
    ChatMessage {
        id: id.to_string(),
        role,
        content: String::new(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    }
}

fn make_turn(id: &str, parts: Vec<TurnPart>, stop_reason: Option<StopReason>) -> Turn {
    let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
    turn.id = rustain::domain::models::TurnId(id.to_string());
    for part in parts {
        turn.push_part(|_id| part);
    }
    turn.stop_reason = stop_reason;
    turn
}

fn prose(text: &str) -> TurnPart {
    TurnPart::Prose { id: PartId(0), text: text.to_string() }
}

fn reasoning(text: &str) -> TurnPart {
    TurnPart::Reasoning { id: PartId(0), text: text.to_string() }
}

fn tool(name: &str, status: InvocationStatus) -> TurnPart {
    let is_success = status == InvocationStatus::Success;
    TurnPart::ToolInvocation {
        id: PartId(0),
        tool: name.to_string(),
        args: serde_json::json!({}),
        status,
        started_at: 1_700_000_000_000,
        ended_at: if is_success { Some(1_700_000_005_000) } else { None },
    }
}

fn render_text(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    view_state: &ViewState,
    clock: &dyn rustain::domain::clock::Clock,
    width: u16,
    height: u16,
) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let _ = terminal.draw(|frame| {
        let area = Rect::new(0, 0, width, height);
        let streaming = StreamingState::default();
        let mut height_cache = HeightCache::default();
        let _ = chat_pane::render_with_search(
            frame, area,
            conversation, open_turn, &streaming, view_state, clock,
            0, true,
            &Theme::dark(),
            &mut height_cache,
            &HashMap::new(), &BTreeMap::new(),
            None, None, &[], &[], None,
        );
    });
    use ratatui::buffer::Buffer;
    let buffer: Buffer = terminal.backend().buffer().clone();
    let mut lines: Vec<String> = Vec::new();
    for y in 0..buffer.area().height {
        let row: String = (0..buffer.area().width)
            .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
            .collect();
        let trimmed = row.trim_end().to_string();
        if !trimmed.is_empty() {
            lines.push(trimmed);
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Snapshot fixtures (AC12 table)
// ---------------------------------------------------------------------------

/// Fixture helpers — each builds the conversation + turn + message for a fixture.

fn fixture_1_live_streaming() -> (Conversation, Turn) {
    let turn = make_turn("f1", vec![
        prose("Let me check the codebase."),
        tool("Read", InvocationStatus::Running),
        prose("Now let me run the tests."),
        tool("Bash", InvocationStatus::Running),
    ], None);
    let msg = make_msg("f1", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_2_post_collapse_tier1() -> (Conversation, Turn) {
    let turn = make_turn("f2", vec![
        prose("Let me find all the relevant files and examine them carefully."),
        tool("Read", InvocationStatus::Success),
        tool("Grep", InvocationStatus::Success),
        tool("Bash", InvocationStatus::Success),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f2", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_4_expanded_one_tool() -> (Conversation, Turn) {
    let turn = make_turn("f4", vec![
        prose("Let me read the config file."),
        tool("Read", InvocationStatus::Success),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f4", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_5_expanded_user_toggled() -> (Conversation, ViewState, Turn) {
    let turn = make_turn("f5", vec![
        prose("Analyzing."),
        tool("Read", InvocationStatus::Success),
        tool("Grep", InvocationStatus::Success),
        tool("Bash", InvocationStatus::Success),
        tool("Edit", InvocationStatus::Success),
        tool("Read", InvocationStatus::Success),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f5", MessageRole::Assistant);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), false); // user explicit expand
    (make_conversation(vec![msg], vec![turn.clone()]), vs, turn)
}

fn fixture_6_failed_auto_expanded() -> (Conversation, Turn) {
    let turn = make_turn("f6", vec![
        prose("Let me try building this."),
        tool("Read", InvocationStatus::Success),
        tool("Bash", InvocationStatus::Error),
        tool("Read", InvocationStatus::Success),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f6", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_7_prose_only() -> (Conversation, Turn) {
    let turn = make_turn("f7", vec![
        prose("hello world"),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f7", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_8_tool_only_no_prose() -> (Conversation, Turn) {
    let turn = make_turn("f8", vec![
        tool("Read", InvocationStatus::Success),
        tool("Bash", InvocationStatus::Success),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f8", MessageRole::Assistant);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id.clone(), true);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_9_mixed_with_reasoning() -> (Conversation, Turn) {
    let turn = make_turn("f9", vec![
        prose("Let me analyze this structure."),
        reasoning("The design uses a hexagonal architecture pattern which separates domain from adapters."),
        tool("Read", InvocationStatus::Success),
    ], Some(StopReason::EndTurn));
    let msg = make_msg("f9", MessageRole::Assistant);
    (make_conversation(vec![msg], vec![turn.clone()]), turn)
}

fn fixture_10_cancelled_respects_collapse() -> (Conversation, ViewState, Turn) {
    let turn = make_turn("f10", vec![
        prose("Running a long batch."),
        tool("Bash", InvocationStatus::Cancelled),
    ], Some(StopReason::EndTurn));
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
    let text = render_text(&conv, Some(&turn), &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn live_streaming_two_running_tools_w120() {
    let (conv, turn) = fixture_1_live_streaming();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, Some(&turn), &ViewState::default(), &clock, 120, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn live_streaming_two_running_tools_w200() {
    let (conv, turn) = fixture_1_live_streaming();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, Some(&turn), &ViewState::default(), &clock, 200, 60);
    insta::assert_snapshot!(text);
}

// Fixture 2: post_collapse_tier1_default (w80, w120, w200)
// Predicate auto-collapses — 3 tools, no user toggle.

#[test]
fn post_collapse_tier1_default_w80() {
    let (conv, _turn) = fixture_2_post_collapse_tier1();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn post_collapse_tier1_default_w120() {
    let (conv, _turn) = fixture_2_post_collapse_tier1();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 120, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn post_collapse_tier1_default_w200() {
    let (conv, _turn) = fixture_2_post_collapse_tier1();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 200, 60);
    insta::assert_snapshot!(text);
}

// Fixture 4: expanded_one_tool_turn (w80 only)

#[test]
fn expanded_one_tool_turn_w80() {
    let (conv, _turn) = fixture_4_expanded_one_tool();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

// Fixture 5: expanded_user_toggled_against_default (w80 only)

#[test]
fn expanded_user_toggled_against_default_w80() {
    let (conv, vs, _turn) = fixture_5_expanded_user_toggled();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &vs, &clock, 80, 60);
    insta::assert_snapshot!(text);
}

// Fixture 6: failed_invocation_auto_expanded (w80, w120, w200)

#[test]
fn failed_invocation_auto_expanded_w80() {
    let (conv, _turn) = fixture_6_failed_auto_expanded();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn failed_invocation_auto_expanded_w120() {
    let (conv, _turn) = fixture_6_failed_auto_expanded();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 120, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn failed_invocation_auto_expanded_w200() {
    let (conv, _turn) = fixture_6_failed_auto_expanded();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 200, 60);
    insta::assert_snapshot!(text);
}

// Fixture 7: prose_only_turn_no_tools (w80 only)

#[test]
fn prose_only_turn_w80() {
    let (conv, _turn) = fixture_7_prose_only();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

// Fixture 8: tool_only_turn_no_prose (w80 only)
// Collapsed line is "▸ 2 tools ✓" — no leading separator (P0-9).

#[test]
fn tool_only_turn_no_prose_w80() {
    let (conv, _turn) = fixture_8_tool_only_no_prose();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

// Fixture 9: mixed_with_reasoning_part (w80, w120, w200)
// Reasoning renders italic fg_secondary per P2-2.

#[test]
fn mixed_with_reasoning_w80() {
    let (conv, _turn) = fixture_9_mixed_with_reasoning();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 80, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn mixed_with_reasoning_w120() {
    let (conv, _turn) = fixture_9_mixed_with_reasoning();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 120, 60);
    insta::assert_snapshot!(text);
}

#[test]
fn mixed_with_reasoning_w200() {
    let (conv, _turn) = fixture_9_mixed_with_reasoning();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock, 200, 60);
    insta::assert_snapshot!(text);
}

// Fixture 10: cancelled_invocation_respects_user_collapse (w80 only)

#[test]
fn cancelled_invocation_respects_user_collapse_w80() {
    let (conv, vs, _turn) = fixture_10_cancelled_respects_collapse();
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &vs, &clock, 80, 60);
    insta::assert_snapshot!(text);
}
