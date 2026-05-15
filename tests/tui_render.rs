use std::collections::HashMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::layout;
use rustain::adapters::tui::state::{TabRenderState, TuiState};
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::adapters::tui::widgets::{chat_pane, input_box, status_bar};
use rustain::domain::models::{Conversation, StreamingState};

fn render_frame(width: u16, height: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();

    let state = TuiState::new(width, height);
    let conversation = Conversation {
        id: String::new(),
        title: String::new(),
        messages: Vec::new(),
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    };
    let streaming = StreamingState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            if let Some(app_layout) = layout::compute_layout(area, &state.theme, &state.input_buffer, 1, false) {
                chat_pane::render(
                    frame,
                    app_layout.chat_pane,
                    &conversation,
                    &streaming,
                    state.scroll_offset(),
                    state.auto_scroll(),
                    &state.theme,
                    &mut TabRenderState::default(),
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    "claude-sonnet-4-6",
                None,
                    &state.status,
                    &state.theme,
                    0,
                    &[],
                    0,
                    app_layout.chat_pane.height,
                    rustain::domain::models::PermissionMode::Normal,
                    state.token_usage.as_ref(),
                    0, // context_window
                    state.has_project_context,
                    None,
                    state.multiline_mode,
                    None, // current_hint
                    0,
                    None,
                    None,
                    None, false,
                    None, // daily_budget (Story 7.5)
                );
                input_box::render(
                    frame,
                    app_layout.input_area,
                    &state.input_buffer,
                    state.cursor_position,
                    state.focus,
                    &state.theme,
                    state.multiline_mode,
                    state.input_scroll_offset,
                    None,
                );
            }
        })
        .unwrap();

    terminal
}

/// AC2 (partial): TUI frame renders with chat pane, status bar, and input area.
// Covers: NFR1 (cold start), NFR2 (redraw)
#[test]
fn test_tui_renders_empty_state() {
    let terminal = render_frame(80, 24);
    let buffer = terminal.backend().buffer().clone();

    // Check that "Welcome to Rustain." appears in the chat pane
    let content: String = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(
        content.contains("Welcome to Rustain."),
        "Expected welcome message in buffer"
    );

    // Check status bar contains model name
    assert!(
        content.contains("claude-sonnet-4-6"),
        "Expected model name in status bar"
    );

    // Check input area border is present (Message title)
    assert!(
        content.contains("Message"),
        "Expected input area with Message title"
    );
}

/// AC2: Layout adapts for compact terminals.
// Covers: NFR2 (responsive layout)
#[test]
fn test_tui_renders_compact_layout() {
    let terminal = render_frame(70, 20);
    let buffer = terminal.backend().buffer().clone();
    let content: String = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // Should still render the welcome message in compact mode
    assert!(
        content.contains("Welcome to Rustain."),
        "Expected welcome message in compact layout"
    );
}

/// Layout returns None for terminals smaller than 60x16.
// Covers: NFR2 (responsive layout)
#[test]
fn test_tui_too_small_terminal() {
    let theme = rustain::adapters::tui::theme::Theme::dark();
    let area = ratatui::prelude::Rect::new(0, 0, 50, 12);
    assert!(
        layout::compute_layout(area, &theme, "", 1, false).is_none(),
        "Expected None for terminal too small"
    );
}
