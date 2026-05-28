// Covers: FR38 (status bar), NFR2 (redraw), AC2 (status bar widget tests)
//! Dedicated rendering tests for the status bar widget.

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::status_bar;
use rustain::domain::models::visual::DensityMode;
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
    context_window: u32,
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
        context_window,
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
    context_window: u32,
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
                None,
                status,
                &theme,
                scroll_offset,
                message_boundaries,
                total_content_height,
                viewport_height,
                permission_mode,
                token_usage,
                context_window,
                has_project_context,
                session_title,
                multiline_mode,
                None, // current_hint
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                DensityMode::Focus,
                false,
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
        0,
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
        0,
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
        0,
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
        0,
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
        0,
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
        reasoning_tokens: None,
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
        0,
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
        0,
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

// Covers: Story 5.4 AC7 — active agent visible in status bar
#[test]
fn test_status_bar_shows_active_agent() {
    let backend = TestBackend::new(80, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            status_bar::render(
                frame,
                area,
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false,
                None,
                0,
                Some("code-reviewer"),
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                DensityMode::Focus,
                false,
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Agent: code-reviewer"),
        "Status bar should show active agent name, got: {:?}",
        text.trim()
    );
}

// Covers: Story 5.4 AC7 — no agent segment when none active
#[test]
fn test_status_bar_hides_agent_when_none() {
    let backend = TestBackend::new(80, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            status_bar::render(
                frame,
                area,
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false,
                None,
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                DensityMode::Focus,
                false,
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        !text.contains("Agent:"),
        "Status bar should NOT show 'Agent:' when no agent is active, got: {:?}",
        text.trim()
    );
}

// Covers: Story 5.4 AC7 — agent name truncated at 24 chars
#[test]
fn test_status_bar_agent_name_truncated() {
    let backend = TestBackend::new(80, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            status_bar::render(
                frame,
                area,
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false,
                None,
                0,
                Some("a-very-long-agent-name-that-exceeds-twenty-four"),
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                DensityMode::Focus,
                false,
            );
        })
        .unwrap();

    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("Agent:"),
        "Status bar should show truncated agent name, got: {:?}",
        text.trim()
    );
    assert!(
        !text.contains("a-very-long-agent-name-that-exceeds-twenty-four"),
        "Status bar should truncate long agent name"
    );
}

// ── Story 7.4 context-window segment tests ─────────────────────

// Covers: AC1 — ctx: segment renders when context_window > 0
#[test]
fn test_status_bar_shows_context_window_ratio() {
    let terminal = render_status_bar(
        120,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        Some(&UsageInfo {
            input_tokens: 12_000,
            output_tokens: 3_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        }),
        200_000,
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("ctx:"),
        "Status bar should show ctx: segment when context_window > 0, got: {:?}",
        text.trim()
    );
    assert!(
        text.contains("12k/200k"),
        "Status bar should show humanized ratio, got: {:?}",
        text.trim()
    );
    assert!(
        text.contains("(6%)"),
        "Status bar should show percentage, got: {:?}",
        text.trim()
    );
}

// Covers: AC1 — ctx: segment omitted when context_window == 0
#[test]
fn test_status_bar_hides_context_window_when_zero() {
    let terminal = render_status_bar(
        120,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        Some(&UsageInfo {
            input_tokens: 12_000,
            output_tokens: 3_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        }),
        0, // unknown model / no context window
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(
        !text.contains("ctx:"),
        "Status bar should omit ctx: segment when context_window == 0, got: {:?}",
        text.trim()
    );
}

// Covers: AC2 — threshold color behavior is applied (text presence only; color verified via manual QA)
#[test]
fn test_status_bar_context_window_threshold_text() {
    // <80% — normal text present
    let terminal = render_status_bar(
        120,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        Some(&UsageInfo {
            input_tokens: 10_000,
            output_tokens: 1_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        }),
        200_000, // 5%
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(text.contains("(5%)"), "Should show 5% ratio");

    // 80–95% — warning text present
    let terminal = render_status_bar(
        120,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        Some(&UsageInfo {
            input_tokens: 170_000,
            output_tokens: 1_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        }),
        200_000, // 85%
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(text.contains("(85%)"), "Should show 85% ratio");

    // >=95% — error text present
    let terminal = render_status_bar(
        120,
        "test-model",
        &StatusState::Idle,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        Some(&UsageInfo {
            input_tokens: 196_000,
            output_tokens: 1_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        }),
        200_000, // 98%
        false,
        None,
    );
    let text = common::buffer_text(&terminal);
    assert!(text.contains("(98%)"), "Should show 98% ratio");
}
