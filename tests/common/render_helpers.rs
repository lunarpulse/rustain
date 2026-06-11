//! Shared render & fixture helpers for S16.x snapshot suite.
//!
//! All consumers MUST inject `MockClock` (no `Instant::now()` calls).
//! See ADR-16-01 §4.

use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::Clock;
use rustain::domain::models::turn::{PartId, TurnPart};
use rustain::domain::models::{
    ChatMessage, Conversation, InvocationStatus, LivenessSnapshot, MessageRole, StopReason,
    StreamingState, Turn, ViewState,
};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// Render helper
// ---------------------------------------------------------------------------

/// Canonical render-to-string helper for insta snapshots.
///
/// Renders a `Conversation` (plus optional open turn) through the parts-aware
/// render path (`render_with_search`) at the given terminal dimensions.
/// Returns a deterministic string of non-empty buffer rows joined by `\n`.
///
/// `streaming` defaults to `StreamingState::default()` when `None`.
/// `liveness` defaults to `None` (no live rail progress) when not provided.
pub fn render_to_string(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    view_state: &ViewState,
    clock: &dyn Clock,
    width: u16,
    height: u16,
    streaming: Option<&StreamingState>,
) -> String {
    render_to_string_ext(
        conversation,
        open_turn,
        view_state,
        clock,
        width,
        height,
        streaming,
        None,
    )
}

/// Extended render-to-string helper with optional liveness snapshot.
/// Story 16.9 — allows tests to inject `LivenessSnapshot` for the live rail.
pub fn render_to_string_ext(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    view_state: &ViewState,
    clock: &dyn Clock,
    width: u16,
    height: u16,
    streaming: Option<&StreamingState>,
    liveness: Option<&LivenessSnapshot>,
) -> String {
    let streaming = streaming.map_or_else(StreamingState::default, |s| s.clone());
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let _ = terminal.draw(|frame| {
        let area = Rect::new(0, 0, width, height);
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
            liveness,
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

// ---------------------------------------------------------------------------
// Fixture-builder helpers
// ---------------------------------------------------------------------------

pub fn make_conversation(messages: Vec<ChatMessage>, turns: Vec<Turn>) -> Conversation {
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

pub fn make_msg(id: &str, role: MessageRole) -> ChatMessage {
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
    }
}

pub fn make_turn(id: &str, parts: Vec<TurnPart>, stop_reason: Option<StopReason>) -> Turn {
    let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
    turn.id = rustain::domain::models::TurnId(id.to_string());
    for part in parts {
        turn.push_part(|_id| part);
    }
    turn.stop_reason = stop_reason;
    turn
}

pub fn prose(text: &str) -> TurnPart {
    TurnPart::Prose {
        id: PartId(0),
        text: text.to_string(),
    }
}

pub fn reasoning(text: &str) -> TurnPart {
    TurnPart::Reasoning {
        id: PartId(0),
        text: text.to_string(),
    }
}

pub fn tool(name: &str, status: InvocationStatus) -> TurnPart {
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

pub fn tool_with_path(name: &str, file_path: &str, status: InvocationStatus) -> TurnPart {
    let is_success = status == InvocationStatus::Success;
    TurnPart::ToolInvocation {
        id: PartId(0),
        tool: name.to_string(),
        args: serde_json::json!({"file_path": file_path}),
        status,
        started_at: 1_700_000_000_000,
        ended_at: if is_success {
            Some(1_700_000_005_000)
        } else {
            None
        },
    }
}
