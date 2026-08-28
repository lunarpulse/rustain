use std::cell::RefCell;
use std::collections::HashMap;

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, FeedbackBlock, FeedbackLevel, MessageRole,
    StreamingPhase, StreamingState, ToolCallInfo, ToolResultInfo,
};

fn make_conversation(messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: "test".to_string(),
        title: String::new(),
        messages,
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

/// AC8: Empty conversation shows Welcome screen.
// Covers: FR7 (chat pane rendering)
#[test]
fn test_chat_pane_empty_shows_welcome() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![]);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Welcome to Rustain."),
        "Expected welcome message, got: {}",
        text.trim()
    );
}

/// AC9: User message renders with "You:" prefix.
// Covers: FR7 (chat pane rendering)
#[test]
fn test_chat_pane_shows_user_message() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![ChatMessage {
        synthetic: false,
        id: rustain::domain::models::generate_conversation_id(),
        role: MessageRole::User,
        content: "Hello world".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }]);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(text.contains("You:"), "Expected 'You:' prefix");
    assert!(
        text.contains("Hello world"),
        "Expected message content 'Hello world'"
    );
}

/// Assistant message renders with "Assistant:" prefix.
// Covers: FR7 (chat pane rendering), FR2 (content blocks)
#[test]
fn test_chat_pane_shows_assistant_message() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![ChatMessage {
        synthetic: false,
        id: rustain::domain::models::generate_conversation_id(),
        role: MessageRole::Assistant,
        content: "Hi there".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }]);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(text.contains("Assistant:"), "Expected 'Assistant:' prefix");
    assert!(text.contains("Hi there"), "Expected message content");
}

/// AC1: Typing indicator shows when streaming with empty buffer.
// Covers: FR1 (streaming)
#[test]
fn test_chat_pane_typing_indicator() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![]);
    let streaming = StreamingState {
        is_streaming: true,
        phase: StreamingPhase::AccumulatingText,
        current_text_buffer: String::new(),
        current_blocks: vec![],
        active_tool_calls: Default::default(),
        thinking_buffer: String::new(),
    };
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("···"),
        "Expected typing indicator '···', got: {}",
        text.trim()
    );
}

/// AC2: Streaming with buffer content shows partial text.
// Covers: FR7 (chat pane rendering), FR1 (streaming), FR2 (content blocks), FR13 (auto-scroll)
#[test]
fn test_chat_pane_streaming_text() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![]);
    let streaming = StreamingState {
        is_streaming: true,
        phase: StreamingPhase::AccumulatingText,
        current_text_buffer: "partial response".to_string(),
        current_blocks: vec![],
        active_tool_calls: Default::default(),
        thinking_buffer: String::new(),
    };
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(text.contains("Assistant:"), "Expected 'Assistant:' prefix");
    assert!(
        text.contains("partial response"),
        "Expected streaming content"
    );
}

/// AC9: User message appears above typing indicator.
// Covers: FR7 (chat pane rendering), FR1 (streaming), FR2 (content blocks), FR13 (auto-scroll)
#[test]
fn test_chat_pane_user_message_before_typing_indicator() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![ChatMessage {
        synthetic: false,
        id: rustain::domain::models::generate_conversation_id(),
        role: MessageRole::User,
        content: "My question".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }]);
    let streaming = StreamingState {
        is_streaming: true,
        phase: StreamingPhase::AccumulatingText,
        current_text_buffer: String::new(),
        current_blocks: vec![],
        active_tool_calls: Default::default(),
        thinking_buffer: String::new(),
    };
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(text.contains("You:"), "Expected user message");
    assert!(text.contains("My question"), "Expected user question");
    assert!(text.contains("···"), "Expected typing indicator");

    // Verify ordering: "You:" appears before "···"
    let you_pos = text.find("You:").unwrap();
    let indicator_pos = text.find("···").unwrap();
    assert!(
        you_pos < indicator_pos,
        "User message should appear before typing indicator"
    );
}

/// AC10: Error messages display with error styling.
// Covers: FR7 (chat pane rendering), FR14 (retry/backoff), FR2 (content blocks)
#[test]
fn test_chat_pane_error_displays_in_red() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![ChatMessage {
        synthetic: false,
        id: rustain::domain::models::generate_conversation_id(),
        role: MessageRole::Assistant,
        content: "Something went wrong".to_string(),
        content_blocks: vec![ContentBlockType::Error],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }]);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(text.contains("Assistant:"), "Expected 'Assistant:' prefix");
    assert!(
        text.contains("Something went wrong"),
        "Expected error content"
    );

    // Verify error styling: check that the cell has the error color
    let buf = terminal.backend().buffer().clone();
    let error_color = theme.colors.error;
    let has_error_color = buf
        .content()
        .iter()
        .any(|cell| cell.fg == error_color && cell.symbol() != " ");
    assert!(has_error_color, "Expected error content with error color");
}

/// AC10: Streaming error displays with error styling.
// Covers: FR7 (chat pane rendering), FR14 (retry/backoff), FR2 (content blocks)
#[test]
fn test_chat_pane_streaming_error() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let conversation = make_conversation(vec![]);
    let streaming = StreamingState {
        is_streaming: true,
        phase: StreamingPhase::AccumulatingText,
        current_text_buffer: "API error occurred".to_string(),
        current_blocks: vec![ContentBlockType::Error],
        active_tool_calls: Default::default(),
        thinking_buffer: String::new(),
    };
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("API error occurred"),
        "Expected error content"
    );

    // Verify error styling
    let buf = terminal.backend().buffer().clone();
    let error_color = theme.colors.error;
    let has_error_color = buf
        .content()
        .iter()
        .any(|cell| cell.fg == error_color && cell.symbol() != " ");
    assert!(has_error_color, "Expected streaming error with error color");
}

/// AC1 (DF-079): Feedback blocks are visible when auto_scroll is true and content fills viewport.
///
/// Regression test: when total_content_height was computed without feedback block heights,
/// visible_start / visible_end were calculated before feedback was included, so feedback
/// blocks rendered below the viewport window and were silently dropped.
// Covers: FR7 (chat pane rendering), FR15 (feedback blocks)
#[test]
fn test_feedback_block_visible_with_auto_scroll() {
    // Height 10: 3 messages × 2 lines each + 2 × 2 spacing = 10 lines exactly fills viewport.
    // With a feedback block (height ≥ 1), total content is 13+ lines.
    // auto_scroll = true must include the feedback block in the viewport.
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    let messages: Vec<ChatMessage> = (1..=3)
        .map(|i| ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: format!(
                "User message {
        }",
                i
            ),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: Default::default(),
            retracted_at_ms: None,
        })
        .collect();
    let conversation = make_conversation(messages);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    let mut feedback_blocks = std::collections::BTreeMap::new();
    feedback_blocks.insert(
        "fb1".to_string(),
        FeedbackBlock {
            id: "fb1".to_string(),
            level: FeedbackLevel::Info,
            message: "Crash recovery notice".to_string(),
            actions: vec![],
        },
    );

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &feedback_blocks,
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Crash recovery notice"),
        "Expected feedback block text in viewport with auto_scroll=true, got: {}",
        text.trim()
    );
}

/// AC2 (DF-061): Height cache and block_boundaries stay coherent after tool block expand.
///
/// When a tool block is expanded the height cache must be invalidated so that:
/// (a) block_boundaries entries reflect the new expanded height, and
/// (b) the cache entry for the message matches the expanded height.
// Covers: FR7 (chat pane rendering), FR5 (tool blocks), FR13 (navigation)
#[test]
fn test_tool_block_expand_updates_cache_and_boundaries() {
    let backend = TestBackend::new(80, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    // A tool call with 3 output lines; collapsed height=1, expanded height=3+3=6.
    // Message text height = 1 (role) + markdown::compute_height("Hello") = 1 + 2 = 3
    // (markdown pipeline appends a blank line after each paragraph block). Total:
    //   collapsed: 3 + 1 = 4
    //   expanded:  3 + 6 = 9
    let tool_id = "tc1".to_string();
    let tc = ToolCallInfo {
        id: tool_id.clone(),
        name: "Bash".to_string(),
        input: serde_json::json!({"command": "ls"}),
        result: Some(ToolResultInfo {
            content: "output1\noutput2\noutput3".to_string(),
            is_error: false,
            diff: rustain::domain::models::WriteDiffState::NotAWrite,
        }),
        started_at_ms: Some(0),
        completed_at_ms: Some(1000),
        status: None,
    };
    let msg_id = rustain::domain::models::generate_conversation_id();
    let conversation = make_conversation(vec![ChatMessage {
        synthetic: false,
        id: msg_id.clone(),
        role: MessageRole::User,
        content: "Hello".to_string(),
        content_blocks: vec![],
        tool_calls: vec![tc],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }]);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // --- First render: collapsed (default) ---
    let mut collapsed_states = HashMap::new();
    collapsed_states.insert(tool_id.clone(), ToolBlockState::default()); // collapsed=true

    let mut tab_render_state = TabRenderState::default();
    let collapsed_boundaries: RefCell<Vec<usize>> = RefCell::new(vec![]);
    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                false,
                &theme,
                &mut tab_render_state,
                &collapsed_states,
                &Default::default(),
            );
            *collapsed_boundaries.borrow_mut() = result.block_boundaries;
        })
        .unwrap();

    let collapsed_bounds = collapsed_boundaries.into_inner();
    // text 3 + collapsed tool 1 = 4
    assert_eq!(
        collapsed_bounds,
        vec![0, 4],
        "Collapsed: block_boundaries should be [0, 4], got {:?}",
        collapsed_bounds
    );
    let collapsed_cache_count = tab_render_state.height_cache.entries.len()
        + tab_render_state.height_cache.message_entries.len();
    assert!(
        collapsed_cache_count > 0,
        "collapsed render should populate height cache"
    );

    // --- Simulate toggle: invalidate cache, switch to expanded ---
    tab_render_state.height_cache.invalidate_all();
    let mut expanded_states = HashMap::new();
    expanded_states.insert(
        tool_id.clone(),
        ToolBlockState {
            collapsed: false,
            peek_active: false,
        },
    );

    let expanded_boundaries: RefCell<Vec<usize>> = RefCell::new(vec![]);
    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                false,
                &theme,
                &mut tab_render_state,
                &expanded_states,
                &Default::default(),
            );
            *expanded_boundaries.borrow_mut() = result.block_boundaries;
        })
        .unwrap();

    // expanded cache should differ from collapsed (version bumped by invalidate_all + re-fill)
    let expanded_cache_count = tab_render_state.height_cache.entries.len()
        + tab_render_state.height_cache.message_entries.len();
    assert!(
        expanded_cache_count > 0,
        "expanded render should populate height cache"
    );
    let expanded_bounds = expanded_boundaries.into_inner();
    // text 3 + expanded tool (3 + 3 output lines) = 3 + 6 = 9
    assert_eq!(
        expanded_bounds,
        vec![0, 9],
        "Expanded: block_boundaries should be [0, 9], got {:?}",
        expanded_bounds
    );
}

// ---------------------------------------------------------------------------
// Story 19.9 A3 — every tool block is expandable by keyboard
// ---------------------------------------------------------------------------
//
// The defect these keystones pin: `find_focused_tool_id` used to return the
// conversation's FIRST tool call id whenever any block boundary landed in the
// viewport's top three rows, and `event_loop.rs` writes that result back into
// `state.focused_tool_id` every frame — so a `Read` before a `Write` made the
// `Write` unreachable by `Enter`, and undid the `Tab` cycle. Focus now follows
// the visible block nearest the viewport top, and an explicit selection the
// caller already holds is kept while its block stays on screen.
//
// Front door: `chat_pane::render_with_search` — the same entry
// `event_loop.rs`'s composition root calls; the result travels to
// `state.focused_tool_id` and is what `Enter` (`app.rs`) acts on.

/// One assistant turn: `Read` → result → `Write` → result → prose, plus the
/// user message that precedes it. Ids are the canonical mirror ids, so the
/// assertions name exactly the strings `tool_block_states` and
/// `state.focused_tool_id` use.
fn read_then_write_turn() -> (Conversation, String, String) {
    use rustain::domain::models::turn::TurnPart;
    use rustain::domain::models::{InvocationStatus, StopReason, ToolOutput, Turn, WriteDiffState};

    let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
    turn.id = rustain::domain::models::TurnId("t19-9".to_string());
    let read_pid = turn.push_part(|id| TurnPart::ToolInvocation {
        id,
        tool: "Read".to_string(),
        args: serde_json::json!({"file_path": "src/main.rs"}),
        status: InvocationStatus::Success,
        started_at: 1_700_000_000_000,
        ended_at: Some(1_700_000_001_000),
    });
    turn.push_part(|id| TurnPart::ToolResult {
        id,
        refs: read_pid,
        output: ToolOutput {
            content: "1\tfn main() {}\n".to_string(),
            is_error: false,
            diff: WriteDiffState::NotAWrite,
        },
    });
    let write_pid = turn.push_part(|id| TurnPart::ToolInvocation {
        id,
        tool: "Write".to_string(),
        args: serde_json::json!({"file_path": "src/main.rs", "content": "fn main() {}\n"}),
        status: InvocationStatus::Success,
        started_at: 1_700_000_002_000,
        ended_at: Some(1_700_000_003_000),
    });
    turn.push_part(|id| TurnPart::ToolResult {
        id,
        refs: write_pid,
        output: ToolOutput {
            content: "Successfully wrote 13 bytes to src/main.rs".to_string(),
            is_error: false,
            diff: WriteDiffState::NotAWrite,
        },
    });
    turn.push_part(|id| TurnPart::Prose {
        id,
        text: "Done.".to_string(),
    });
    turn.stop_reason = Some(StopReason::EndTurn);

    let read_id = rustain::domain::models::turn::tool_call_id_for(&turn.id, read_pid);
    let write_id = rustain::domain::models::turn::tool_call_id_for(&turn.id, write_pid);

    let mut conversation = make_conversation(vec![
        bare_message("u19-9", MessageRole::User, "look at main.rs"),
        bare_message(&turn.id.0, MessageRole::Assistant, ""),
    ]);
    conversation.turns = vec![turn];
    (conversation, read_id, write_id)
}

fn bare_message(id: &str, role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: id.to_string(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: Default::default(),
        retracted_at_ms: None,
    }
}

/// Render through the production entry and return `(focused_tool_id,
/// block_boundaries, total_content_height)`.
#[allow(clippy::type_complexity)]
fn focus_probe(
    conversation: &Conversation,
    height: u16,
    scroll_offset: usize,
    auto_scroll: bool,
    current_focus: Option<&str>,
) -> (Option<String>, Vec<usize>, usize) {
    focus_probe_with_view(
        conversation,
        height,
        scroll_offset,
        auto_scroll,
        current_focus,
        &rustain::domain::models::ViewState::default(),
    )
}

#[allow(clippy::type_complexity)]
fn focus_probe_with_view(
    conversation: &Conversation,
    height: u16,
    scroll_offset: usize,
    auto_scroll: bool,
    current_focus: Option<&str>,
    view_state: &rustain::domain::models::ViewState,
) -> (Option<String>, Vec<usize>, usize) {
    let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
    let out: RefCell<(Option<String>, Vec<usize>, usize)> = RefCell::new((None, vec![], 0));
    let mut tab_render_state = TabRenderState::default();
    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render_with_search(
                frame,
                area,
                conversation,
                None,
                &StreamingState::default(),
                view_state,
                &rustain::domain::clock::SystemClock::default(),
                scroll_offset,
                auto_scroll,
                &Theme::dark(),
                &mut tab_render_state,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                None,
                None, // liveness
                None, // open_prose
                current_focus,
            );
            *out.borrow_mut() = (
                result.focused_tool_id,
                result.block_boundaries,
                result.total_content_height,
            );
        })
        .unwrap();
    out.into_inner()
}

/// A3(3)(a): a NON-FIRST tool block scrolled to the viewport top takes focus.
///
/// Mutant (executed RED at Task 1): restore `find_focused_tool_id`'s
/// `all_tool_ids.into_iter().next()` return — this asserts `read_id` instead.
// Covers: FR29 (collapsible tool blocks, plural)
#[test]
fn focus_follows_the_non_first_tool_block_at_the_viewport_top() {
    let (conversation, read_id, write_id) = read_then_write_turn();

    // Learn the layout from the render itself: for the turn path,
    // block_boundaries is [user_start, assistant_start, read_start,
    // write_start, prose_start] — index 3 is the Write block's start.
    let (_, bounds, total) = focus_probe(&conversation, 60, 0, true, None);
    assert!(
        bounds.len() >= 5,
        "expected user + assistant + 3 in-turn boundaries, got {:?}",
        bounds
    );
    let write_start = bounds[3];
    assert!(write_start > 0 && write_start < total);

    // A viewport of exactly (total - write_start) rows with no scroll offset
    // puts visible_start on write_start: the Write's start is the top row and
    // the Read is above the window.
    let height = u16::try_from(total - write_start).expect("small fixture");
    let (focused, _, _) = focus_probe(&conversation, height, 0, false, None);
    assert_eq!(
        focused.as_deref(),
        Some(write_id.as_str()),
        "the block at the viewport top must take focus, not the conversation's first tool call ({})",
        read_id
    );
}

/// A3(3)(b): an explicit `Tab` selection survives the next frame's recompute
/// while its block is still visible.
///
/// Mutant (executed RED at Task 1): drop the `current_focus` keep-branch —
/// focus falls back to the nearest visible block, i.e. the Read.
// Covers: FR29
#[test]
fn focus_keeps_an_explicit_selection_while_its_block_is_visible() {
    let (conversation, read_id, write_id) = read_then_write_turn();
    let (_, _, total) = focus_probe(&conversation, 60, 0, true, None);
    let height = u16::try_from(total).expect("small fixture");

    // Whole conversation visible: the Read is the block nearest the top, so
    // without a held focus that is what a recompute picks.
    let (default_focus, _, _) = focus_probe(&conversation, height, 0, false, None);
    assert_eq!(
        default_focus.as_deref(),
        Some(read_id.as_str()),
        "positive control: with no held focus the top-most visible block is focused"
    );

    let (kept, _, _) = focus_probe(&conversation, height, 0, false, Some(&write_id));
    assert_eq!(
        kept.as_deref(),
        Some(write_id.as_str()),
        "a held focus must survive while its block is on screen — this is what lets Tab reach a block Enter can then open"
    );
}

/// A3(3)(b, second half): a held focus whose block has scrolled OUT of the
/// viewport is released, and the visible block nearest the top takes over.
// Covers: FR29
#[test]
fn focus_is_released_when_its_block_scrolls_out_of_view() {
    let (conversation, read_id, write_id) = read_then_write_turn();
    let (_, bounds, total) = focus_probe(&conversation, 60, 0, true, None);
    let write_start = bounds[3];
    let height = u16::try_from(total - write_start).expect("small fixture");

    // visible_start == write_start, so the Read's start is above the window.
    let (focused, _, _) = focus_probe(&conversation, height, 0, false, Some(&read_id));
    assert_eq!(
        focused.as_deref(),
        Some(write_id.as_str()),
        "a held focus that left the viewport must not pin focus off-screen"
    );
}

/// A3(3)(c) positive control: no tool blocks ⇒ no focus, so `Enter` is a no-op
/// rather than acting on an unrelated block.
// Covers: FR29
#[test]
fn focus_is_none_when_the_conversation_has_no_tool_blocks() {
    let conversation = make_conversation(vec![
        bare_message("u1", MessageRole::User, "hello"),
        bare_message("a1", MessageRole::Assistant, "hi"),
    ]);
    let (focused, _, _) = focus_probe(&conversation, 20, 0, true, None);
    assert_eq!(focused, None, "no tool block ⇒ no focus");
    let (focused_held, _, _) = focus_probe(&conversation, 20, 0, true, Some("tc_ghost_0"));
    assert_eq!(
        focused_held, None,
        "a focus naming no existing block must not be kept alive by the keep-branch"
    );
}

/// The legacy no-turn path (a `ChatMessage` carrying `tool_calls` with no
/// matching `Turn`) must contribute its blocks to the focus list too — attach
/// mode and restored pre-turn conversations render through it.
///
/// Mutant (executed RED at Task 1): delete the `tool_block_boundaries.push`
/// in the `MessageRole::User | MessageRole::System` arm — focus can then only
/// ever be `None` for this fixture.
// Covers: FR29
#[test]
fn focus_reaches_a_non_first_block_on_the_legacy_no_turn_path() {
    use rustain::domain::models::WriteDiffState;

    let tc = |id: &str, name: &str, path: &str| ToolCallInfo {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({"file_path": path}),
        result: Some(ToolResultInfo {
            content: "out1\nout2\nout3".to_string(),
            is_error: false,
            diff: WriteDiffState::NotAWrite,
        }),
        started_at_ms: Some(0),
        completed_at_ms: Some(1000),
        status: None,
    };
    let mut msg = bare_message("legacy-1", MessageRole::User, "two tools, no turn");
    msg.tool_calls = vec![
        tc("tc-legacy-read", "Read", "src/main.rs"),
        tc("tc-legacy-write", "Write", "src/main.rs"),
    ];
    let conversation = make_conversation(vec![msg]);

    // block_boundaries on this path records block ENDS; the focus list records
    // STARTS. Derive the second block's start from the first block's end.
    let (_, bounds, total) = focus_probe(&conversation, 60, 0, true, None);
    assert_eq!(
        bounds.len(),
        3,
        "message start + two tool-block ends, got {:?}",
        bounds
    );
    let write_start = bounds[1];
    let height = u16::try_from(total - write_start).expect("small fixture");

    let (focused, _, _) = focus_probe(&conversation, height, 0, false, None);
    assert_eq!(
        focused.as_deref(),
        Some("tc-legacy-write"),
        "the second legacy block, scrolled to the viewport top, must take focus"
    );

    // Positive control: the whole message visible ⇒ the FIRST block is focused,
    // so the assertion above is about position, not about a constant.
    let full = u16::try_from(total).expect("small fixture");
    let (first, _, _) = focus_probe(&conversation, full, 0, false, None);
    assert_eq!(first.as_deref(), Some("tc-legacy-read"));
}

/// A3 rule 2: moving to a turn (`]]` / `[[`, i.e. `view_state.focused_turn`)
/// re-seats focus into THAT turn's first visible tool block — even when an
/// earlier turn's block is nearer the viewport top and even when the target
/// block can never be scrolled to the top because it lies in the
/// conversation's last viewport-height of lines.
///
/// Mutant (executed RED at Task 1): delete the `focused_turn_prefix` arm —
/// focus falls back to nearest-visible, i.e. the earlier turn's block, and the
/// single-invocation tail turn becomes unreachable by keyboard again.
// Covers: FR29
#[test]
fn focus_follows_the_focused_turn_even_when_an_earlier_block_is_nearer_the_top() {
    use rustain::domain::models::turn::TurnPart;
    use rustain::domain::models::view_state::ViewState;
    use rustain::domain::models::{
        InvocationStatus, StopReason, ToolOutput, Turn, TurnId, WriteDiffState,
    };

    fn tool_turn(id: &str, tool: &str, prose: &str) -> (Turn, String) {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        turn.id = TurnId(id.to_string());
        let pid = turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: tool.to_string(),
            args: serde_json::json!({"file_path": "src/auth/parse.rs"}),
            status: InvocationStatus::Success,
            started_at: 1_700_000_000_000,
            ended_at: Some(1_700_000_001_000),
        });
        turn.push_part(|id| TurnPart::ToolResult {
            id,
            refs: pid,
            output: ToolOutput {
                content: "ok".to_string(),
                is_error: false,
                diff: WriteDiffState::NotAWrite,
            },
        });
        turn.push_part(|id| TurnPart::Prose {
            id,
            text: prose.to_string(),
        });
        turn.stop_reason = Some(StopReason::EndTurn);
        let tc_id = rustain::domain::models::turn::tool_call_id_for(&turn.id, pid);
        (turn, tc_id)
    }

    // Two single-invocation turns: `Bash` then `Write`. Neither turn can be
    // Tab-cycled (one invocation each), which is the shape J2's capture has.
    let (bash_turn, bash_id) = tool_turn("t-bash", "Bash", "No advisories apply offline.");
    let (write_turn, write_id) = tool_turn("t-write", "Write", "Fix applied.");
    let mut conversation = make_conversation(vec![
        bare_message("u-1", MessageRole::User, "check the advisory database"),
        bare_message("t-bash", MessageRole::Assistant, ""),
        bare_message("u-2", MessageRole::User, "propose a fix"),
        bare_message("t-write", MessageRole::Assistant, ""),
    ]);
    conversation.turns = vec![bash_turn, write_turn];

    let (_, _, total) = focus_probe(&conversation, 60, 0, true, None);
    let height = u16::try_from(total).expect("small fixture");

    // Positive control: with no focused turn, nearest-visible picks the Bash.
    let (nearest, _, _) = focus_probe(&conversation, height, 0, false, None);
    assert_eq!(
        nearest.as_deref(),
        Some(bash_id.as_str()),
        "positive control: nearest-to-top is the earlier block"
    );

    let mut view_state = ViewState::default();
    view_state.set_focused_turn(Some(TurnId("t-write".to_string())));
    let (in_turn, _, _) = focus_probe_with_view(
        &conversation,
        height,
        0,
        false,
        Some(&bash_id), // the focus `]]` is moving away FROM
        &view_state,
    );
    assert_eq!(
        in_turn.as_deref(),
        Some(write_id.as_str()),
        "moving to a turn must re-seat focus into that turn, not keep the earlier block"
    );
}
