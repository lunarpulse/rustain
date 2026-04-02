//! PermissionCard widget — renders inline permission prompts in the chat stream.

use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

/// Extract a short summary of the tool input for display.
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
/// Visual spec:
/// ```text
/// ┃ Bash: cargo test --all                              ┃
/// ┃ [y] Allow  [n] Deny  [a] Always allow               ┃
/// ```
pub fn render_permission_lines<'a>(
    tool_name: &str,
    tool_input: &serde_json::Value,
    theme: &'a Theme,
) -> Vec<Line<'a>> {
    let display = tool_display(tool_name, tool_input);
    let border_style = Style::default()
        .fg(theme.colors.permission_border)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();

    // Line 1: tool name + command
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled(
            format!("{}: ", tool_name),
            Style::default()
                .fg(theme.colors.tool_name)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(display, Style::default().fg(theme.colors.fg_primary)),
        Span::styled(" ┃", border_style),
    ]));

    // Line 2: action keys
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled("[y]", Style::default().fg(theme.colors.success)),
        Span::styled(" Allow  ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[n]", Style::default().fg(theme.colors.error)),
        Span::styled(" Deny  ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[a]", Style::default().fg(theme.colors.warning)),
        Span::styled(
            " Always allow",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        Span::styled(" ┃", border_style),
    ]));

    lines
}

/// Compute the rendered height of a permission prompt.
#[allow(dead_code)]
pub fn permission_prompt_height() -> usize {
    2 // tool line + action keys line
}

/// Render a blocked command notice in the chat stream.
/// NOTE: AC5 blocked notices currently render via tool_block.rs error state
/// (bold red ┃ + ✗ + reason), which provides richer context (tool name, elapsed).
/// This standalone variant is available for non-tool-block blocked notices.
///
/// Visual spec:
/// ```text
/// ┃ ✗ Command blocked: dangerous pattern 'rm -rf /'     ┃
/// ```
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

    #[test]
    fn test_permission_prompt_renders_two_lines() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_permission_lines(
            "Bash",
            &serde_json::json!({"command": "cargo test"}),
            &theme,
        );
        assert_eq!(lines.len(), 2);

        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line1.contains("Bash"));
        assert!(line1.contains("cargo test"));

        let line2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line2.contains("[y]"));
        assert!(line2.contains("[n]"));
        assert!(line2.contains("[a]"));
    }

    #[test]
    fn test_permission_prompt_height() {
        assert_eq!(permission_prompt_height(), 2);
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
