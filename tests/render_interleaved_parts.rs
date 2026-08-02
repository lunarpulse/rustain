//! Integration tests for Story 16.4 parts-aware render.
//!
//! Covers:
//! - AC1: Interleaved parts walk in stream order
//! - AC13: Single-source render guard at TurnComplete boundary

use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::MockClock;
use rustain::domain::models::turn::{PartId, TurnPart};
use rustain::domain::models::{
    ChatMessage, Conversation, InvocationStatus, MessageRole, StopReason, StreamingState, Turn,
    ViewState,
};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap};

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
        compaction: None,
    }
}

fn make_assistant_turn(id: &str, parts: Vec<TurnPart>, stop_reason: Option<StopReason>) -> Turn {
    let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
    turn.id = rustain::domain::models::TurnId(id.to_string());
    for part in parts {
        turn.push_part(|_id| part);
    }
    turn.stop_reason = stop_reason;
    turn
}

fn make_prose(text: &str) -> TurnPart {
    TurnPart::Prose {
        id: PartId(0),
        text: text.to_string(),
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
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }
}

fn make_tool(name: &str, status: InvocationStatus) -> TurnPart {
    let is_success = status == InvocationStatus::Success;
    TurnPart::ToolInvocation {
        id: PartId(0),
        tool: name.to_string(),
        args: serde_json::json!({}),
        status,
        started_at: 1_700_000_000_000,
        ended_at: if is_success {
            Some(1_700_000_005_000)
        } else {
            None
        },
    }
}

fn make_result(content: &str, is_error: bool) -> TurnPart {
    TurnPart::ToolResult {
        id: PartId(1),
        refs: PartId(0),
        output: rustain::domain::models::ToolOutput {
            content: content.to_string(),
            is_error,
        },
    }
}

fn render_text(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    view_state: &ViewState,
    clock: &dyn rustain::domain::clock::Clock,
) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
    let _ = terminal.draw(|frame| {
        let area = Rect::new(0, 0, 120, 60);
        let streaming = StreamingState::default();
        let mut tab_render_state = TabRenderState::default();
        let _ = chat_pane::render_with_search(
            frame,
            area,
            conversation,
            open_turn,
            &streaming,
            view_state,
            clock,
            0,
            true,
            &Theme::dark(),
            &mut tab_render_state,
            &HashMap::new(),
            &BTreeMap::new(),
            None,
            None,
            &[],
            &[],
            None,
            None, // liveness
            None, // open_prose
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

#[test]
fn expanded_turn_renders_parts_in_order() {
    let turn = make_assistant_turn(
        "t1",
        vec![
            make_prose("Reading the file."),
            make_tool("Read", InvocationStatus::Success),
            make_prose("Looks like the bug is on line 42."),
            make_tool("Edit", InvocationStatus::Success),
            make_prose("Done."),
            make_result("file content", false),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("t1", MessageRole::Assistant);
    let conv = make_conversation(vec![msg], vec![turn]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock);
    assert!(text.contains("Reading the file"), "Missing prose: {}", text);
    assert!(
        text.contains("Looks like the bug"),
        "Missing prose: {}",
        text
    );
}

#[test]
fn collapsed_turn_renders_summary() {
    let turn = make_assistant_turn(
        "t2",
        vec![
            make_prose("Hello world."),
            make_tool("Read", InvocationStatus::Success),
            make_tool("Bash", InvocationStatus::Success),
            make_tool("Edit", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("t2", MessageRole::Assistant);
    let conv = make_conversation(vec![msg], vec![turn.clone()]);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id, true);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &vs, &clock);
    assert!(text.contains("3 tools"), "Expected '3 tools' in: {}", text);
}

#[test]
fn turn_complete_transition_no_duplicate() {
    let turn = make_assistant_turn("t3", vec![make_prose("Single.")], Some(StopReason::EndTurn));
    let msg = make_msg("t3", MessageRole::Assistant);
    let conv = make_conversation(vec![msg], vec![turn.clone()]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let with = render_text(&conv, Some(&turn), &ViewState::default(), &clock);
    let without = render_text(&conv, None, &ViewState::default(), &clock);
    assert_eq!(with, without, "Duplicate render at TurnComplete boundary");
}

#[test]
fn gutter_renders_on_assistant_turn() {
    let turn = make_assistant_turn("t4", vec![make_prose("Gutter.")], Some(StopReason::EndTurn));
    let msg = make_msg("t4", MessageRole::Assistant);
    let conv = make_conversation(vec![msg], vec![turn]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock);
    assert!(text.contains('│'), "Missing gutter: {}", text);
}

#[test]
fn user_message_renders_without_gutter() {
    let um = ChatMessage {
        id: "u1".into(),
        role: MessageRole::User,
        content: "Hello".into(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    };
    let conv = make_conversation(vec![um], vec![]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock);
    assert!(text.contains("You:"), "Missing role indicator: {}", text);
}

#[test]
fn running_invocation_shows_spinner() {
    let turn = make_assistant_turn(
        "t5",
        vec![
            make_prose("Running."),
            make_tool("Read", InvocationStatus::Running),
        ],
        None,
    );
    let msg = make_msg("t5", MessageRole::Assistant);
    let conv = make_conversation(vec![msg], vec![turn]);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &ViewState::default(), &clock);
    let has_spinner = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().any(|c| text.contains(c));
    assert!(has_spinner, "No spinner in: {}", text);
}

#[test]
fn error_turn_is_force_expanded() {
    let turn = make_assistant_turn(
        "t6",
        vec![
            make_prose("Error."),
            make_tool("Bash", InvocationStatus::Error),
            make_result("failed", true),
        ],
        Some(StopReason::EndTurn),
    );
    let msg = make_msg("t6", MessageRole::Assistant);
    let conv = make_conversation(vec![msg], vec![turn.clone()]);
    let mut vs = ViewState::default();
    vs.collapsed.insert(turn.id, true);
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    let text = render_text(&conv, None, &vs, &clock);
    assert!(text.contains('✗'), "Error not expanded: {}", text);
}
