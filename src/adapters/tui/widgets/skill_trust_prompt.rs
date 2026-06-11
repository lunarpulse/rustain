//! Skill trust prompt widget — renders inline trust prompts in the chat stream.
//! Story 5-2 AC4: workspace-tier skill activation trust gate.

use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

/// Render trust prompt lines for inline display in the chat pane.
///
/// Visual spec:
/// ```text
/// ┃ New project skill detected: "review-code"              ┃
/// ┃ Trust and enable this skill for this session?           ┃
/// ┃ [y] Yes  [n] No  [i] Inspect              [2 queued]  ┃
/// ```
pub fn render_trust_lines<'a>(
    skill_name: &str,
    theme: &'a Theme,
    queue_len: usize,
) -> Vec<Line<'a>> {
    let border_style = Style::default()
        .fg(theme.colors.permission_border)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();

    // Line 1: New project skill detected: "{name}"
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled(
            "New project skill detected: ",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        Span::styled(
            format!("\"{}\"", skill_name),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ┃", border_style),
    ]));

    // Line 2: Trust and enable this skill for this session?
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled(
            "Trust and enable this skill for this session? ",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        Span::styled(" ┃", border_style),
    ]));

    // Line 3: [y] Yes  [n] No  [i] Inspect  [N queued]
    let mut line3_spans = vec![
        Span::styled("┃ ", border_style),
        Span::styled("[y]", Style::default().fg(theme.colors.success)),
        Span::styled(" Yes  ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[n]", Style::default().fg(theme.colors.error)),
        Span::styled(" No  ", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled("[i]", Style::default().fg(theme.colors.accent)),
        Span::styled(" Inspect", Style::default().fg(theme.colors.fg_secondary)),
    ];
    if queue_len > 0 {
        line3_spans.push(Span::styled(
            format!("  [{} more queued]", queue_len),
            Style::default().fg(theme.colors.fg_secondary),
        ));
    }
    line3_spans.push(Span::styled(" ┃", border_style));
    lines.push(Line::from(line3_spans));

    lines
}

/// Render inspection mode lines for viewing skill file contents.
///
/// Visual spec:
/// ```text
/// ┃ Inspect skill: "review-code"                            ┃
/// ┃ <file contents, scrollable>                              ┃
/// ┃ [Esc] Back to trust prompt                               ┃
/// ```
pub fn render_inspect_lines<'a>(
    skill_name: &str,
    content: &str,
    max_lines: usize,
    theme: &'a Theme,
) -> Vec<Line<'a>> {
    let border_style = Style::default()
        .fg(theme.colors.accent)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();

    // Header line
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled(
            "Inspect skill: ",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        Span::styled(
            format!("\"{}\"", skill_name),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ┃", border_style),
    ]));

    // Content lines (truncated)
    let content_lines: Vec<&str> = content.lines().take(max_lines).collect();
    for line_content in &content_lines {
        let display = if line_content.chars().count() > 78 {
            let truncated: String = line_content.chars().take(75).collect();
            format!("{}...", truncated)
        } else {
            line_content.to_string()
        };
        lines.push(Line::from(vec![
            Span::styled("┃ ", border_style),
            Span::styled(display, Style::default().fg(theme.colors.fg_primary)),
            Span::styled(" ┃", border_style),
        ]));
    }
    if content.lines().count() > max_lines {
        lines.push(Line::from(vec![
            Span::styled("┃ ", border_style),
            Span::styled(
                "... (truncated)",
                Style::default().fg(theme.colors.fg_secondary),
            ),
            Span::styled(" ┃", border_style),
        ]));
    }

    // Footer: [Esc] Back
    lines.push(Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled("[Esc]", Style::default().fg(theme.colors.fg_secondary)),
        Span::styled(
            " Back to trust prompt",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        Span::styled(" ┃", border_style),
    ]));

    lines
}

/// Compute the rendered height of a trust prompt (3 lines).
#[allow(dead_code)]
pub fn trust_prompt_height() -> usize {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trust_prompt_renders_three_lines() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_trust_lines("review-code", &theme, 0);
        assert_eq!(lines.len(), 3);

        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line1.contains("review-code"));

        let line3: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line3.contains("[y]"));
        assert!(line3.contains("[n]"));
        assert!(line3.contains("[i]"));
    }

    #[test]
    fn test_trust_prompt_queue_indicator() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_trust_lines("review-code", &theme, 2);
        let line3: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line3.contains("[2 more queued]"));
    }

    #[test]
    fn test_trust_prompt_no_queue_when_zero() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_trust_lines("review-code", &theme, 0);
        let line3: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!line3.contains("more queued"));
    }

    #[test]
    fn test_inspect_lines_basic() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_inspect_lines("test", "line1\nline2\nline3", 10, &theme);
        // header + 3 content lines + footer
        assert_eq!(lines.len(), 5);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("Inspect skill"));
        assert!(all_text.contains("line1"));
        assert!(all_text.contains("[Esc]"));
    }

    #[test]
    fn test_inspect_lines_truncation() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let content = (0..20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_inspect_lines("test", &content, 5, &theme);
        // header + 5 content + truncated notice + footer = 8
        assert_eq!(lines.len(), 8);
    }

    #[test]
    fn test_trust_prompt_height() {
        assert_eq!(trust_prompt_height(), 3);
    }
}
