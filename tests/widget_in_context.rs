// Covers: AC3 (widget-in-context integration tests)
//! Integration tests verifying each widget type renders correctly within the
//! full layout (compute_layout + chat pane + status bar + input box).

use std::collections::{BTreeMap, HashMap};

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;
use ratatui::widgets::{Clear, Paragraph};

use rustain::adapters::tui::layout;
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::adapters::tui::widgets::{chat_pane, input_box, permission_prompt, status_bar};
use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{
    ChatMessage, Conversation, FeedbackBlock, FeedbackLevel, FocusState, MessageRole,
    PermissionMode, StatusState, StopReason, StreamingState, ToolCallInfo,
};

fn make_conversation(messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: "test-ctx".to_string(),
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
    }
}

/// Render full layout: chat pane + status bar + input box.
fn render_full_layout(
    conversation: &Conversation,
    streaming: &StreamingState,
    tool_block_states: &HashMap<String, ToolBlockState>,
    feedback_blocks: &BTreeMap<String, FeedbackBlock>,
) -> Terminal<TestBackend> {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let app_layout = layout::compute_layout(area, &theme, "", 1, false)
                .expect("layout must compute for 80x24");
            chat_pane::render(
                frame,
                app_layout.chat_pane,
                conversation,
                streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                tool_block_states,
                feedback_blocks,
            );
            status_bar::render(
                frame,
                app_layout.status_bar,
                "test-model",
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                app_layout.chat_pane.height,
                PermissionMode::Normal,
                None,
                false,
                None,
                false, // multiline_mode
                None,  // current_hint
                0,
                None,
                None,
                None,
                false,
            );
            input_box::render(
                frame,
                app_layout.input_area,
                "",
                0,
                FocusState::Input,
                &theme,
                false,
                0,
                None,
            );
        })
        .unwrap();

    terminal
}

// Covers: FR29 (collapsible tool blocks), AC3 — tool_block renders within full layout
#[test]
fn test_tool_block_in_full_layout() {
    let conversation = make_conversation(vec![
        ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: "Read my file".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        },
        ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::Assistant,
            content: "Reading file.".to_string(),
            content_blocks: vec![],
            tool_calls: vec![ToolCallInfo {
                id: "toolu_test1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"file_path": "src/main.rs"
                }),
                result: Some(rustain::domain::models::ToolResultInfo {
                    content: "fn main() {}".to_string(),
                    is_error: false,
                }),
                started_at_ms: Some(0),
                completed_at_ms: Some(100),
                status: None,
            }],
            created_at: 0,
            token_count: None,
            stop_reason: Some(StopReason::ToolUse),
            images: vec![],
        },
    ]);

    let mut tool_states = HashMap::new();
    tool_states.insert(
        "toolu_test1".to_string(),
        ToolBlockState {
            collapsed: true,
            peek_active: false,
        },
    );

    let terminal = render_full_layout(
        &conversation,
        &StreamingState::default(),
        &tool_states,
        &BTreeMap::new(),
    );
    let text = common::buffer_text(&terminal);

    // Tool block should show tool name in the composed frame
    assert!(
        text.contains("Read"),
        "Tool block should show tool name 'Read' in full layout, got:\n{}",
        text
    );
    assert!(
        text.contains("src/main.rs"),
        "Tool block should show file path in full layout"
    );
}

// Covers: AC3 — feedback_block renders within full layout
#[test]
fn test_feedback_block_in_full_layout() {
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
    }]);

    let mut feedback_blocks = BTreeMap::new();
    feedback_blocks.insert(
        "fb-1".to_string(),
        FeedbackBlock {
            id: "fb-1".to_string(),
            level: FeedbackLevel::Error,
            message: "Connection lost".to_string(),
            actions: vec![],
        },
    );

    let terminal = render_full_layout(
        &conversation,
        &StreamingState::default(),
        &HashMap::new(),
        &feedback_blocks,
    );
    let text = common::buffer_text(&terminal);

    assert!(
        text.contains("Connection lost"),
        "Feedback block error message should be visible in full layout"
    );
}

// Covers: FR24 (permission prompt), AC3 — permission_prompt renders within full layout
#[test]
fn test_permission_prompt_in_full_layout() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let conversation = make_conversation(vec![]);
    let streaming = StreamingState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let app_layout = layout::compute_layout(area, &theme, "", 1, false)
                .expect("layout must compute for 80x24");
            chat_pane::render(
                frame,
                app_layout.chat_pane,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &BTreeMap::<String, FeedbackBlock>::new(),
            );

            let prompt_lines = permission_prompt::render_permission_lines(
                &ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                "Bash",
                "rm -rf /tmp/test",
                &theme,
                0,
            );
            let prompt_height = prompt_lines.len() as u16;
            let prompt_area = Rect {
                x: app_layout.chat_pane.x,
                y: app_layout.chat_pane.y + app_layout.chat_pane.height
                    - prompt_height.min(app_layout.chat_pane.height),
                width: app_layout.chat_pane.width,
                height: prompt_height.min(app_layout.chat_pane.height),
            };
            frame.render_widget(Clear, prompt_area);
            frame.render_widget(Paragraph::new(prompt_lines), prompt_area);

            status_bar::render(
                frame,
                app_layout.status_bar,
                "test-model",
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                app_layout.chat_pane.height,
                PermissionMode::Normal,
                None,
                false,
                None,
                false, // multiline_mode
                None,  // current_hint
                0,
                None,
                None,
                None,
                false,
            );
            input_box::render(
                frame,
                app_layout.input_area,
                "",
                0,
                FocusState::Input,
                &theme,
                false,
                0,
                None,
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Bash"),
        "Permission prompt should show tool name 'Bash' in full layout"
    );
    assert!(
        text.contains("rm -rf"),
        "Permission prompt should show command in full layout"
    );
}

// Covers: FR32 (ask user question), AC3 — ask_user_question renders within full layout
#[test]
fn test_ask_user_question_in_full_layout() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let conversation = make_conversation(vec![]);
    let streaming = StreamingState::default();

    let aq_state = AskUserQuestionState {
        tool_use_id: "tu-ctx-1".to_string(),
        question: "What is your project name?".to_string(),
        input_buffer: String::new(),
        cursor_position: 0,
        submitted_answer: None,
    };

    terminal
        .draw(|frame| {
            let area = frame.area();
            let app_layout = layout::compute_layout(area, &theme, "", 1, false)
                .expect("layout must compute for 80x24");
            chat_pane::render(
                frame,
                app_layout.chat_pane,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &HashMap::<String, ToolBlockState>::new(),
                &BTreeMap::<String, FeedbackBlock>::new(),
            );

            let aq_lines =
                rustain::adapters::tui::widgets::ask_user_question::render_ask_user_lines(
                    &aq_state,
                    app_layout.chat_pane.width,
                    &theme,
                );
            let aq_height = aq_lines.len() as u16;
            let aq_area = Rect {
                x: app_layout.chat_pane.x,
                y: app_layout.chat_pane.y + app_layout.chat_pane.height
                    - aq_height.min(app_layout.chat_pane.height),
                width: app_layout.chat_pane.width,
                height: aq_height.min(app_layout.chat_pane.height),
            };
            frame.render_widget(Clear, aq_area);
            frame.render_widget(Paragraph::new(aq_lines), aq_area);

            status_bar::render(
                frame,
                app_layout.status_bar,
                "test-model",
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                app_layout.chat_pane.height,
                PermissionMode::Normal,
                None,
                false,
                None,
                false, // multiline_mode
                None,  // current_hint
                0,
                None,
                None,
                None,
                false,
            );
            input_box::render(
                frame,
                app_layout.input_area,
                "",
                0,
                FocusState::Input,
                &theme,
                false,
                0,
                None,
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("project name"),
        "AskUserQuestion should show question text in full layout, got:\n{}",
        text
    );
}

// Covers: AC3 — empty_state renders within full layout
// (already covered by test_e2e_fresh_session_empty_state in e2e_harness.rs;
//  this test adds explicit traceability and full-layout verification)
#[test]
fn test_empty_state_in_full_layout() {
    let terminal = render_full_layout(
        &make_conversation(vec![]),
        &StreamingState::default(),
        &HashMap::new(),
        &BTreeMap::new(),
    );
    let text = common::buffer_text(&terminal);

    assert!(
        text.contains("Welcome to Rustain"),
        "Empty state should show welcome message in full layout"
    );
    assert!(
        text.contains("Type a message"),
        "Empty state should show prompt to type a message"
    );
    // Verify status bar and input box are also rendered
    assert!(text.contains("test-model"), "Status bar visible");
    assert!(text.contains("Message"), "Input box border visible");
}
