//! PermissionCard widget — renders inline permission prompts in the chat stream.

use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::tool_block::display_tool_name;
use crate::domain::models::tool_call::ApprovalSource;

/// Extract a short summary of the tool input for display.
#[cfg(test)]
fn tool_display(tool_name: &str, tool_input: &serde_json::Value) -> String {
    match tool_name {
        "Bash" | "bash" => tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| {
                if s.chars().count() > 80 {
                    let truncated: String = s.chars().take(77).collect();
                    format!("{}...", truncated)
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_default(),
        "Read" | "read" => tool_input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Write" | "write" => tool_input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => {
            let s = format!("{}", tool_input);
            if s.chars().count() > 80 {
                let truncated: String = s.chars().take(77).collect();
                format!("{}...", truncated)
            } else {
                s
            }
        }
    }
}

/// Render permission prompt lines for inline display in the chat pane.
/// Returns a Vec of Line objects to be appended to the rendered output.
///
/// Visual spec (5-action, 3-line):
/// ```text
/// ┃ Bash: cargo test --all              [3 more queued] ┃
/// ┃ [y] Allow  [s] Session  [a] Always                  ┃
/// ┃ [n] Deny   [f] Deny + feedback                      ┃
/// ```
pub fn render_permission_lines<'a>(
    source: &ApprovalSource,
    tool_name: &str,
    tool_input: &str,
    theme: &'a Theme,
    queue_len: usize,
) -> Vec<Line<'a>> {
    let display = if tool_input.is_empty() {
        String::new()
    } else {
        tool_input.to_string()
    };

    // Subagent/background prefix (AC11)
    let prefix = match source {
        ApprovalSource::ForegroundSubagent { subagent_type, .. } => {
            format!("[subagent: {}] ", subagent_type)
        }
        ApprovalSource::BackgroundAgent { subagent_type, .. } => {
            format!("[background: {}] ", subagent_type)
        }
        ApprovalSource::RemotePeer { peer_id, .. } => {
            format!("[remote peer: {}] ", peer_id)
        }
        ApprovalSource::ForegroundTurn { .. } | ApprovalSource::AcpSession { .. } => String::new(),
    };

    let border_style = Style::default()
        .fg(theme.colors.permission_border)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();

    // Line 1: optional prefix + tool name + command + optional queue indicator (AC6)
    let mut line1_spans = vec![Span::styled("┃ ", border_style)];
    if !prefix.is_empty() {
        line1_spans.push(Span::styled(
            prefix,
            Style::default().fg(theme.colors.subagent_attribution),
        ));
    }
    line1_spans.push(Span::styled(
        format!("{}: ", display_tool_name(tool_name)),
        Style::default()
            .fg(theme.colors.tool_name)
            .add_modifier(Modifier::BOLD),
    ));
    line1_spans.push(Span::styled(
        display,
        Style::default().fg(theme.colors.fg_primary),
    ));
    if queue_len > 0 {
        line1_spans.push(Span::styled(
            format!("  [{} more queued]", queue_len),
            Style::default().fg(theme.colors.fg_secondary),
        ));
    }
    line1_spans.push(Span::styled(" ┃", border_style));
    lines.push(Line::from(line1_spans));

    // Line 2: [y] Allow  [s] Session  [a] Always (or Always for [server] for MCP)
    let always_label = if let Some(rest) = tool_name.strip_prefix("mcp__") {
        if let Some((server, _)) = rest.split_once("__") {
            format!(" Always for [{server}]")
        } else {
            " Always".to_string()
        }
    } else {
        " Always".to_string()
    };
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled("[y]", Style::default().fg(theme.colors.success)),
        Span::styled(" Allow  ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[s]", Style::default().fg(theme.colors.accent)),
        Span::styled(" Session  ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[a]", Style::default().fg(theme.colors.warning)),
        Span::styled(always_label, Style::default().fg(theme.colors.fg_secondary)),
        Span::styled(" ┃", border_style),
    ]));

    // Line 3: [n] Deny   [f] Deny + feedback
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled("[n]", Style::default().fg(theme.colors.error)),
        Span::styled(" Deny   ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[f]", Style::default().fg(theme.colors.auto_sent_border)),
        Span::styled(
            " Deny + feedback",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        Span::styled(" ┃", border_style),
    ]));

    lines
}

/// Compute the rendered height of a permission prompt (3 lines).
#[allow(dead_code)]
pub fn permission_prompt_height() -> usize {
    3
}

/// Render feedback input lines for the deny-with-feedback mini-input (AC5).
///
/// Visual spec:
/// ```text
/// ┃ Feedback: <text>▋                                    ┃
/// ┃ [Enter] send  [Esc] cancel                           ┃
/// ```
pub fn render_feedback_input_lines<'a>(buffer: &str, theme: &'a Theme) -> Vec<Line<'a>> {
    let border_style = Style::default()
        .fg(theme.colors.auto_sent_border)
        .add_modifier(Modifier::BOLD);

    vec![
        // Line 1: Feedback: <buffer>▋
        Line::from(vec![
            Span::styled("┃ ", border_style),
            Span::styled("Feedback: ", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(
                buffer.to_string(),
                Style::default().fg(theme.colors.fg_primary),
            ),
            Span::styled("▋", Style::default().fg(theme.colors.fg_primary)),
            Span::styled(" ┃", border_style),
        ]),
        // Line 2: [Enter] send  [Esc] cancel
        Line::from(vec![
            Span::styled("┃ ", border_style),
            Span::styled("[Enter]", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(" send  ", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled("[Esc]", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(" cancel", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(" ┃", border_style),
        ]),
    ]
}

/// Render a blocked command notice in the chat stream.
#[allow(dead_code)]
pub fn render_blocked_notice_lines<'a>(reason: &str, theme: &'a Theme) -> Vec<Line<'a>> {
    let border_style = Style::default()
        .fg(theme.colors.error_border)
        .add_modifier(Modifier::BOLD);

    vec![Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled(
            format!("{} ", crate::domain::models::visual::symbols::ERROR),
            Style::default()
                .fg(theme.colors.error)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            reason.to_string(),
            Style::default().fg(theme.colors.fg_primary),
        ),
        Span::styled(" ┃", border_style),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tool_call::ApprovalSource;

    #[test]
    fn test_permission_prompt_renders_three_lines() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_permission_lines(
            &ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash",
            "cargo test",
            &theme,
            0,
        );
        assert_eq!(lines.len(), 3);

        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line1.contains("Bash"));
        assert!(line1.contains("cargo test"));

        let line2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line2.contains("[y]"));
        assert!(line2.contains("[s]"));
        assert!(line2.contains("[a]"));

        let line3: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line3.contains("[n]"));
        assert!(line3.contains("[f]"));
    }

    #[test]
    fn test_permission_prompt_height() {
        assert_eq!(permission_prompt_height(), 3);
    }

    #[test]
    fn test_permission_prompt_five_action_glyphs() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_permission_lines(
            &ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash",
            "ls",
            &theme,
            0,
        );
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("[y]"), "missing [y] glyph");
        assert!(all_text.contains("[s]"), "missing [s] glyph");
        assert!(all_text.contains("[a]"), "missing [a] glyph");
        assert!(all_text.contains("[n]"), "missing [n] glyph");
        assert!(all_text.contains("[f]"), "missing [f] glyph");
    }

    #[test]
    fn test_permission_prompt_queue_indicator_present() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_permission_lines(
            &ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash",
            "ls",
            &theme,
            3,
        );
        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line1.contains("[3 more queued]"),
            "queue indicator missing: {}",
            line1
        );
    }

    #[test]
    fn test_permission_prompt_queue_indicator_absent_when_zero() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_permission_lines(
            &ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash",
            "ls",
            &theme,
            0,
        );
        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !line1.contains("more queued"),
            "queue indicator should be absent: {}",
            line1
        );
    }

    #[test]
    fn test_feedback_input_renders_two_lines() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_feedback_input_lines("don't delete", &theme);
        assert_eq!(lines.len(), 2);
        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line1.contains("Feedback:"));
        assert!(line1.contains("don't delete"));
        assert!(line1.contains("▋"), "cursor marker missing");
        let line2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line2.contains("[Enter]"));
        assert!(line2.contains("[Esc]"));
    }

    #[test]
    fn test_blocked_notice_renders_one_line() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_blocked_notice_lines("Command blocked: rm -rf /", &theme);
        assert_eq!(lines.len(), 1);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("✗"));
        assert!(content.contains("Command blocked"));
    }

    #[test]
    fn test_tool_display_truncation() {
        let long_cmd = "a".repeat(100);
        let display = tool_display("Bash", &serde_json::json!({"command": long_cmd}));
        assert!(display.len() <= 83); // 77 chars + "..."
        assert!(display.ends_with("..."));
    }
}
