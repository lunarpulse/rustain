use std::cell::RefCell;
use std::collections::HashMap;

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::HeightCache;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::chat_pane::RenderResult;
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
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
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
                &mut HeightCache::default(),
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
        role: MessageRole::User,
        content: "Hello world".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
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
                &mut HeightCache::default(),
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
        role: MessageRole::Assistant,
        content: "Hi there".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
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
                &mut HeightCache::default(),
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
                &mut HeightCache::default(),
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
                &mut HeightCache::default(),
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
        role: MessageRole::User,
        content: "My question".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
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
                &mut HeightCache::default(),
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
        role: MessageRole::Assistant,
        content: "Something went wrong".to_string(),
        content_blocks: vec![ContentBlockType::Error],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
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
                &mut HeightCache::default(),
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
                &mut HeightCache::default(),
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
            role: MessageRole::User,
            content: format!("User message {}", i),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
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
                &mut HeightCache::default(),
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
        }),
        started_at_ms: Some(0),
        completed_at_ms: Some(1000),
    };
    let conversation = make_conversation(vec![ChatMessage {
        role: MessageRole::User,
        content: "Hello".to_string(),
        content_blocks: vec![],
        tool_calls: vec![tc],
        created_at: 0,
        token_count: None,
        stop_reason: None,
    }]);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // --- First render: collapsed (default) ---
    let mut collapsed_states = HashMap::new();
    collapsed_states.insert(tool_id.clone(), ToolBlockState::default()); // collapsed=true

    let mut height_cache = HeightCache::default();
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
                &mut height_cache,
                &collapsed_states,
                &Default::default(),
            );
            *collapsed_boundaries.borrow_mut() = result.block_boundaries;
        })
        .unwrap();

    let collapsed_cache = height_cache.get(0);
    let collapsed_bounds = collapsed_boundaries.into_inner();
    // text 3 + collapsed tool 1 = 4
    assert_eq!(
        collapsed_cache,
        Some(4),
        "Collapsed: cache should be 4, got {:?}",
        collapsed_cache
    );
    assert_eq!(
        collapsed_bounds,
        vec![0, 4],
        "Collapsed: block_boundaries should be [0, 4], got {:?}",
        collapsed_bounds
    );

    // --- Simulate toggle: invalidate cache, switch to expanded ---
    height_cache.invalidate_all();
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
                &mut height_cache,
                &expanded_states,
                &Default::default(),
            );
            *expanded_boundaries.borrow_mut() = result.block_boundaries;
        })
        .unwrap();

    let expanded_cache = height_cache.get(0);
    let expanded_bounds = expanded_boundaries.into_inner();
    // text 3 + expanded tool (3 + 3 output lines) = 3 + 6 = 9
    assert_eq!(
        expanded_cache,
        Some(9),
        "Expanded: cache should be 9, got {:?}",
        expanded_cache
    );
    assert_eq!(
        expanded_bounds,
        vec![0, 9],
        "Expanded: block_boundaries should be [0, 9], got {:?}",
        expanded_bounds
    );
}
