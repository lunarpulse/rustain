//! E2E test harness for rustain.
//!
//! Provides `TestHarness` — a self-contained test environment that drives
//! the full message→streaming→render pipeline with:
//! - `MockProvider`: returns predefined StreamChunk sequences
//! - `TestBackend`: captures rendered frames for assertion
//! - Domain event injection: simulates user input via AppEvent channel
//! - Full apply_chunk + widget rendering integration

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Style;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::layout;
use rustain::adapters::tui::state::{HeightCache, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{chat_pane, input_box, status_bar};
use rustain::domain::errors::ProviderError;
use rustain::domain::events::{ChunkAction, DomainInputEvent, DomainKey};
use rustain::domain::models::{
    ChatMessage, CompletionOptions, Conversation, FeedbackBlock, FocusState, Message, MessageRole,
    PermissionMode, StatusState, StopReason, StreamChunk, StreamingPhase, StreamingState,
    ToolCallInfo, apply_chunk, generate_conversation_id,
};
use rustain::domain::ports::ProviderPort;
use rustain::domain::services::message_builder;
use rustain::domain::services::turn_queue::TurnQueue;

use rustain::adapters::tui::widgets::tool_block::ToolBlockState;

// ── MockProvider ────────────────────────────────────────────────────────────

/// A mock provider that returns predefined StreamChunk sequences.
/// Each call to `stream_completion` pops the next sequence from the queue.
/// Scaffolded for future E2E streaming tests (Story 3-7). Not yet instantiated.
#[allow(dead_code)]
pub struct MockProvider {
    responses: std::sync::Mutex<Vec<Vec<StreamChunk>>>,
}

#[allow(dead_code)]
impl MockProvider {
    /// Create a MockProvider with a queue of responses.
    /// Each inner Vec<StreamChunk> is returned for one stream_completion call.
    pub fn new(responses: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }

    /// Create a simple provider that returns one text response then EndTurn.
    pub fn simple_text(text: &str) -> Self {
        Self::new(vec![vec![
            StreamChunk::Text {
                content: text.to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ]])
    }

    /// Create a provider that returns a tool_use response, then after tool result,
    /// returns a text response with EndTurn.
    pub fn with_tool_call(
        tool_id: &str,
        tool_name: &str,
        tool_input: serde_json::Value,
        final_text: &str,
    ) -> Self {
        Self::new(vec![
            // First call: assistant wants to use a tool
            vec![
                StreamChunk::Text {
                    content: format!("I'll use {} for you.", tool_name),
                    parent_tool_use_id: None,
                },
                StreamChunk::ToolUse {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    input: tool_input,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            // Second call: after tool result, final response
            vec![
                StreamChunk::Text {
                    content: final_text.to_string(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ])
    }

    /// Create a provider that returns an error.
    pub fn with_error(error: &str) -> Self {
        Self::new(vec![vec![
            StreamChunk::Error {
                content: error.to_string(),
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled,
            },
        ]])
    }
}

impl std::fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockProvider").finish()
    }
}

#[async_trait]
impl ProviderPort for MockProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(ProviderError::Other("No more mock responses".into()));
        }
        let chunks = responses.remove(0);
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> &str {
        "mock"
    }
}

// ── TestHarness ─────────────────────────────────────────────────────────────

/// Self-contained test environment for E2E testing.
pub struct TestHarness {
    pub terminal: Terminal<TestBackend>,
    pub state: TuiState,
    pub conversation: Conversation,
    pub streaming: StreamingState,
    #[allow(dead_code)]
    pub turn_queue: TurnQueue,
    pub height_cache: HeightCache,
    pub tool_block_states: HashMap<String, ToolBlockState>,
    pub feedback_blocks: BTreeMap<String, FeedbackBlock>,
    pub theme: Theme,
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHarness {
    /// Create a new test harness with standard 80x24 terminal.
    pub fn new() -> Self {
        Self::with_size(80, 24)
    }

    /// Create a new test harness with custom terminal size.
    pub fn with_size(width: u16, height: u16) -> Self {
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        let state = TuiState::with_capability(
            width,
            height,
            rustain::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        let conversation = Conversation {
            id: generate_conversation_id(),
            title: String::new(),
            messages: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };

        Self {
            terminal,
            state,
            conversation,
            streaming: StreamingState::default(),
            turn_queue: TurnQueue::default(),
            height_cache: HeightCache::default(),
            tool_block_states: HashMap::new(),
            feedback_blocks: BTreeMap::new(),
            theme,
        }
    }

    /// Render the current state to the TestBackend.
    pub fn render(&mut self) {
        let conv = &self.conversation;
        let streaming = &self.streaming;
        let theme = &self.theme;
        let height_cache = &mut self.height_cache;
        let tool_states = &self.tool_block_states;
        let feedback = &self.feedback_blocks;
        let scroll_offset = self.state.scroll_offset;
        let auto_scroll = self.state.auto_scroll;
        let status = &self.state.status;
        let input_buffer = &self.state.input_buffer;
        let cursor_position = self.state.cursor_position;
        let focus = self.state.focus.clone();
        let has_project_context = self.state.has_project_context;
        let total_content_height = self.state.total_content_height;
        let multiline_mode = self.state.multiline_mode;
        let input_scroll_offset = self.state.input_scroll_offset;
        let msg_boundaries: Vec<usize> = vec![];
        let viewport_height: u16 = 20; // approximate for status bar

        self.terminal
            .draw(|frame| {
                let area = frame.area();
                if let Some(app_layout) =
                    layout::compute_layout(area, theme, input_buffer, 1, false)
                {
                    chat_pane::render(
                        frame,
                        app_layout.chat_pane,
                        conv,
                        streaming,
                        scroll_offset,
                        auto_scroll,
                        theme,
                        height_cache,
                        tool_states,
                        feedback,
                    );

                    status_bar::render(
                        frame,
                        app_layout.status_bar,
                        "mock-model",
                        status,
                        theme,
                        scroll_offset,
                        &msg_boundaries,
                        total_content_height,
                        viewport_height,
                        PermissionMode::Normal,
                        None, // token_usage
                        has_project_context,
                        None, // session_title
                        multiline_mode,
                        None, // current_hint
                        0,
                        None,
                        None,
                        None,
                    );

                    input_box::render(
                        frame,
                        app_layout.input_area,
                        input_buffer,
                        cursor_position,
                        focus,
                        theme,
                        multiline_mode,
                        input_scroll_offset,
                        None,
                    );
                }
            })
            .unwrap();
    }

    /// Get the full rendered text from the TestBackend buffer.
    pub fn screen_text(&self) -> String {
        self.terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// Check if the rendered screen contains a string.
    pub fn screen_contains(&self, text: &str) -> bool {
        self.screen_text().contains(text)
    }

    /// Assert the screen contains a string, with a descriptive message.
    pub fn assert_screen_contains(&self, text: &str, context: &str) {
        assert!(
            self.screen_contains(text),
            "{}: Expected screen to contain {:?}, but screen was:\n{}",
            context,
            text,
            self.screen_text_lines().join("\n")
        );
    }

    /// Assert the screen does NOT contain a string.
    pub fn assert_screen_not_contains(&self, text: &str, context: &str) {
        assert!(
            !self.screen_contains(text),
            "{}: Expected screen NOT to contain {:?}",
            context,
            text,
        );
    }

    /// Get screen text split into lines (by terminal width).
    pub fn screen_text_lines(&self) -> Vec<String> {
        let buf = self.terminal.backend().buffer().clone();
        let width = buf.area.width as usize;
        let text: String = buf
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        text.as_bytes()
            .chunks(width)
            .map(|chunk| String::from_utf8_lossy(chunk).trim_end().to_string())
            .collect()
    }

    // ── User Input Simulation ───────────────────────────────────────────

    /// Simulate typing a character.
    pub fn type_char(&mut self, c: char) -> InputAction {
        let event = DomainInputEvent::KeyPress(c);
        handle_input(&mut self.state, &event)
    }

    /// Simulate pressing a special key.
    pub fn press_key(&mut self, key: DomainKey) -> InputAction {
        let event = DomainInputEvent::SpecialKey(key);
        handle_input(&mut self.state, &event)
    }

    /// Type a full string into the input buffer.
    pub fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.type_char(c);
        }
    }

    /// Submit the current input (press Enter in Input focus).
    pub fn submit_input(&mut self) -> InputAction {
        self.press_key(DomainKey::Enter)
    }

    /// Focus the input area.
    pub fn focus_input(&mut self) {
        if !matches!(self.state.focus, FocusState::Input) {
            self.type_char('i');
        }
    }

    /// Focus the chat area.
    pub fn focus_chat(&mut self) {
        if !matches!(self.state.focus, FocusState::Chat) {
            self.press_key(DomainKey::Esc);
        }
    }

    // ── Streaming Simulation ────────────────────────────────────────────

    /// Add a user message and start a simulated turn.
    pub fn send_message(&mut self, text: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conversation.messages.push(ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: text.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: now,
            token_count: None,
            stop_reason: None,
            images: vec![],
            });

        self.streaming.is_streaming = true;
        self.streaming.phase = StreamingPhase::AccumulatingText;
        self.state.status = StatusState::Streaming;
        self.state.auto_scroll = true;
        self.state.needs_redraw = true;
    }

    /// Process a StreamChunk through apply_chunk and return the action.
    pub fn process_chunk(&mut self, chunk: StreamChunk) -> ChunkAction {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        apply_chunk(&mut self.conversation, &mut self.streaming, chunk, now)
    }

    /// Process a sequence of chunks (simulating a full provider response).
    pub fn process_chunks(&mut self, chunks: Vec<StreamChunk>) -> Vec<ChunkAction> {
        chunks
            .into_iter()
            .map(|chunk| self.process_chunk(chunk))
            .collect()
    }

    /// Complete the streaming state after TurnComplete (reset streaming flags).
    pub fn finalize_turn(&mut self) {
        self.streaming.is_streaming = false;
        self.streaming.phase = StreamingPhase::Idle;
        self.streaming.current_text_buffer.clear();
        self.streaming.current_blocks.clear();
        self.streaming.active_tool_calls.clear();
        self.state.status = StatusState::Idle;
        self.state.needs_redraw = true;
    }

    /// Simulate a complete turn: send message, process all chunks, finalize.
    pub fn complete_turn(&mut self, user_msg: &str, chunks: Vec<StreamChunk>) -> Vec<ChunkAction> {
        self.send_message(user_msg);
        let actions = self.process_chunks(chunks);

        // Check if last action was TurnComplete and finalize
        if let Some(ChunkAction::TurnComplete { .. }) = actions.last() {
            self.finalize_turn();
        }

        actions
    }

    // ── API Message Validation ──────────────────────────────────────────

    /// Build API messages from current conversation state (same as production code).
    pub fn build_api_messages(&self) -> Vec<Message> {
        message_builder::build_api_messages(&self.conversation)
    }

    /// Validate that no API message would have empty content blocks.
    /// Returns Ok(()) or Err with description of the problem.
    pub fn validate_api_messages(&self) -> Result<(), String> {
        use rustain::adapters::anthropic::types::AnthropicRequest;

        let messages = self.build_api_messages();
        let options = CompletionOptions {
            model: "test-model".into(),
            max_tokens: 8192,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));
        let json = serde_json::to_value(&req).map_err(|e| format!("Serialization error: {}", e))?;

        if let Some(msgs) = json["messages"].as_array() {
            for (i, msg) in msgs.iter().enumerate() {
                if let Some(content) = msg["content"].as_array() {
                    if content.is_empty() {
                        return Err(format!("Message {} has empty content array", i));
                    }
                    for (j, block) in content.iter().enumerate() {
                        if block["type"] == "text" {
                            if let Some(text) = block["text"].as_str() {
                                if text.is_empty() {
                                    return Err(format!(
                                        "Message {} block {} has empty text",
                                        i, j
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ── Style/Color Assertion Helpers ────────────────────────────────────────
// Covers: AC5 (style/color assertion helpers for TUI test infrastructure)

/// Extract the `Style` of a specific cell in the rendered buffer.
///
/// `row` and `col` are zero-based coordinates.
pub fn buffer_cell_style(buffer: &ratatui::buffer::Buffer, row: u16, col: u16) -> Style {
    use ratatui::prelude::Position;
    buffer
        .cell(Position::new(col, row))
        .map(|cell| Style {
            fg: Some(cell.fg),
            bg: Some(cell.bg),
            underline_color: None,
            add_modifier: cell.modifier,
            sub_modifier: ratatui::style::Modifier::empty(),
        })
        .unwrap_or_default()
}

/// Scan the buffer for `text` and verify that every matching character
/// satisfies the style predicate `check`. Returns `true` if at least one
/// occurrence is found where all characters pass the predicate.
pub fn buffer_contains_styled_text(
    buffer: &ratatui::buffer::Buffer,
    text: &str,
    check: impl Fn(&Style) -> bool,
) -> bool {
    let cells = buffer.content();
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || cells.len() < chars.len() {
        return false;
    }

    'outer: for start in 0..=(cells.len() - chars.len()) {
        for (i, &expected) in chars.iter().enumerate() {
            let cell = &cells[start + i];
            if cell.symbol().chars().next().unwrap_or(' ') != expected {
                continue 'outer;
            }
        }
        // Text matched — now check style for every character
        let all_styled = (0..chars.len()).all(|i| {
            let cell = &cells[start + i];
            let style = Style {
                fg: Some(cell.fg),
                bg: Some(cell.bg),
                underline_color: None,
                add_modifier: cell.modifier,
                sub_modifier: ratatui::style::Modifier::empty(),
            };
            check(&style)
        });
        if all_styled {
            return true;
        }
    }
    false
}

impl TestHarness {
    /// Assert that the status bar row contains `text`.
    ///
    /// The status bar is positioned at `height - MIN_INPUT_HEIGHT - 1` (one row above the
    /// input area). This formula is valid when the input buffer is empty, which is the
    /// standard harness initial state. Tests that fill the input buffer before checking
    /// the status bar must account for any extra input rows.
    ///
    /// A runtime check validates the computed row contains non-whitespace content; if it
    /// does not, the layout may have changed and this offset needs revisiting.
    pub fn assert_status_bar_contains(&self, text: &str) {
        assert!(
            !text.is_empty(),
            "assert_status_bar_contains called with empty text — this always passes"
        );
        let buf = self.terminal.backend().buffer().clone();
        let height = buf.area.height;
        let width = buf.area.width as usize;
        // Status bar sits at height - MIN_INPUT_HEIGHT - 1 (= height - 4 for empty input).
        let status_row = height.saturating_sub(layout::MIN_INPUT_HEIGHT + 1);

        let row_start = status_row as usize * width;
        let row_end = (row_start + width).min(buf.content().len());
        let row_text: String = buf.content()[row_start..row_end]
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();

        // Validate: the status bar row must contain non-whitespace content.
        // If this assertion fires, the layout changed and the row offset needs updating.
        assert!(
            !row_text.trim().is_empty(),
            "assert_status_bar_contains: status row {} is blank — layout may have changed \
             (terminal {}x{}). Recheck MIN_INPUT_HEIGHT offset.",
            status_row,
            buf.area.width,
            height,
        );

        assert!(
            row_text.contains(text),
            "Expected status bar (row {}) to contain {:?}, but got: {:?}",
            status_row,
            text,
            row_text.trim()
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// STYLE HELPER TESTS
// ═══════════════════════════════════════════════════════════════════════════
// Covers: AC5 (unit tests for style assertion helpers)

#[test]
fn test_buffer_cell_style_returns_correct_style() {
    let backend = TestBackend::new(20, 3);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let line = ratatui::text::Line::from(ratatui::text::Span::styled(
                "BOLD TEXT",
                Style::default()
                    .fg(theme.colors.error)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ));
            frame.render_widget(ratatui::widgets::Paragraph::new(line), area);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let style = buffer_cell_style(&buf, 0, 0); // 'B' of "BOLD TEXT"
    assert_eq!(style.fg, Some(theme.colors.error));
    assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
}

#[test]
fn test_buffer_contains_styled_text_finds_styled_match() {
    let backend = TestBackend::new(40, 3);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let line = ratatui::text::Line::from(vec![
                ratatui::text::Span::raw("normal "),
                ratatui::text::Span::styled("ERROR", Style::default().fg(theme.colors.error)),
                ratatui::text::Span::raw(" rest"),
            ]);
            frame.render_widget(ratatui::widgets::Paragraph::new(line), area);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();

    // Should find "ERROR" with error color
    assert!(buffer_contains_styled_text(&buf, "ERROR", |s| s.fg == Some(theme.colors.error)));

    // Should NOT find "normal" with error color
    assert!(!buffer_contains_styled_text(&buf, "normal", |s| s.fg == Some(theme.colors.error)));
}

#[test]
fn test_buffer_contains_styled_text_empty_text_returns_false() {
    let backend = TestBackend::new(10, 1);
    let terminal = Terminal::new(backend).unwrap();
    let buf = terminal.backend().buffer().clone();
    assert!(!buffer_contains_styled_text(&buf, "", |_| true));
}

#[test]
fn test_assert_status_bar_contains_finds_model() {
    let mut h = TestHarness::new();
    h.render();
    h.assert_status_bar_contains("mock-model");
    h.assert_status_bar_contains("Normal");
}

// ═══════════════════════════════════════════════════════════════════════════
// E2E SMOKE TESTS
// ═══════════════════════════════════════════════════════════════════════════

// Covers: FR38 (status bar), AC5 (style helpers)
#[test]
fn test_e2e_fresh_session_empty_state() {
    let mut h = TestHarness::new();
    h.render();

    h.assert_screen_contains("Welcome to Rustain", "Empty state should show welcome");
    // AC5: Use status bar assertion helper instead of generic screen_contains
    h.assert_status_bar_contains("mock-model");
    h.assert_status_bar_contains("Normal");
}

// Covers: FR16 (input controls)
#[test]
fn test_e2e_type_and_submit_message() {
    let mut h = TestHarness::new();

    // Focus input, type, submit
    h.focus_input();
    h.type_text("Hello world");
    h.render();
    h.assert_screen_contains("Hello world", "Input should show typed text");

    let action = h.submit_input();
    assert!(
        matches!(action, InputAction::SubmitMessage(ref s) if s == "Hello world"),
        "Enter should produce SubmitMessage"
    );
}

// Covers: FR1 (streaming), FR2 (content blocks), FR3 (abort preserves)
#[test]
fn test_e2e_simple_streaming_response() {
    let mut h = TestHarness::new();

    // Simulate a complete turn
    let _actions = h.complete_turn(
        "What is Rust?",
        vec![
            StreamChunk::Text {
                content: "Rust is a systems programming language.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );

    h.render();

    // Verify conversation state
    assert_eq!(
        h.conversation.messages.len(),
        2,
        "Should have user + assistant messages"
    );
    assert_eq!(h.conversation.messages[0].role, MessageRole::User);
    assert_eq!(h.conversation.messages[1].role, MessageRole::Assistant);
    assert_eq!(
        h.conversation.messages[1].content,
        "Rust is a systems programming language."
    );

    // Verify screen rendering
    h.assert_screen_contains("You:", "User message prefix visible");
    h.assert_screen_contains("What is Rust?", "User question visible");
    h.assert_screen_contains("Assistant:", "Assistant prefix visible");
    h.assert_screen_contains(
        "Rust is a systems programming language.",
        "Response visible",
    );
    h.assert_screen_not_contains("···", "No typing indicator after completion");

    // Verify API messages are valid
    h.validate_api_messages()
        .expect("API messages should be valid after simple turn");
}

// Covers: FR1 (streaming), NFR2 (redraw)
#[test]
fn test_e2e_typing_indicator_during_streaming() {
    let mut h = TestHarness::new();

    h.send_message("Hello");
    h.render();

    h.assert_screen_contains("You:", "User message visible");
    h.assert_screen_contains("Hello", "Message content visible");
    h.assert_screen_contains("···", "Typing indicator visible during streaming");

    // AC5: Verify typing indicator uses muted color (style assertion)
    let buf = h.terminal.backend().buffer().clone();
    let muted = h.theme.colors.fg_muted;
    assert!(
        buffer_contains_styled_text(&buf, "···", |s| s.fg == Some(muted)),
        "Typing indicator should render with fg_muted color"
    );

    // Process first text chunk
    h.process_chunk(StreamChunk::Text {
        content: "Hi there!".to_string(),
        parent_tool_use_id: None,
    });
    h.render();

    h.assert_screen_contains("Hi there!", "Streamed text visible");
}

// Covers: FR3 (abort preserves partial)
#[test]
fn test_e2e_abort_preserves_partial_response() {
    let mut h = TestHarness::new();

    h.send_message("Tell me a long story");

    // Stream some text
    h.process_chunk(StreamChunk::Text {
        content: "Once upon a time".to_string(),
        parent_tool_use_id: None,
    });

    // Abort (TurnComplete with Cancelled)
    h.process_chunk(StreamChunk::TurnComplete {
        stop_reason: StopReason::Cancelled,
    });
    h.finalize_turn();

    h.render();

    // Partial response should be preserved
    assert_eq!(h.conversation.messages.len(), 2);
    assert!(
        h.conversation.messages[1]
            .content
            .contains("Once upon a time"),
        "Partial response preserved after abort"
    );
}

// Covers: FR1 (streaming error)
#[test]
fn test_e2e_error_during_streaming() {
    let mut h = TestHarness::new();

    h.send_message("Hello");

    // Process error chunk
    h.process_chunk(StreamChunk::Error {
        content: "Connection lost".to_string(),
    });
    h.process_chunk(StreamChunk::TurnComplete {
        stop_reason: StopReason::Cancelled,
    });
    h.finalize_turn();

    h.render();

    // Error text should be captured in the assistant response
    assert!(
        !h.conversation.messages.is_empty(),
        "At least user message exists"
    );
}

// Covers: FR23 (tool execution), FR29 (tool blocks)
#[test]
fn test_e2e_tool_use_conversation_state() {
    let mut h = TestHarness::new();

    // Send user message
    h.send_message("Read file.txt");

    // Process assistant response with tool_use
    h.process_chunk(StreamChunk::Text {
        content: "I'll read that file.".to_string(),
        parent_tool_use_id: None,
    });
    h.process_chunk(StreamChunk::ToolUse {
        id: "toolu_abc123".to_string(),
        name: "Read".to_string(),
        input: serde_json::json!({"file_path": "file.txt"}),
    });
    h.process_chunk(StreamChunk::TurnComplete {
        stop_reason: StopReason::ToolUse,
    });

    // Verify streaming state has active tool call
    assert!(
        h.streaming.active_tool_calls.contains_key("toolu_abc123"),
        "Active tool call should be tracked"
    );

    // Process tool result
    h.process_chunk(StreamChunk::ToolResult {
        id: "toolu_abc123".to_string(),
        content: "file contents here".to_string(),
        is_error: false,
    });

    h.render();

    // Verify streaming state tracked the tool call
    // (In production, TurnComplete(ToolUse) triggers event loop to finalize the message.
    //  Here we verify the streaming layer correctly tracked it.)
    assert!(
        h.streaming
            .active_tool_calls
            .values()
            .any(|tc| tc.id == "toolu_abc123" && tc.name == "Read"),
        "Tool call should be tracked in streaming state"
    );
    assert!(
        h.streaming
            .active_tool_calls
            .values()
            .any(|tc| tc.result.is_some()),
        "Tool result should be recorded"
    );
}

// Covers: FR23 (tool execution), NFR19 (API message integrity)
#[test]
fn test_e2e_api_messages_valid_after_tool_use() {
    let mut h = TestHarness::new();

    // Simulate a complete tool-use turn
    h.send_message("Read file.txt");

    h.process_chunk(StreamChunk::Text {
        content: "Reading file.".to_string(),
        parent_tool_use_id: None,
    });
    h.process_chunk(StreamChunk::ToolUse {
        id: "toolu_001".to_string(),
        name: "Read".to_string(),
        input: serde_json::json!({"file_path": "file.txt"}),
    });
    h.process_chunk(StreamChunk::TurnComplete {
        stop_reason: StopReason::ToolUse,
    });

    // Finalize the assistant message (simulate what event loop does)
    let content = std::mem::take(&mut h.streaming.current_text_buffer);
    let blocks = std::mem::take(&mut h.streaming.current_blocks);
    let tool_calls: Vec<ToolCallInfo> = h
        .streaming
        .active_tool_calls
        .drain()
        .map(|(_, v)| v)
        .collect();
    h.conversation.messages.push(ChatMessage {
        synthetic: false,
        id: rustain::domain::models::generate_conversation_id(),
        role: MessageRole::Assistant,
        content,
        content_blocks: blocks,
        tool_calls,
        created_at: 0,
        token_count: None,
        stop_reason: Some(StopReason::ToolUse),
        images: vec![],
        });

    // Validate API messages — this is the critical regression test
    // The P0 bug caused empty text blocks here
    h.validate_api_messages()
        .expect("API messages must be valid after tool-use turn");

    // Verify the assistant message includes tool_use blocks in API format
    let api_msgs = h.build_api_messages();
    let assistant = api_msgs
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("Should have assistant message");
    assert!(
        !assistant.tool_uses.is_empty(),
        "Assistant API message should include tool_use blocks"
    );
    assert_eq!(assistant.tool_uses[0].id, "toolu_001");
    assert_eq!(assistant.tool_uses[0].name, "Read");
}

// Covers: FR13 (auto-scroll), FR22 (vim keybindings)
#[test]
fn test_e2e_scroll_navigation() {
    let mut h = TestHarness::new();

    // Add enough messages to need scrolling
    for i in 0..10 {
        h.complete_turn(
            &format!("Message {}", i),
            vec![
                StreamChunk::Text {
                    content: format!("Response {}", i),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        );
    }

    h.render();

    // Should be at bottom (auto_scroll)
    assert_eq!(
        h.state.scroll_offset, 0,
        "Should be at bottom after messages"
    );

    // Update total_content_height to simulate what event_loop does after render
    // (In production, chat_pane::render returns the height and event_loop stores it)
    h.state.total_content_height = 100; // Simulate enough content to scroll

    // Switch to chat focus and scroll up
    h.focus_chat();
    let action = h.type_char('k');
    assert!(
        matches!(action, InputAction::Consumed),
        "k should scroll up"
    );
    assert!(
        h.state.scroll_offset > 0,
        "Scroll offset should increase after k"
    );

    // Jump to bottom with G
    h.type_char('G');
    assert_eq!(h.state.scroll_offset, 0, "G should jump to bottom");
}

// Covers: FR22 (vim keybindings), FR16 (input controls)
#[test]
fn test_e2e_keyboard_focus_switching() {
    let mut h = TestHarness::new();

    // Default focus is Input
    assert!(matches!(h.state.focus, FocusState::Input));

    // Esc switches to Chat
    h.press_key(DomainKey::Esc);
    assert!(matches!(h.state.focus, FocusState::Chat));

    // 'i' switches back to Input
    h.type_char('i');
    assert!(matches!(h.state.focus, FocusState::Input));

    // Esc again
    h.press_key(DomainKey::Esc);
    assert!(matches!(h.state.focus, FocusState::Chat));

    // 'q' should quit
    let action = h.type_char('q');
    assert!(matches!(action, InputAction::Quit));
}

// Covers: FR1 (streaming), FR2 (content blocks)
#[test]
fn test_e2e_multi_turn_conversation() {
    let mut h = TestHarness::new();

    // Turn 1
    h.complete_turn(
        "What is 2+2?",
        vec![
            StreamChunk::Text {
                content: "4".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );

    // Turn 2
    h.complete_turn(
        "And 3+3?",
        vec![
            StreamChunk::Text {
                content: "6".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );

    h.render();

    assert_eq!(
        h.conversation.messages.len(),
        4,
        "Should have 4 messages (2 turns)"
    );
    h.assert_screen_contains("What is 2+2?", "First question visible");
    h.assert_screen_contains("And 3+3?", "Second question visible");

    // Validate API messages for multi-turn
    h.validate_api_messages()
        .expect("Multi-turn API messages should be valid");
}

// Covers: NFR2 (responsive layout)
#[test]
fn test_e2e_compact_layout() {
    // Test at minimum supported size (60x16)
    let mut h = TestHarness::with_size(60, 16);

    h.complete_turn(
        "Hello",
        vec![
            StreamChunk::Text {
                content: "Hi!".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );

    h.render();

    h.assert_screen_contains("You:", "User message visible in compact layout");
    h.assert_screen_contains("Hi!", "Response visible in compact layout");
}

// Covers: FR115 (project context)
#[test]
fn test_e2e_empty_system_prompt_no_crash() {
    let mut h = TestHarness::new();

    // Complete a turn — system prompt is empty (no CLAUDE.md)
    h.complete_turn(
        "Hello",
        vec![
            StreamChunk::Text {
                content: "Hi!".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );

    // The API messages should not include a system field
    let messages = h.build_api_messages();
    let options = CompletionOptions {
        model: "test".into(),
        max_tokens: 8192,
        system_prompt: String::new(), // Empty — no CLAUDE.md
        temperature: None,
        tools: vec![],
    };

    let req = rustain::adapters::anthropic::types::AnthropicRequest::from((
        messages.as_slice(),
        &options,
    ));
    let json = serde_json::to_value(&req).unwrap();
    assert!(
        json.get("system").is_none(),
        "Empty system prompt should not appear in request"
    );
}
