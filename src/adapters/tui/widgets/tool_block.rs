//! ToolBlock widget — renders tool calls in collapsed/expanded/error states.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::adapters::tui::theme::Theme;
use crate::domain::models::ToolCallInfo;

/// Per-tool-block UI state (not domain state).
#[derive(Debug, Clone)]
pub struct ToolBlockState {
    pub collapsed: bool,
    pub peek_active: bool,
}

impl Default for ToolBlockState {
    fn default() -> Self {
        Self {
            collapsed: true,
            peek_active: false,
        }
    }
}

/// Extract a short summary from tool input for display.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| {
                if s.chars().count() > 60 {
                    let truncated: String = s.chars().take(57).collect();
                    format!("{}...", truncated)
                } else {
                    s.to_string()
                }
            })
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
fn elapsed_str(tc: &ToolCallInfo) -> String {
    let start = tc.started_at_ms.unwrap_or(0);
    if start == 0 {
        return String::new();
    }

    let end = tc.completed_at_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    });

    let elapsed_secs = (end.saturating_sub(start)) as f64 / 1000.0;
    format!("{:.1}s", elapsed_secs)
}

/// Compute the rendered height of a tool block.
pub fn tool_block_height(tc: &ToolCallInfo, state: &ToolBlockState) -> usize {
    if let Some(ref result) = tc.result {
        if result.is_error {
            // Error: 1 line for header + error lines
            let error_lines = result.content.lines().count().max(1);
            return 1 + error_lines;
        }
        if state.collapsed {
            1 // One-line collapsed summary
        } else {
            // Expanded: border top + input line + output lines + border bottom
            let output_lines = result.content.lines().count();
            3 + output_lines // top border + input line + output + bottom border
        }
    } else {
        1 // Executing — one-line with ticker
    }
}

/// Render a tool block into lines for the chat pane.
/// Returns a Vec of Line objects to be appended to the rendered output.
pub fn render_tool_block_lines<'a>(
    tc: &ToolCallInfo,
    theme: &'a Theme,
    state: &ToolBlockState,
    width: u16,
) -> Vec<Line<'a>> {
    let summary = tool_summary(&tc.name, &tc.input);
    let elapsed = elapsed_str(tc);

    match &tc.result {
        None => {
            // Executing state
            let line = Line::from(vec![
                Span::styled(
                    "┄ ",
                    Style::default().fg(theme.colors.tool_border_collapsed),
                ),
                Span::styled(
                    tc.name.clone(),
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
            vec![line]
        }
        Some(result) if result.is_error => {
            // Error state
            let mut lines = vec![Line::from(vec![
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
                Span::styled(
                    tc.name.clone(),
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
            ])];
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
        Some(result) => {
            if state.collapsed {
                // Collapsed success
                let line = Line::from(vec![
                    Span::styled(
                        "┄ ",
                        Style::default().fg(theme.colors.tool_border_collapsed),
                    ),
                    Span::styled(
                        tc.name.clone(),
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
                vec![line]
            } else {
                // Expanded success
                let w = width as usize;
                let border_char = "─";
                let header = format!("─ {} ", tc.name);
                let trailer = format!(" ✓ ({}) ", elapsed);
                let fill_len = w.saturating_sub(header.len() + trailer.len() + 2);
                let fill = border_char.repeat(fill_len);

                let mut lines = Vec::new();

                // Top border with tool name
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("┌{}", header),
                        Style::default().fg(theme.colors.tool_border_expanded),
                    ),
                    Span::styled(
                        fill.clone(),
                        Style::default().fg(theme.colors.tool_border_expanded),
                    ),
                    Span::styled(
                        format!("{}┐", trailer),
                        Style::default().fg(theme.colors.fg_muted),
                    ),
                ]));

                // Input line
                let input_summary = match tc.name.as_str() {
                    "Bash" | "bash" => {
                        format!(
                            "$ {}",
                            tc.input
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                        )
                    }
                    _ => format!("{}", tc.input),
                };
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.colors.tool_border_expanded)),
                    Span::styled(input_summary, Style::default().fg(theme.colors.fg_primary)),
                ]));

                // Output lines
                for out_line in result.content.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(theme.colors.tool_border_expanded)),
                        Span::styled(
                            out_line.to_string(),
                            Style::default().fg(theme.colors.fg_secondary),
                        ),
                    ]));
                }

                // Bottom border
                let bottom_fill = border_char.repeat(w.saturating_sub(2));
                lines.push(Line::from(Span::styled(
                    format!("└{}┘", bottom_fill),
                    Style::default().fg(theme.colors.tool_border_expanded),
                )));

                lines
            }
        }
    }
}

/// Render a peek overlay for a tool block.
/// Returns a Paragraph widget positioned as a floating overlay.
pub fn render_peek_overlay<'a>(
    tc: &'a ToolCallInfo,
    theme: &'a Theme,
    viewport: Rect,
) -> (Paragraph<'a>, Rect) {
    let content = tc
        .result
        .as_ref()
        .map(|r| r.content.as_str())
        .unwrap_or("(no output)");

    let content_lines: Vec<&str> = content.lines().collect();
    let width = (80).min(viewport.width.saturating_sub(4));
    let height = (content_lines.len() as u16 + 2).min(viewport.height.saturating_sub(4));

    let x = (viewport.width.saturating_sub(width)) / 2 + viewport.x;
    let y = (viewport.height.saturating_sub(height)) / 2 + viewport.y;

    let area = Rect::new(x, y, width, height);

    let text: Vec<Line> = content_lines
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(theme.colors.fg_primary),
            ))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.info_border))
        .title(Span::styled(
            format!(" {} ", tc.name),
            Style::default()
                .fg(theme.colors.tool_name)
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(text).block(block);

    (paragraph, area)
}

/// Compute a simple line-by-line diff for Write tool display.
/// Uses longest common subsequence (LCS) to find additions and deletions.
pub fn compute_diff(original: &str, new_content: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    if old_lines.is_empty() {
        // New file — all lines are additions
        return new_lines
            .iter()
            .map(|l| DiffLine {
                kind: DiffKind::Added,
                content: l.to_string(),
            })
            .collect();
    }

    // Simple LCS diff
    let m = old_lines.len();
    let n = new_lines.len();

    // Size guard: LCS is O(m*n) space. For large files, fall back to all-removed + all-added.
    if m.saturating_mul(n) > 100_000 {
        let mut result = Vec::with_capacity(m + n);
        for line in &old_lines {
            result.push(DiffLine {
                kind: DiffKind::Removed,
                content: line.to_string(),
            });
        }
        for line in &new_lines {
            result.push(DiffLine {
                kind: DiffKind::Added,
                content: line.to_string(),
            });
        }
        return result;
    }

    // Build LCS table
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if old_lines[i - 1] == new_lines[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to produce diff
    let mut result = Vec::new();
    let mut i = m;
    let mut j = n;

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_lines[i - 1] == new_lines[j - 1] {
            result.push(DiffLine {
                kind: DiffKind::Context,
                content: old_lines[i - 1].to_string(),
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            result.push(DiffLine {
                kind: DiffKind::Added,
                content: new_lines[j - 1].to_string(),
            });
            j -= 1;
        } else {
            result.push(DiffLine {
                kind: DiffKind::Removed,
                content: old_lines[i - 1].to_string(),
            });
            i -= 1;
        }
    }

    result.reverse();
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub content: String,
}

/// Render diff lines with coloring.
pub fn render_diff_lines<'a>(diff: &[DiffLine], theme: &'a Theme) -> Vec<Line<'a>> {
    diff.iter()
        .map(|dl| {
            let (prefix, color) = match dl.kind {
                DiffKind::Added => ("+", theme.colors.success),
                DiffKind::Removed => ("-", theme.colors.error),
                DiffKind::Context => (" ", theme.colors.fg_secondary),
            };
            Line::from(Span::styled(
                format!("{} {}", prefix, dl.content),
                Style::default().fg(color),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ToolResultInfo;

    fn make_tool_call(name: &str, result: Option<ToolResultInfo>) -> ToolCallInfo {
        let completed = result.as_ref().map(|_| 1002300u64);
        ToolCallInfo {
            id: "test_id".to_string(),
            name: name.to_string(),
            input: serde_json::json!({"command": "ls -la"}),
            result,
            started_at_ms: Some(1000000),
            completed_at_ms: completed,
        }
    }

    #[test]
    fn test_tool_block_height_executing() {
        let tc = make_tool_call("Bash", None);
        let state = ToolBlockState::default();
        assert_eq!(tool_block_height(&tc, &state), 1);
    }

    #[test]
    fn test_tool_block_height_collapsed_success() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "hello\nworld".to_string(),
                is_error: false,
            }),
        );
        let state = ToolBlockState::default();
        assert_eq!(tool_block_height(&tc, &state), 1);
    }

    #[test]
    fn test_tool_block_height_expanded_success() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "line1\nline2\nline3".to_string(),
                is_error: false,
            }),
        );
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        // 3 (header+input+bottom) + 3 output lines = 6
        assert_eq!(tool_block_height(&tc, &state), 6);
    }

    #[test]
    fn test_tool_block_height_expanded_empty_content() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: String::new(),
                is_error: false,
            }),
        );
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        // 3 (header+input+bottom) + 0 output lines = 3
        assert_eq!(tool_block_height(&tc, &state), 3);
    }

    #[test]
    fn test_tool_block_height_error() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "error msg".to_string(),
                is_error: true,
            }),
        );
        let state = ToolBlockState::default();
        assert_eq!(tool_block_height(&tc, &state), 2);
    }

    #[test]
    fn test_render_executing_state() {
        let tc = make_tool_call("Bash", None);
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState::default();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80);
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
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState::default();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80);
        assert_eq!(lines.len(), 1);
        let line_str: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_str.contains("✓"));
    }

    #[test]
    fn test_render_expanded_success() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "output line".to_string(),
                is_error: false,
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        let lines = render_tool_block_lines(&tc, &theme, &state, 80);
        assert!(lines.len() > 1);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("┌"));
    }

    #[test]
    fn test_render_error_state() {
        let tc = make_tool_call(
            "Bash",
            Some(ToolResultInfo {
                content: "command not found".to_string(),
                is_error: true,
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState::default();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80);
        assert!(lines.len() >= 2);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("✗"));
    }

    #[test]
    fn test_diff_new_file() {
        let diff = compute_diff("", "line1\nline2");
        assert_eq!(diff.len(), 2);
        assert!(diff.iter().all(|d| d.kind == DiffKind::Added));
    }

    #[test]
    fn test_diff_modification() {
        let diff = compute_diff(
            "fn main() {\n    println!(\"hello\");\n}",
            "fn main() {\n    println!(\"hello world\");\n    println!(\"goodbye\");\n}",
        );
        assert!(diff.iter().any(|d| d.kind == DiffKind::Added));
        assert!(diff.iter().any(|d| d.kind == DiffKind::Context));
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
}
