// Covers: FR38 (status bar), NFR2 (redraw), AC2 (status bar widget tests)
//! Dedicated rendering tests for the status bar widget.

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::status_bar;
use rustain::domain::models::{PermissionMode, StatusState, UsageInfo};

#[allow(clippy::too_many_arguments)]
fn render_status_bar(
    width: u16,
    model: &str,
    status: &StatusState,
    scroll_offset: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: u16,
    permission_mode: PermissionMode,
    token_usage: Option<&UsageInfo>,
    has_project_context: bool,
    session_title: Option<&str>,
) -> Terminal<TestBackend> {
    render_status_bar_ml(
        width,
        model,
        status,
        scroll_offset,
        message_boundaries,
        total_content_height,
        viewport_height,
        permission_mode,
        token_usage,
        has_project_context,
        session_title,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_status_bar_ml(
    width: u16,
    model: &str,
    status: &StatusState,
    scroll_offset: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: u16,
    permission_mode: PermissionMode,
    token_usage: Option<&UsageInfo>,
    has_project_context: bool,
    session_title: Option<&str>,
    multiline_mode: bool,
) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            status_bar::render(
                frame,
                area,
                model,
                status,
                &theme,
                scroll_offset,
                message_boundaries,
                total_content_height,
                viewport_height,
                permission_mode,
                token_usage,
                has_project_context,
                session_title,
                multiline_mode,
                None, // current_hint
            );
        })
        .unwrap();

    terminal
}

// Covers: FR38 (status bar), AC2 — model name visible
#[test]
fn test_status_bar_shows_model_name() {
    let terminal = render_status_bar(
        80,
        "sonnet-4-6",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        None,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("sonnet-4-6"),
        "Status bar should show model name, got: {:?}",
        text.trim()
    );
}

// Covers: FR38, AC2 — permission mode visible (Normal)
#[test]
fn test_status_bar_shows_normal_mode() {
    let terminal = render_status_bar(
        80,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        None,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Normal"),
        "Status bar should show Normal permission mode"
    );
}

// Covers: FR38, AC2 — permission mode visible (YOLO)
#[test]
fn test_status_bar_shows_yolo_mode() {
    let terminal = render_status_bar(
        80,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Yolo,
        None,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("YOLO"),
        "Status bar should show YOLO permission mode"
    );
}

// Covers: FR38, AC2 — scroll position visible when scrolled up
#[test]
fn test_status_bar_scroll_position() {
    let boundaries = vec![0, 10, 20, 30, 40];
    let terminal = render_status_bar(
        80,
        "test-model",
        &StatusState::Idle,
        15, // scrolled up
        &boundaries,
        50,
        20,
        PermissionMode::Normal,
        None,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("msg "),
        "Status bar should show scroll position indicator (e.g. 'msg 2/5') when scrolled, got: {:?}",
        text.trim()
    );
}

// Covers: FR38, AC2 — streaming indicator visible
#[test]
fn test_status_bar_streaming_indicator() {
    let terminal = render_status_bar(
        80,
        "test-model",
        &StatusState::Streaming,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        None,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Streaming"),
        "Status bar should show 'Streaming...' when streaming"
    );
}

// Covers: FR38, AC2 — usage/token info visible
#[test]
fn test_status_bar_token_usage() {
    let usage = UsageInfo {
        input_tokens: 1200,
        output_tokens: 3400,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let terminal = render_status_bar(
        80,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        Some(&usage),
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("1.2k"),
        "Status bar should show formatted input token count"
    );
    assert!(
        text.contains("3.4k"),
        "Status bar should show formatted output token count"
    );
}

// Covers: FR38, NFR2, AC2 — compact width still shows model + mode
#[test]
fn test_status_bar_compact_width() {
    let terminal = render_status_bar(
        60, // compact
        "sonnet-4-6",
        &StatusState::Idle,
        0,
        &[],
        0,
        12,
        PermissionMode::Normal,
        None,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("sonnet-4-6"),
        "Compact status bar should still show model name"
    );
    assert!(
        text.contains("Normal"),
        "Compact status bar should still show permission mode"
    );
}
