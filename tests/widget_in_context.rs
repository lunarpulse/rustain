// Covers: AC3 (widget-in-context integration tests)
//! Integration tests verifying each widget type renders correctly within the
//! full layout (compute_layout + chat pane + status bar + input box).

use std::collections::BTreeMap;

use crate::common;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::decision_queue;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::{self, Shell};
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{chat_pane, input_box, status_bar};
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
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    }
}

/// Render full layout: chat pane + status bar + input box.
fn render_full_layout(
    conversation: &Conversation,
    streaming: &StreamingState,
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
                feedback_blocks,
            );
            status_bar::render(
                frame,
                app_layout.status_bar,
                "test-model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                app_layout.chat_pane.height,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false, // multiline_mode
                true,  // context_injection_on
                0,     // injected_tokens
                None,  // current_hint
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                false,
                None,
                rustain::adapters::tui::state::LogAwareness::Hidden,
                0,
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
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
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
                    diff: rustain::domain::models::WriteDiffState::NotAWrite,
                    images: vec![],
                }),
                started_at_ms: Some(0),
                completed_at_ms: Some(100),
                status: None,
                provider_id: None,
            }],
            created_at: 0,
            token_count: None,
            stop_reason: Some(StopReason::ToolUse),
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        },
    ]);

    let terminal = render_full_layout(&conversation, &StreamingState::default(), &BTreeMap::new());
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
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: rustain::domain::models::Authorship::HumanWritten,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
    }]);

    let mut feedback_blocks = BTreeMap::new();
    feedback_blocks.insert(
        "fb-1".to_string(),
        FeedbackBlock {
            id: "fb-1".to_string(),
            level: FeedbackLevel::Error,
            message: "Connection lost".to_string(),
            seq: 0,
        },
    );

    let terminal = render_full_layout(&conversation, &StreamingState::default(), &feedback_blocks);
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
    let mut state = rustain::adapters::tui::state::TuiState::new(80, 24);
    rustain::adapters::tui::decision_queue::raise_permission(
        &mut state,
        rustain::adapters::tui::decision_queue::PermissionPayload::new(
            rustain::domain::models::tool_call::RequestId::new(),
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            rustain::domain::models::ToolRisk::Elevated,
            rustain::adapters::tui::decision_queue::PermissionInput::Ready(std::sync::Arc::new(
                serde_json::json!({"command": "rm -rf /tmp/test"}),
            )),
        ),
        rustain::adapters::tui::decision_queue::Responder::Local,
    );

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
                &BTreeMap::<String, FeedbackBlock>::new(),
            );

            status_bar::render(
                frame,
                app_layout.status_bar,
                "test-model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                app_layout.chat_pane.height,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false, // multiline_mode
                true,  // context_injection_on
                0,     // injected_tokens
                None,  // current_hint
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                false,
                None,
                rustain::adapters::tui::state::LogAwareness::Hidden,
                0,
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
            let shell = rustain::adapters::tui::presentation::Shell::new(area, &app_layout);
            rustain::adapters::tui::presentation::compose(
                frame,
                &state,
                &shell,
                rustain::adapters::tui::keymap::Host::Local,
                &Default::default(),
                &mut rustain::adapters::tui::presentation::occlusion::OpaqueRects::new(
                    shell.chat_pane,
                ),
                |_| {},
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

    let mut state = TuiState::new(80, 24);
    let (tx, _rx) = tokio::sync::oneshot::channel();
    decision_queue::raise_question(
        &mut state,
        "test-ctx",
        "tu-ctx-1".to_string(),
        "What is your project name?".to_string(),
        tx,
    );

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
                &BTreeMap::<String, FeedbackBlock>::new(),
            );

            status_bar::render(
                frame,
                app_layout.status_bar,
                "test-model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                app_layout.chat_pane.height,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false, // multiline_mode
                true,  // context_injection_on
                0,     // injected_tokens
                None,  // current_hint
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                false,
                None,
                rustain::adapters::tui::state::LogAwareness::Hidden,
                0,
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
            // The question is a decision frame over the Stage: composed by the shared boundary.
            let shell = Shell::new(frame.area(), &app_layout);
            presentation::compose(
                frame,
                &state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("project name") && text.contains("Decision: Question"),
        "AskUserQuestion should show question text in its frame in full layout, got:\n{}",
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
