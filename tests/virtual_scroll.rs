use std::collections::HashMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::HeightCache;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::domain::models::{ChatMessage, Conversation, MessageRole, StreamingState};

fn make_conversation(msg_count: usize) -> Conversation {
    let messages: Vec<ChatMessage> = (0..msg_count)
        .map(|i| ChatMessage {
            role: if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            content: format!("Message number {} with some content to fill a line.", i),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: i as i64,
            token_count: None,
            stop_reason: None,
        })
        .collect();

    Conversation {
        id: "bench".to_string(),
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

/// AC8: Virtual scrolling with viewport culling renders only visible messages.
/// Local benchmark: 1000+ messages, assert render < 16ms.
/// Ignored on CI — use relative scaling test instead.
#[test]
#[ignore]
fn test_virtual_scroll_1000_messages_performance() {
    let conversation = make_conversation(1000);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    let start = std::time::Instant::now();
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
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();
    let elapsed = start.elapsed();

    // Local benchmark: should render in < 16ms
    assert!(
        elapsed.as_millis() < 16,
        "1000-message render took {:?} (should be < 16ms)",
        elapsed
    );
}

/// AC8: CI-safe relative benchmark — 1000 msgs in <= 3x the time of 100 msgs.
#[test]
fn test_virtual_scroll_relative_scaling() {
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // Benchmark 100 messages
    let conv_100 = make_conversation(100);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    let start_100 = std::time::Instant::now();
    for _ in 0..10 {
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conv_100,
                    &streaming,
                    0,
                    true,
                    &theme,
                    &mut cache,
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }
    let time_100 = start_100.elapsed();

    // Benchmark 1000 messages
    let conv_1000 = make_conversation(1000);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    let start_1000 = std::time::Instant::now();
    for _ in 0..10 {
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conv_1000,
                    &streaming,
                    0,
                    true,
                    &theme,
                    &mut cache,
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }
    let time_1000 = start_1000.elapsed();

    // 1000 messages should be ≤ 3x the time of 100 messages
    let ratio = time_1000.as_nanos() as f64 / time_100.as_nanos().max(1) as f64;
    assert!(
        ratio <= 3.0,
        "1000-msg render ({:?}) should be ≤ 3x of 100-msg render ({:?}), ratio: {:.2}",
        time_1000,
        time_100,
        ratio,
    );
}

/// Edge case: zero messages.
#[test]
fn test_virtual_scroll_zero_messages() {
    let conversation = make_conversation(0);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            assert_eq!(result.total_content_height, 0);
        })
        .unwrap();
}

/// Edge case: one message.
#[test]
fn test_virtual_scroll_one_message() {
    let conversation = make_conversation(1);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            assert!(result.total_content_height > 0);
            assert_eq!(result.block_boundaries.len(), 1);
            // Single user message
            assert_eq!(result.message_boundaries.len(), 1);
        })
        .unwrap();
}

// Covers: FR13 (auto-scroll), NFR4 (1000+ message performance), AC7 — viewport culling
#[test]
fn test_virtual_scroll_viewport_culling() {
    let conversation = make_conversation(60);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // First render at bottom (scroll_offset = 0, auto_scroll = true) to get total height
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();
    let mut total_height = 0;

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
            total_height = result.total_content_height;
        })
        .unwrap();

    assert!(total_height > 24, "60 messages should exceed viewport height");

    // Render scrolled to middle (offset = half of max scroll range)
    let max_offset = total_height.saturating_sub(24);
    let mid_offset = max_offset / 2;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                mid_offset,
                false, // NOT auto-scroll — use explicit offset
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text: String = terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // First message (index 0) should NOT be visible when scrolled to middle
    assert!(
        !text.contains("Message number 0 "),
        "First message should not be visible when scrolled to middle"
    );
    // Last message (index 59) should NOT be visible when scrolled to middle
    assert!(
        !text.contains("Message number 59 "),
        "Last message should not be visible when scrolled to middle"
    );
    // Some middle message should be visible
    let has_middle = (20..40).any(|i| text.contains(&format!("Message number {}", i)));
    assert!(has_middle, "Some middle messages should be visible");
}

// Covers: FR13 (auto-scroll), AC7 — jump to bottom shows last messages
#[test]
fn test_virtual_scroll_jump_to_bottom() {
    let conversation = make_conversation(60);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    // Render at bottom (scroll_offset = 0, auto_scroll = true)
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true, // auto_scroll = jump to bottom
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text: String = terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // Last message should be visible
    assert!(
        text.contains("Message number 59"),
        "Last message should be visible at bottom"
    );
    // First message should NOT be visible
    assert!(
        !text.contains("Message number 0 "),
        "First message should not be visible at bottom"
    );
}

// Covers: AC7 — messages above viewport not present in buffer
#[test]
fn test_virtual_scroll_above_viewport_not_rendered() {
    let conversation = make_conversation(60);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // Scroll all the way to the top (max offset)
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();
    let mut total_height = 0;

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
            total_height = result.total_content_height;
        })
        .unwrap();

    let max_offset = total_height.saturating_sub(24);

    // Now render at the top
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                max_offset,
                false,
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text: String = terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // First message should be visible at top
    assert!(
        text.contains("Message number 0"),
        "First message should be visible when scrolled to top"
    );
    // Last message should NOT be visible
    assert!(
        !text.contains("Message number 59"),
        "Last message should not be visible when scrolled to top"
    );
}

/// Edge case: only user messages (no assistant).
#[test]
fn test_virtual_scroll_only_user_messages() {
    let messages: Vec<ChatMessage> = (0..5)
        .map(|i| ChatMessage {
            role: MessageRole::User,
            content: format!("User message {}", i),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: i as i64,
            token_count: None,
            stop_reason: None,
        })
        .collect();
    let conversation = Conversation {
        id: "test".to_string(),
        title: String::new(),
        messages,
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    };
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut cache,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            assert_eq!(result.message_boundaries.len(), 5);
            assert_eq!(result.block_boundaries.len(), 5);
        })
        .unwrap();
}
