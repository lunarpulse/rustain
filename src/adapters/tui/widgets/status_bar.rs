use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::offset_to_message_index;
use crate::domain::models::{PermissionMode, StatusState, UsageInfo};

/// Render the status bar with model name, current status, scroll position, and permission mode.
// Covers: FR38, UX-DR76, UX-DR93
pub fn render(
    frame: &mut Frame,
    area: Rect,
    model: &str,
    provider_status: Option<&str>,
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
    multiline_mode: bool,
    current_hint: Option<&str>,
    active_skill_count: usize,
    active_agent_name: Option<&str>,
    pending_plan_reminder_at_turn: Option<u32>,
    drill_down_breadcrumb: Option<&str>,
    pinned_active: bool,
) {
    let status_text = status.display_text();
    let fg = theme.colors.status_fg;
    let sep = " │ ";

    // Build left side: model [ctx] │ mode │ tokens │ status
    // Spec layout: "sonnet-4-6 [ctx] │ normal │ ↑1.2k ↓3.4k │ Ready"
    let model_label = match provider_status {
        Some(provider_id) => {
            let combined = format!(" {}/{}", provider_id, model);
            if has_project_context {
                format!("{} [ctx]", combined)
            } else {
                combined
            }
        }
        None => {
            if has_project_context {
                format!(" {} [ctx]", model)
            } else {
                format!(" {}", model)
            }
        }
    };
    let mut left_spans: Vec<Span> = Vec::new();

    // S16.8 AC15: Persistent anchor indicator when Pinned (LEFT slot).
    if pinned_active {
        left_spans.push(Span::styled(
            " ⚓ ".to_string(),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }

    left_spans.push(Span::styled(model_label, Style::default().fg(fg)));

    if let Some(breadcrumb) = drill_down_breadcrumb {
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            breadcrumb.to_string(),
            Style::default().fg(theme.colors.fg_muted),
        ));
    }

    // Session title (after model, if restored session)
    if let Some(title) = session_title {
        let display = if title.is_empty() { "Untitled" } else { title };
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(display.to_string(), Style::default().fg(fg)));
    }

    // Permission mode (second segment)
    let mode_text = match permission_mode {
        PermissionMode::Plan => "PLAN",
        PermissionMode::Normal => "Normal",
        PermissionMode::AutoEdit => "AUTOEDIT",
        PermissionMode::Yolo => "YOLO",
    };
    left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
    left_spans.push(match permission_mode {
        PermissionMode::Plan => {
            Span::styled(mode_text.to_string(), Style::default().fg(Color::Blue))
        }
        PermissionMode::Normal => Span::styled(mode_text.to_string(), Style::default().fg(fg)),
        PermissionMode::AutoEdit => Span::styled(
            mode_text.to_string(),
            Style::default().fg(theme.colors.accent),
        ),
        PermissionMode::Yolo => Span::styled(
            mode_text.to_string(),
            Style::default()
                .fg(Color::White)
                .bg(theme.colors.status_yolo_warning)
                .add_modifier(Modifier::BOLD),
        ),
    });

    // Plan-mode reminder chip ( Story 6-0d AC5/AC8 )
    if permission_mode == PermissionMode::Plan {
        if let Some(turn) = pending_plan_reminder_at_turn {
            left_spans.push(Span::styled(
                format!("{}⟳ plan-reminder t+{}", sep, turn),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

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

    // ML mode indicator
    // Covers: UX-DR76
    if multiline_mode {
        left_spans.push(Span::styled(
            format!("{}[ML]", sep),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Active skill count (AC12: suppressed when zero)
    if active_skill_count > 0 {
        left_spans.push(Span::styled(
            format!("{}Skills: {} active", sep, active_skill_count),
            Style::default().fg(theme.colors.accent),
        ));
    }

    // Active agent (Story 5.4 AC7: show name when active, suppress when none)
    if let Some(name) = active_agent_name {
        let truncated = if name.len() > 24 {
            let mut end = 24;
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &name[..end])
        } else {
            name.to_string()
        };
        left_spans.push(Span::styled(
            format!("{}Agent: {}", sep, truncated),
            Style::default().fg(theme.colors.accent),
        ));
    }

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

    // Contextual hint: right-aligned in remaining space (UX-DR93, UX-DR96)
    // The hint is the first thing truncated if the terminal is too narrow.
    if let Some(hint) = current_hint {
        let left_line = Line::from(left_spans.clone());
        let left_width = left_line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>();
        let bar_width = area.width as usize;
        let hint_width = hint.chars().count();
        // Only render hint if it fits in the remaining space (with at least 1 gap)
        if left_width + 1 + hint_width <= bar_width {
            let padding = bar_width - left_width - hint_width;
            let mut spans_with_hint = left_spans;
            spans_with_hint.push(Span::raw(" ".repeat(padding)));
            spans_with_hint.push(Span::styled(
                hint,
                theme.typography.hint.fg(theme.colors.text_hint),
            ));
            let line = Line::from(spans_with_hint);
            let widget = Paragraph::new(line).style(Style::default().bg(theme.colors.status_bg));
            return frame.render_widget(widget, area);
        }
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
            reasoning_tokens: None,
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
