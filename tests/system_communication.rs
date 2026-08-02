//! Integration tests for Story 1-7: System Communication Widgets.
//!
//! Tests: FeedbackBlock rendering, flash message expiry, token usage formatting,
//! StatusState transitions, and AskUserQuestion card.

use std::collections::{BTreeMap, HashMap};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::ask_user_question::{
    AskUserQuestionState, render_ask_user_lines,
};
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::adapters::tui::widgets::{chat_pane, feedback_block, status_bar};
use rustain::domain::models::{
    ChatMessage, Conversation, FeedbackAction, FeedbackBlock, FeedbackLevel, MessageRole,
    StatusState, StreamingState, UsageInfo, next_delay,
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

/// AC1: Error FeedbackBlock renders with bold red border, error symbol, retry action.
// Covers: FR14 (retry/backoff), UX-DR81 (feedback blocks)
#[test]
fn test_error_feedback_block_in_conversation() {
    let theme = Theme::dark();
    let mut feedback_blocks = BTreeMap::new();
    let fb = FeedbackBlock {
        id: "fb-1".to_string(),
        level: FeedbackLevel::Error,
        message: "Couldn't reach Anthropic API".to_string(),
        actions: vec![FeedbackAction::Retry],
    };
    feedback_blocks.insert("fb-1".to_string(), fb);

    let conversation = make_conversation(vec![ChatMessage {
        synthetic: false,
        id: rustain::domain::models::generate_conversation_id(),
        role: MessageRole::User,
        content: "Hello".to_string(),
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

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

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

    // Verify feedback block renders by checking buffer content
    let buffer = terminal.backend().buffer().clone();
    let mut content = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            content.push_str(buffer.cell((x, y)).unwrap().symbol());
        }
    }

    assert!(content.contains('✗'), "Error symbol should be present");
    assert!(
        content.contains("[Ctrl+K r]"),
        "Retry action should be present"
    );
}

/// AC2: Warning FeedbackBlock max 3 lines.
// Covers: UX-DR81 (feedback blocks)
#[test]
fn test_warning_feedback_max_lines() {
    let theme = Theme::dark();
    let fb = FeedbackBlock {
        id: "w-1".to_string(),
        level: FeedbackLevel::Warning,
        message: "Running low on context. Consider compacting or starting a fresh conversation to avoid losing quality.".to_string(),
        actions: vec![FeedbackAction::Compact, FeedbackAction::StartFresh],
    };
    let lines = feedback_block::render_feedback_lines(&fb, 50, &theme);
    assert!(
        lines.len() <= 3,
        "Warning should be max 3 lines, got {}",
        lines.len()
    );
}

/// AC4: AskUserQuestion card with double border.
// Covers: FR32 (ask user question)
#[test]
fn test_ask_user_question_card_double_border() {
    let theme = Theme::dark();
    let state = AskUserQuestionState {
        tool_use_id: "tu-1".to_string(),
        question: "What is your project name?".to_string(),
        input_buffer: "MyProject".to_string(),
        cursor_position: 9,
        submitted_answer: None,
    };
    let lines = render_ask_user_lines(&state, 60, &theme);

    let first: String = lines[0]
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(first.contains('╔'), "Should have double border top");
    assert!(first.contains('╗'), "Should have double border top-right");

    let last: String = lines
        .last()
        .unwrap()
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(last.contains('╚'), "Should have double border bottom");
    assert!(last.contains('╝'), "Should have double border bottom-right");
}

/// AC5: Flash message appears in status bar and auto-dismisses.
// Covers: FR38 (status bar)
#[test]
fn test_flash_message_expiry_simulation() {
    let tick_ms = 250u64;
    let mut status = StatusState::Flash {
        message: "Config parse error".to_string(),
        remaining_ms: 1000,
    };

    // Simulate 4 ticks (1000ms total)
    for _ in 0..3 {
        if let StatusState::Flash { remaining_ms, .. } = &mut status {
            assert!(*remaining_ms > tick_ms);
            *remaining_ms -= tick_ms;
        }
    }
    // 4th tick should trigger expiry
    if let StatusState::Flash { remaining_ms, .. } = &status {
        assert!(*remaining_ms <= tick_ms, "Should be ready to expire");
        status = StatusState::Idle;
    }
    assert_eq!(status, StatusState::Idle, "Flash should revert to Idle");
}

/// AC7: Token usage display formatting.
// Covers: FR38 (status bar)
#[test]
fn test_token_usage_display_formatting() {
    let usage = UsageInfo {
        input_tokens: 1200,
        output_tokens: 3400,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: None,
    };
    assert_eq!(status_bar::format_token_usage(&usage), "↑1.2k ↓3.4k");

    let small_usage = UsageInfo {
        input_tokens: 50,
        output_tokens: 100,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: None,
    };
    assert_eq!(status_bar::format_token_usage(&small_usage), "↑50 ↓100");
}

/// AC1: Exponential backoff: 1s -> 2s -> 4s -> 8s -> 16s.
// Covers: FR14 (retry/backoff)
#[test]
fn test_exponential_backoff_sequence() {
    let expected = [1000, 2000, 4000, 8000, 16000];
    for (attempt, &expected_delay) in expected.iter().enumerate() {
        assert_eq!(
            next_delay(attempt as u8),
            expected_delay,
            "Attempt {} should have {}ms delay",
            attempt,
            expected_delay
        );
    }
    // Capped at 16s
    assert_eq!(next_delay(5), 16000);
    assert_eq!(next_delay(10), 16000);
}

/// AC6: StatusState transitions cover all states.
// Covers: FR38 (status bar)
#[test]
fn test_status_state_complete_lifecycle() {
    let mut status = StatusState::Idle;
    assert_eq!(status.display_text(), "Ready");
    assert!(!status.is_active());

    status = StatusState::Streaming;
    assert_eq!(status.display_text(), "Streaming...");
    assert!(status.is_active());

    status = StatusState::Executing {
        tool_name: "bash".to_string(),
        elapsed_ms: 100,
    };
    assert!(status.display_text().contains("bash"));
    assert!(status.is_active());

    status = StatusState::Retrying {
        attempt: 2,
        max: 5,
        next_in_ms: 4000,
    };
    assert!(status.display_text().contains("2/5"));
    assert!(status.is_active());

    status = StatusState::Flash {
        message: "Done".to_string(),
        remaining_ms: 1000,
    };
    assert_eq!(status.display_text(), "Done");
    assert!(!status.is_active());

    status = StatusState::Idle;
    assert_eq!(status.display_text(), "Ready");
}
