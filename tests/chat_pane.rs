use std::collections::HashMap;

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::HeightCache;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, MessageRole, StreamingPhase, StreamingState,
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
