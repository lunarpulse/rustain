use std::collections::HashMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::HeightCache;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::domain::events::ChunkAction;
use rustain::domain::models::{
    ChatMessage, Conversation, MessageRole, StopReason, StreamChunk, StreamingPhase,
    StreamingState, apply_chunk, generate_conversation_id,
};

fn make_conversation() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    }
}

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect()
}

/// AC12: E2E integration test validates full message→response flow.
/// Simulates: launch → render empty state → send message → receive streaming
/// response → verify response appears in chat pane.
/// Uses ratatui TestBackend (no real terminal) and domain functions (no real API).
#[test]
fn test_e2e_message_to_streaming_response() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    let mut conversation = make_conversation();
    let mut streaming = StreamingState::default();

    // Step 1: Render empty state
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
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text = buffer_text(&terminal);
    assert!(
        text.contains("Welcome to Rustain."),
        "Step 1: Expected welcome message"
    );

    // Step 2: User sends a message
    conversation.messages.push(ChatMessage {
        role: MessageRole::User,
        content: "What is Rust?".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1000,
        token_count: None,
        stop_reason: None,
    });

    // Start streaming (typing indicator phase)
    streaming.is_streaming = true;
    streaming.phase = StreamingPhase::AccumulatingText;

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
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text = buffer_text(&terminal);
    assert!(
        text.contains("You:"),
        "Step 2: Expected user message prefix"
    );
    assert!(
        text.contains("What is Rust?"),
        "Step 2: Expected user message content"
    );
    assert!(text.contains("···"), "Step 2: Expected typing indicator");

    // Step 3: Process streaming chunks via apply_chunk
    let chunk1 = StreamChunk::Text {
        content: "Rust is a ".to_string(),
        parent_tool_use_id: None,
    };
    let action1 = apply_chunk(&mut conversation, &mut streaming, chunk1, 1001);
    assert_eq!(action1, ChunkAction::NeedsRedraw);
    assert_eq!(streaming.current_text_buffer, "Rust is a ");

    let chunk2 = StreamChunk::Text {
        content: "systems programming language.".to_string(),
        parent_tool_use_id: None,
    };
    let action2 = apply_chunk(&mut conversation, &mut streaming, chunk2, 1002);
    assert_eq!(action2, ChunkAction::NeedsRedraw);
    assert_eq!(
        streaming.current_text_buffer,
        "Rust is a systems programming language."
    );

    // Render mid-stream
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
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text = buffer_text(&terminal);
    assert!(
        text.contains("Rust is a systems programming language."),
        "Step 3: Expected streaming content"
    );

    // Step 4: Turn complete
    let chunk3 = StreamChunk::TurnComplete {
        stop_reason: StopReason::EndTurn,
    };
    let action3 = apply_chunk(&mut conversation, &mut streaming, chunk3, 1003);
    assert!(matches!(action3, ChunkAction::TurnComplete { .. }));
    assert!(!streaming.is_streaming);

    // Verify conversation has 2 messages (user + assistant)
    assert_eq!(
        conversation.messages.len(),
        2,
        "Step 4: Expected 2 messages (user + assistant)"
    );
    assert_eq!(conversation.messages[0].role, MessageRole::User);
    assert_eq!(conversation.messages[1].role, MessageRole::Assistant);
    assert_eq!(
        conversation.messages[1].content,
        "Rust is a systems programming language."
    );

    // Step 5: Final render shows complete conversation
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
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
            );
        })
        .unwrap();

    let text = buffer_text(&terminal);
    assert!(text.contains("You:"), "Step 5: User message visible");
    assert!(
        text.contains("What is Rust?"),
        "Step 5: User question visible"
    );
    assert!(
        text.contains("Assistant:"),
        "Step 5: Assistant prefix visible"
    );
    assert!(
        text.contains("Rust is a systems programming language."),
        "Step 5: Assistant response visible"
    );
    // No typing indicator after turn complete
    assert!(
        !text.contains("···"),
        "Step 5: No typing indicator after completion"
    );
}
