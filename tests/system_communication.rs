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
    ChatMessage, Conversation, FeedbackBlock, FeedbackLevel, MessageRole, StatusState,
    StreamingState, UsageInfo, next_delay,
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
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
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
        seq: 0,
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
        authorship: rustain::domain::models::Authorship::HumanWritten,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
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
    // Old: `[Ctrl+K r]` Retry chip. New (21.1 S2 AC12): a block advertises no key.
    assert!(!content.contains("Ctrl+K"), "no action chip is painted");
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
        seq: 0,
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

/// AC5 (Story 21.1 AC1): a notice is visible until its store deadline and is
/// swept by the first draw at or after it; nothing counts ticks down.
// Covers: FR38 (status bar)
#[test]
fn test_flash_message_expiry_simulation() {
    use rustain::adapters::tui::notifications::{
        NoticeDraft, NoticeSeverity, NoticeSource, NotificationStore, Presentation,
    };
    use rustain::domain::clock::MockClock;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let ttls = rustain::domain::models::NotificationsConfig::default().clamped();
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut store = NotificationStore::new(clock.clone(), ttls);
    store.notify(NoticeDraft::new(
        NoticeSource::Config,
        NoticeSeverity::Error,
        "Config parse error",
    ));
    let ttl = Duration::from_secs(ttls.error);

    // One millisecond before the deadline the toast is still there.
    clock.advance(ttl - Duration::from_millis(1));
    assert!(!store.sweep(), "not expired before its deadline");
    assert_eq!(store.visible().count(), 1);

    // At the deadline the next sweep expires it exactly once; the record stays
    // in the log.
    clock.advance(Duration::from_millis(1));
    assert!(store.sweep(), "expired at its deadline");
    assert_eq!(store.visible().count(), 0);
    assert!(!store.sweep(), "each toast expires exactly once");
    let record = store.records().next().expect("the record is retained");
    assert_eq!(record.message, "Config parse error");
    assert_eq!(record.presentation, Presentation::Expired);
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

    status = StatusState::Idle;
    assert_eq!(status.display_text(), "Ready");
}
