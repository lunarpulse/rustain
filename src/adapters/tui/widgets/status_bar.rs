use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::offset_to_message_index;
use crate::domain::models::{PermissionMode, StatusState, UsageInfo};

/// Render the status bar with model name, current status, scroll position, and permission mode.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    model: &str,
    status: &StatusState,
    theme: &Theme,
    scroll_offset: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: u16,
    permission_mode: PermissionMode,
    token_usage: Option<&UsageInfo>,
    has_project_context: bool,
    session_title: Option<&str>,
) {
    let status_text = status.display_text();
    let fg = theme.colors.status_fg;
    let sep = " │ ";

    // Build left side: model [ctx] │ mode │ tokens │ status
    // Spec layout: "sonnet-4-6 [ctx] │ normal │ ↑1.2k ↓3.4k │ Ready"
    let model_label = if has_project_context {
        format!(" {} [ctx]", model)
    } else {
        format!(" {}", model)
    };
    let mut left_spans: Vec<Span> = vec![Span::styled(model_label, Style::default().fg(fg))];

    // Session title (after model, if restored session)
    if let Some(title) = session_title {
        let display = if title.is_empty() { "Untitled" } else { title };
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            display.to_string(),
            Style::default().fg(fg),
        ));
    }

    // Permission mode (second segment)
    let mode_text = match permission_mode {
        PermissionMode::Normal => "Normal",
        PermissionMode::Yolo => "YOLO",
    };
    left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
    left_spans.push(match permission_mode {
        PermissionMode::Normal => Span::styled(mode_text.to_string(), Style::default().fg(fg)),
        PermissionMode::Yolo => Span::styled(
            mode_text.to_string(),
            Style::default()
                .fg(Color::White)
                .bg(theme.colors.status_yolo_warning)
                .add_modifier(Modifier::BOLD),
        ),
    });

    // Token usage (third segment)
    if let Some(usage) = token_usage {
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            format_token_usage(usage),
            Style::default().fg(fg),
        ));
    }

    // Status (fourth segment — rightmost, most dynamic)
    left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
    left_spans.push(Span::styled(
        status_text,
        Style::default().fg(if status.is_active() {
            theme.colors.status_streaming
        } else {
            fg
        }),
    ));

    // AC4: Show scroll position indicator when scrolled
    if scroll_offset > 0 && !message_boundaries.is_empty() {
        let (current, total) = offset_to_message_index(
            scroll_offset,
            viewport_height,
            message_boundaries,
            total_content_height,
        );
        left_spans.push(Span::styled(
            format!(" │ msg {}/{}", current, total),
            Style::default().fg(fg),
        ));
    }

    let line = Line::from(left_spans);
    let widget = Paragraph::new(line).style(Style::default().bg(theme.colors.status_bg));
    frame.render_widget(widget, area);
}

/// Format token counts compactly: raw numbers below 1000, `Xk` suffix above.
fn format_token_count(count: u32) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// Format token usage as `↑{input} ↓{output}`.
pub fn format_token_usage(usage: &UsageInfo) -> String {
    format!(
        "↑{} ↓{}",
        format_token_count(usage.input_tokens),
        format_token_count(usage.output_tokens)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::StatusState;

    #[test]
    fn test_format_token_count_small() {
        assert_eq!(format_token_count(500), "500");
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
    }

    #[test]
    fn test_format_token_count_large() {
        assert_eq!(format_token_count(1000), "1.0k");
        assert_eq!(format_token_count(1200), "1.2k");
        assert_eq!(format_token_count(3400), "3.4k");
        assert_eq!(format_token_count(10000), "10.0k");
    }

    #[test]
    fn test_format_token_usage() {
        let usage = UsageInfo {
            input_tokens: 1200,
            output_tokens: 3400,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        assert_eq!(format_token_usage(&usage), "↑1.2k ↓3.4k");
    }

    #[test]
    fn test_status_state_display_text() {
        assert_eq!(StatusState::Idle.display_text(), "Ready");
        assert_eq!(StatusState::Streaming.display_text(), "Streaming...");
        assert_eq!(
            StatusState::Executing {
                tool_name: "bash".to_string(),
                elapsed_ms: 500,
            }
            .display_text(),
            "Executing bash..."
        );
        assert_eq!(
            StatusState::Retrying {
                attempt: 2,
                max: 5,
                next_in_ms: 4000,
            }
            .display_text(),
            "Retrying (2/5) in 4.0s"
        );
        assert_eq!(
            StatusState::Flash {
                message: "Config error".to_string(),
                remaining_ms: 1000,
            }
            .display_text(),
            "Config error"
        );
    }

    #[test]
    fn test_status_state_is_active() {
        assert!(!StatusState::Idle.is_active());
        assert!(StatusState::Streaming.is_active());
        assert!(
            StatusState::Executing {
                tool_name: "test".to_string(),
                elapsed_ms: 0,
            }
            .is_active()
        );
        assert!(
            StatusState::Retrying {
                attempt: 1,
                max: 5,
                next_in_ms: 1000,
            }
            .is_active()
        );
        assert!(
            !StatusState::Flash {
                message: "test".to_string(),
                remaining_ms: 1000,
            }
            .is_active()
        );
    }
}
