//! ToolBlock widget — compact tool headers and inline errors on the Stage.

use ratatui::prelude::*;

use crate::adapters::daemon::protocol::ToolInputWithheld;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::clock::Clock;
use crate::domain::models::ToolCallInfo;

/// Map a status chip string to its theme color.
fn chip_color(chip: &str, theme: &Theme) -> ratatui::style::Color {
    match chip {
        "⋯ Validating" => theme.colors.tool_status_validating,
        "⧖ Scheduled" => theme.colors.tool_status_scheduled,
        "? Awaiting approval" => theme.colors.tool_status_awaiting,
        "● Executing" => theme.colors.tool_status_executing,
        "✓ Success" => theme.colors.tool_status_success,
        "✗ Error" => theme.colors.tool_status_error,
        "⊘ Cancelled" => theme.colors.tool_status_cancelled,
        _ => theme.colors.fg_muted,
    }
}

/// Extract a short summary from tool input for display.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            // Display summary only: a 60-cell budget with the `...` marker
            // inside it. The raw command stays untouched in `tc.input`.
            .map(|s| geom::ellipsize(s, 60, "...").into_owned())
            .unwrap_or_default(),
        "Read" | "read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Write" | "write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => format!("{}", input),
    }
}

/// Compute elapsed time string.
fn elapsed_str(tc: &ToolCallInfo, clock: &dyn Clock) -> String {
    let start = tc.started_at_ms.unwrap_or(0);
    if start == 0 {
        return String::new();
    }

    let end = tc
        .completed_at_ms
        .unwrap_or_else(|| clock.wall_now_ms().max(0) as u64);

    let elapsed_secs = (end.saturating_sub(start)) as f64 / 1000.0;
    format!("{:.1}s", elapsed_secs)
}

/// Project a tool name for display at the render boundary.
///
/// MCP tools (`mcp__<server>__<tool>`) render as `[server] tool`.
/// Built-in tool names are returned unchanged (zero-allocation `Cow::Borrowed`).
pub fn display_tool_name(name: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__") {
            return std::borrow::Cow::Owned(format!("[{server}] {tool}"));
        }
    }
    std::borrow::Cow::Borrowed(name)
}

/// Cached display name for reuse across render ticks.
/// Use this when the same name is rendered multiple times in one frame.
pub fn display_tool_name_cached(name: &str, cache: &mut Option<String>) -> String {
    if cache.is_none() {
        *cache = Some(display_tool_name(name).into_owned());
    }
    cache.as_ref().unwrap().clone()
}

/// Compute the rendered height of a compact tool block.
pub fn tool_block_height(tc: &ToolCallInfo) -> usize {
    if let Some(result) = &tc.result
        && result.is_error
    {
        return 1 + result.content.lines().count().max(1);
    }
    1
}

/// Render a tool block into lines for the chat pane.
/// Returns a Vec of Line objects to be appended to the rendered output.
pub fn render_tool_block_lines<'a>(
    tc: &ToolCallInfo,
    theme: &'a Theme,
    width: u16,
    clock: &dyn Clock,
) -> Vec<Line<'a>> {
    render_tool_block_lines_attached(tc, theme, width, clock, None)
}

/// Render attached tool input using the host's typed withholding reason.
/// Standalone callers continue through `render_tool_block_lines` unchanged.
pub fn render_tool_block_lines_attached<'a>(
    tc: &ToolCallInfo,
    theme: &'a Theme,
    _width: u16,
    clock: &dyn Clock,
    withheld: Option<&ToolInputWithheld>,
) -> Vec<Line<'a>> {
    let summary = withheld.map_or_else(|| tool_summary(&tc.name, &tc.input), |w| w.notice());
    let elapsed = elapsed_str(tc, clock);

    match &tc.result {
        None => {
            // Executing state
            let mut spans = vec![Span::styled(
                "┄ ",
                Style::default().fg(theme.colors.tool_border_collapsed),
            )];
            if let Some(ref chip) = tc.status {
                spans.push(Span::styled(
                    format!("{} ", chip),
                    Style::default().fg(chip_color(chip, theme)),
                ));
            }
            spans.extend(vec![
                Span::styled(
                    display_tool_name(&tc.name).into_owned(),
                    Style::default()
                        .fg(theme.colors.tool_name)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" \"{}\"", summary),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
                Span::styled(
                    format!(" → running... ({})", elapsed),
                    Style::default().fg(theme.colors.fg_muted),
                ),
                Span::styled(
                    " ┄",
                    Style::default().fg(theme.colors.tool_border_collapsed),
                ),
            ]);
            vec![Line::from(spans)]
        }
        Some(result) if result.is_error => {
            // Error state
            let mut header = vec![
                Span::styled(
                    "┃ ",
                    Style::default()
                        .fg(theme.colors.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "✗ ",
                    Style::default()
                        .fg(theme.colors.error)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(ref chip) = tc.status {
                header.push(Span::styled(
                    format!("{} ", chip),
                    Style::default().fg(chip_color(chip, theme)),
                ));
            }
            header.extend(vec![
                Span::styled(
                    display_tool_name(&tc.name).into_owned(),
                    Style::default()
                        .fg(theme.colors.tool_name)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" \"{}\"", summary),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
                Span::styled(
                    format!(" ({}) ┃", elapsed),
                    Style::default().fg(theme.colors.fg_muted),
                ),
            ]);
            let mut lines = vec![Line::from(header)];
            // Error message lines
            for err_line in result.content.lines() {
                lines.push(Line::from(vec![
                    Span::styled(
                        "┃ ",
                        Style::default()
                            .fg(theme.colors.error)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        err_line.to_string(),
                        Style::default().fg(theme.colors.error),
                    ),
                ]));
            }
            lines
        }
        Some(_) => {
            let mut spans = vec![Span::styled(
                "┄ ",
                Style::default().fg(theme.colors.tool_border_collapsed),
            )];
            if let Some(ref chip) = tc.status {
                spans.push(Span::styled(
                    format!("{} ", chip),
                    Style::default().fg(chip_color(chip, theme)),
                ));
            }
            spans.extend(vec![
                Span::styled(
                    display_tool_name(&tc.name).into_owned(),
                    Style::default()
                        .fg(theme.colors.tool_name)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" \"{}\"", summary),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
                Span::styled(" → ", Style::default().fg(theme.colors.fg_muted)),
                Span::styled(
                    "✓",
                    Style::default()
                        .fg(theme.colors.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" ({})", elapsed),
                    Style::default().fg(theme.colors.fg_muted),
                ),
                Span::styled(
                    " ┄",
                    Style::default().fg(theme.colors.tool_border_collapsed),
                ),
            ]);
            vec![Line::from(spans)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::clock::MockClock;
    use crate::domain::models::{ToolResultInfo, WriteDiffState, display_diff};

    fn test_clock() -> MockClock {
        MockClock::at_wall_ms(0)
    }

    fn make_tool_call(name: &str, result: Option<ToolResultInfo>) -> ToolCallInfo {
        let completed = result.as_ref().map(|_| 1002300u64);
        ToolCallInfo {
            id: "test_id".to_string(),
            name: name.to_string(),
            input: serde_json::json!({"command": "ls -la"}),
            result,
            started_at_ms: Some(1000000),
            completed_at_ms: completed,
            status: None,
            provider_id: None,
        }
    }

    #[test]
    fn attached_withheld_and_too_large_replace_input_in_every_block_state() {
        let theme = Theme::dark();
        let clock = test_clock();
        for withheld in [
            ToolInputWithheld::PeerTier,
            ToolInputWithheld::TooLarge { bytes: 2_097_152 },
        ] {
            for result in [
                None,
                Some(ToolResultInfo {
                    content: "output".into(),
                    is_error: false,
                    diff: WriteDiffState::NotAWrite,
                    images: Vec::new(),
                }),
                Some(ToolResultInfo {
                    content: "failed".into(),
                    is_error: true,
                    diff: WriteDiffState::NotAWrite,
                    images: Vec::new(),
                }),
            ] {
                let mut tc = make_tool_call("Bash", result);
                tc.input = serde_json::json!({"command": "secret-command"});
                let lines =
                    render_tool_block_lines_attached(&tc, &theme, 120, &clock, Some(&withheld));
                let text = lines
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains(&withheld.notice()), "{text}");
                assert!(!text.contains("secret-command"), "{text}");
                assert_eq!(lines.len(), tool_block_height(&tc),);
                let trusted = render_tool_block_lines_attached(&tc, &theme, 120, &clock, None);
                assert!(
                    trusted
                        .iter()
                        .any(|line| line.to_string().contains("secret-command"))
                );
                assert_eq!(trusted, render_tool_block_lines(&tc, &theme, 120, &clock),);
            }
        }
    }

    #[test]
    fn attached_withheld_does_not_reconstruct_diff_from_arguments() {
        let mut tc = make_tool_call(
            "Edit",
            Some(ToolResultInfo {
                content: "edited".into(),
                is_error: false,
                diff: WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        tc.input = serde_json::json!({
            "old_string": "secret-old",
            "new_string": "secret-new",
        });
        let theme = Theme::dark();
        let clock = test_clock();
        let withheld = ToolInputWithheld::PeerTier;
        let lines = render_tool_block_lines_attached(&tc, &theme, 120, &clock, Some(&withheld));
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&withheld.notice()));
        assert!(!text.contains("edited"));
        assert!(!text.contains("secret-old"));
        assert!(!text.contains("secret-new"));
        assert_eq!(lines.len(), tool_block_height(&tc));
        let trusted = render_tool_block_lines(&tc, &theme, 120, &clock);
        assert!(
            trusted
                .iter()
                .any(|line| line.to_string().contains("secret-old"))
        );
        assert!(
            trusted
                .iter()
                .any(|line| line.to_string().contains("secret-new"))
        );
    }

    #[test]
    fn attached_withheld_write_keeps_only_compact_header() {
        let mut tc = make_tool_call(
            "Write",
            Some(ToolResultInfo {
                content: "write completed".into(),
                is_error: false,
                diff: WriteDiffState::Diff {
                    lines: display_diff("before", "after"),
                    more: 0,
                },
                images: Vec::new(),
            }),
        );
        tc.input = serde_json::json!({"content": "secret-input"});
        let theme = Theme::dark();
        let clock = test_clock();
        let withheld = ToolInputWithheld::PeerTier;
        let lines = render_tool_block_lines_attached(&tc, &theme, 120, &clock, Some(&withheld));
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&withheld.notice()));
        assert!(!text.contains("before"));
        assert!(!text.contains("after"));
        assert!(!text.contains("secret-input"));
        assert_eq!(lines.len(), tool_block_height(&tc));
    }

    #[test]
    fn test_tool_block_height_executing() {
        let tc = make_tool_call("Bash", None);
        assert_eq!(tool_block_height(&tc), 1);
    }

    #[test]
    fn test_tool_block_height_collapsed_success() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "hello\nworld".to_string(),
                is_error: false,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        assert_eq!(tool_block_height(&tc), 1);
    }

    #[test]
    fn test_tool_block_height_multiline_success_stays_compact() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "line1\nline2\nline3".to_string(),
                is_error: false,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        assert_eq!(tool_block_height(&tc), 1);
    }

    #[test]
    fn test_tool_block_height_empty_success_stays_compact() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: String::new(),
                is_error: false,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        assert_eq!(tool_block_height(&tc), 1);
    }

    #[test]
    fn test_tool_block_height_error() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "error msg".to_string(),
                is_error: true,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        assert_eq!(tool_block_height(&tc), 2);
    }

    #[test]
    fn test_render_executing_state() {
        let tc = make_tool_call("Bash", None);
        let theme = crate::adapters::tui::theme::Theme::dark();
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, 80, &clock);
        assert_eq!(lines.len(), 1);
        let line_str: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_str.contains("Bash"));
        assert!(line_str.contains("running..."));
    }

    #[test]
    fn test_render_collapsed_success() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "output".to_string(),
                is_error: false,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, 80, &clock);
        assert_eq!(lines.len(), 1);
        let line_str: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_str.contains("✓"));
    }

    #[test]
    fn test_render_success_never_displays_output_inline() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "output line".to_string(),
                is_error: false,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, 80, &clock);
        assert_eq!(lines.len(), 1);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!first.contains("output line"));
        assert!(first.contains("✓"));
    }

    #[test]
    fn test_render_error_state() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "command not found".to_string(),
                is_error: true,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, 80, &clock);
        assert!(lines.len() >= 2);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("✗"));
    }

    #[test]
    fn test_tool_summary_bash() {
        let summary = tool_summary("Bash", &serde_json::json!({"command": "ls -la"}));
        assert_eq!(summary, "ls -la");
    }

    #[test]
    fn test_tool_summary_truncation() {
        let long_cmd = "a".repeat(100);
        let summary = tool_summary("Bash", &serde_json::json!({"command": long_cmd}));
        assert!(summary.len() <= 63);
        assert!(summary.ends_with("..."));
    }

    fn make_write_edit_call(
        name: &str,
        input: serde_json::Value,
        diff: WriteDiffState,
    ) -> ToolCallInfo {
        ToolCallInfo {
            id: "test_id".to_string(),
            name: name.to_string(),
            input,
            result: Some(ToolResultInfo {
                content: "Successfully wrote 4 bytes to f.rs".to_string(),
                is_error: false,
                diff,
                images: Vec::new(),
            }),
            started_at_ms: Some(1000000),
            completed_at_ms: Some(1002300),
            status: None,
            provider_id: None,
        }
    }

    /// Compact height/render lockstep for diff-bearing and ordinary tools.
    #[test]
    fn height_matches_render_for_write_and_other_tools() {
        let clock = test_clock();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let cases = vec![
            make_write_edit_call(
                "Write",
                serde_json::json!({"file_path": "f.rs", "content": "a\nb\nc"}),
                WriteDiffState::from_original(b"a\nx\nc", "a\nb\nc"),
            ),
            make_write_edit_call(
                "Edit",
                serde_json::json!({
                    "file_path": "f.rs",
                    "old_string": "a",
                    "new_string": "b"
                }),
                WriteDiffState::NotAWrite,
            ),
            make_tool_call(
                "Bash",
                Some(ToolResultInfo {
                    content: "one\ntwo".to_string(),
                    is_error: false,
                    diff: crate::domain::models::WriteDiffState::NotAWrite,
                    images: Vec::new(),
                }),
            ),
        ];
        for tc in cases {
            let rendered = render_tool_block_lines(&tc, &theme, 80, &clock);
            assert_eq!(
                tool_block_height(&tc),
                rendered.len(),
                "height/render lockstep for {}",
                tc.name
            );
        }
    }
}
