//! ToolBlock widget — renders tool calls in collapsed/expanded/error states.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::adapters::tui::theme::Theme;
use crate::domain::clock::Clock;
use crate::domain::models::{
    DIFF_MAX_LINES, DiffKind, DiffLine, NotCapturedReason, ToolCallInfo, WriteDiffState,
    diff::cap_lines, display_diff,
};

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

/// The body an expanded Write/Edit success block shows INSTEAD of the raw
/// result-content lines (Story 19.1). `None` (from [`expanded_diff_body`])
/// means "ordinary tool" — render the output lines as before.
enum DiffBody {
    /// Diff lines, already elided and capped at `DIFF_MAX_LINES`. The
    /// `… N more lines` marker is inlined by `cap_lines`, so this is the
    /// complete body — the widget adds nothing to it.
    Lines { lines: Vec<DiffLine> },
    /// A4 honest one-liner — the write completed but the original content was
    /// not captured. `reason` names WHY, so the block can never claim "no
    /// active checkpoint" for a snapshot read failure.
    NotCaptured {
        bytes: usize,
        reason: NotCapturedReason,
    },
    /// The write completed and changed nothing a line diff can show.
    NoChange,
}

/// Derive the expanded-block diff body for a completed, non-error tool
/// call (Story 19.1 A2/A3/A4). Pure function of `tc` — no I/O.
///
/// - **Edit** renders a hunk from its own input (`old_string` →
///   `new_string`) — zero plumbing (A2). Memoized, because this function is
///   called from the layout path on every frame.
/// - **Write** renders the state the infrastructure site computed. Every
///   variant is explicit: there is no value that means two things.
fn expanded_diff_body(tc: &ToolCallInfo) -> Option<DiffBody> {
    let result = tc.result.as_ref()?;
    if result.is_error {
        return None;
    }
    match tc.name.as_str() {
        "Edit" | "edit" => {
            let old = tc.input.get("old_string").and_then(|v| v.as_str())?;
            let new = tc.input.get("new_string").and_then(|v| v.as_str())?;
            // Both are mandatory and must differ (the adapter rejects
            // anything else); malformed input falls back to output lines.
            if old.is_empty() || old == new {
                return None;
            }
            let (lines, _more) = edit_hunk(old, new);
            if lines.is_empty() {
                return Some(DiffBody::NoChange);
            }
            Some(DiffBody::Lines { lines })
        }
        "Write" | "write" => match &result.diff {
            // A Write cannot legitimately be NotAWrite; the site only emits
            // it when the input was unreadable. Fall back to output text
            // rather than invent a diff.
            WriteDiffState::NotAWrite => None,
            WriteDiffState::NewFile => {
                let content = tc.input.get("content").and_then(|v| v.as_str())?;
                let (lines, _more) = cap_lines(display_diff("", content), DIFF_MAX_LINES);
                if lines.is_empty() {
                    return Some(DiffBody::NoChange);
                }
                Some(DiffBody::Lines { lines })
            }
            WriteDiffState::Diff { lines, more } if lines.is_empty() && *more == 0 => {
                Some(DiffBody::NoChange)
            }
            WriteDiffState::Diff { lines, .. } => Some(DiffBody::Lines {
                lines: lines.clone(),
            }),
            WriteDiffState::NotCaptured { reason } => Some(DiffBody::NotCaptured {
                bytes: tc
                    .input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map_or(0, str::len),
                reason: *reason,
            }),
        },
        _ => None,
    }
}

/// Memoized Edit hunk. `expanded_diff_body` is reached from
/// [`tool_block_height`], which the chat pane calls for EVERY tool block on
/// EVERY frame; computing an LCS there made frame cost scale with the size of
/// every visible edit. The cache is keyed by the input strings' hash and is
/// bounded — the TUI renders on one thread, so a thread-local is enough.
fn edit_hunk(old: &str, new: &str) -> (Vec<DiffLine>, usize) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};

    const MEMO_CAP: usize = 256;
    thread_local! {
        static MEMO: RefCell<HashMap<u64, (Vec<DiffLine>, usize)>> =
            RefCell::new(HashMap::new());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    old.hash(&mut hasher);
    new.hash(&mut hasher);
    let key = hasher.finish();

    MEMO.with(|memo| {
        if let Some(hit) = memo.borrow().get(&key) {
            return hit.clone();
        }
        let computed = cap_lines(display_diff(old, new), DIFF_MAX_LINES);
        let mut m = memo.borrow_mut();
        if m.len() >= MEMO_CAP {
            m.clear();
        }
        m.insert(key, computed.clone());
        computed
    })
}

/// Height of the expanded body (diff lines + cap marker, A4 line, or the
/// ordinary output lines). Keeps [`tool_block_height`] in lockstep with
/// [`render_tool_block_lines`].
fn expanded_body_height(tc: &ToolCallInfo) -> usize {
    match expanded_diff_body(tc) {
        Some(DiffBody::Lines { lines }) => lines.len(),
        Some(DiffBody::NotCaptured { .. }) | Some(DiffBody::NoChange) => 1,
        None => tc.result.as_ref().map_or(0, |r| r.content.lines().count()),
    }
}

/// Compute the rendered height of a tool block.
pub fn tool_block_height(tc: &ToolCallInfo, state: &ToolBlockState) -> usize {
    if let Some(result) = &tc.result {
        if result.is_error {
            // Error: 1 line for header + error lines
            let error_lines = result.content.lines().count().max(1);
            return 1 + error_lines;
        }
        if state.collapsed {
            1 // One-line collapsed summary
        } else {
            // Expanded: border top + input line + body + border bottom
            3 + expanded_body_height(tc)
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
    clock: &dyn Clock,
) -> Vec<Line<'a>> {
    let summary = tool_summary(&tc.name, &tc.input);
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
        Some(result) => {
            if state.collapsed {
                // Collapsed success
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
            } else {
                // Expanded success
                let w = width as usize;
                let border_char = "─";
                let header = format!("─ {} ", display_tool_name(&tc.name));
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

                // Body: for Write/Edit the inline diff replaces the raw
                // result-content lines (Story 19.1 — FR30's "see what the
                // tool actually changed" instead of "Successfully wrote N
                // bytes"). Diff lines deliberately carry NO "│ " gutter so
                // they never share a prefix with output-tail lines (A8).
                match expanded_diff_body(tc) {
                    Some(DiffBody::Lines { lines: dls }) => {
                        lines.extend(render_diff_lines(&dls, theme));
                    }
                    Some(DiffBody::NotCaptured { bytes, reason }) => {
                        // "overwrote" is only true when we know a file was
                        // replaced, which is exactly the no-checkpoint case
                        // AC4/A4 pin. For every other reason the provenance
                        // is unknown, so say "wrote" rather than assert an
                        // overwrite that may not have happened.
                        let verb = match reason {
                            NotCapturedReason::NoActiveCheckpoint => "overwrote",
                            _ => "wrote",
                        };
                        lines.push(Line::from(Span::styled(
                            format!(
                                "{} {} bytes — previous content not captured ({})",
                                verb,
                                bytes,
                                reason.describe()
                            ),
                            Style::default().fg(theme.colors.fg_muted),
                        )));
                    }
                    Some(DiffBody::NoChange) => {
                        lines.push(Line::from(Span::styled(
                            "no line changes — the file content is unchanged",
                            Style::default().fg(theme.colors.fg_muted),
                        )));
                    }
                    None => {
                        for out_line in result.content.lines() {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    "│ ",
                                    Style::default().fg(theme.colors.tool_border_expanded),
                                ),
                                Span::styled(
                                    out_line.to_string(),
                                    Style::default().fg(theme.colors.fg_secondary),
                                ),
                            ]));
                        }
                    }
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
            format!(" {} ", display_tool_name(&tc.name)),
            Style::default()
                .fg(theme.colors.tool_name)
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(text).block(block);

    (paragraph, area)
}

/// Render diff lines with coloring.
///
/// Diff rows deliberately carry no `│ ` gutter so a matcher can never confuse
/// them with output-tail lines (ruling A8). `Elided` rows carry their own
/// text and take no `+`/`-`/` ` prefix, so they cannot be mistaken for
/// content either.
pub fn render_diff_lines<'a>(diff: &[DiffLine], theme: &'a Theme) -> Vec<Line<'a>> {
    diff.iter()
        .map(|dl| {
            let (prefix, color) = match dl.kind {
                DiffKind::Added => ("+", theme.colors.success),
                DiffKind::Removed => ("-", theme.colors.error),
                DiffKind::Context => (" ", theme.colors.fg_secondary),
                DiffKind::Elided => {
                    return Line::from(Span::styled(
                        dl.content.clone(),
                        Style::default().fg(theme.colors.fg_muted),
                    ));
                }
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
    use crate::domain::clock::MockClock;
    use crate::domain::models::ToolResultInfo;

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
                diff: crate::domain::models::WriteDiffState::NotAWrite,
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
                diff: crate::domain::models::WriteDiffState::NotAWrite,
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
                diff: crate::domain::models::WriteDiffState::NotAWrite,
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
                diff: crate::domain::models::WriteDiffState::NotAWrite,
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
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80, &clock);
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
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState::default();
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80, &clock);
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
                diff: crate::domain::models::WriteDiffState::NotAWrite,
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80, &clock);
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
                diff: crate::domain::models::WriteDiffState::NotAWrite,
            }),
        );
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState::default();
        let clock = test_clock();
        let lines = render_tool_block_lines(&tc, &theme, &state, 80, &clock);
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
            }),
            started_at_ms: Some(1000000),
            completed_at_ms: Some(1002300),
            status: None,
        }
    }

    fn render_expanded(tc: &ToolCallInfo) -> (Vec<String>, crate::adapters::tui::theme::Theme) {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        let clock = test_clock();
        let lines = render_tool_block_lines(tc, &theme, &state, 80, &clock);
        let strings: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        (strings, theme)
    }

    /// AC1 — Edit renders a hunk from its own input, no plumbing: the
    /// lines contain `  a`, `- b`, `+ c` in that order, coloured
    /// fg_secondary / error / success, and the byte-count result text is
    /// replaced by the diff.
    #[test]
    fn ac1_edit_renders_hunk_from_own_input() {
        let tc = make_write_edit_call(
            "Edit",
            serde_json::json!({
                "file_path": "f.rs",
                "old_string": "a\nb",
                "new_string": "a\nc"
            }),
            WriteDiffState::NotAWrite,
        );
        let (lines, theme) = render_expanded(&tc);
        let pos: Vec<Option<usize>> = ["  a", "- b", "+ c"]
            .iter()
            .map(|needle| lines.iter().position(|l| l == needle))
            .collect();
        assert!(
            pos.iter().all(|p| p.is_some()),
            "all hunk lines present: {lines:?}"
        );
        let [pa, pb, pc] = [pos[0].unwrap(), pos[1].unwrap(), pos[2].unwrap()];
        assert!(pa < pb && pb < pc, "hunk order  a/- b/+ c: {lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("Successfully wrote")),
            "the diff replaces the byte-count result text: {lines:?}"
        );
        // Colours (AC1): context fg_secondary, removed error, added success.
        let rendered = render_tool_block_lines(
            &tc,
            &theme,
            &ToolBlockState {
                collapsed: false,
                peek_active: false,
            },
            80,
            &test_clock(),
        );
        let find_styled = |needle: &str| {
            rendered
                .iter()
                .find(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        == needle
                })
                .and_then(|l| l.spans.first())
                .map(|s| s.style.fg)
        };
        assert_eq!(find_styled("  a"), Some(Some(theme.colors.fg_secondary)));
        assert_eq!(find_styled("- b"), Some(Some(theme.colors.error)));
        assert_eq!(find_styled("+ c"), Some(Some(theme.colors.success)));
    }

    /// AC2 first half — Write to a file that did not exist, diff `None`:
    /// every line of `input.content` renders with `+` (input-derived path).
    #[test]
    fn ac2_write_new_file_renders_all_additions_from_input() {
        let tc = make_write_edit_call(
            "Write",
            serde_json::json!({
                "file_path": "new.rs",
                "content": "fn a() {}\nfn b() {}"
            }),
            WriteDiffState::NewFile,
        );
        let (lines, _theme) = render_expanded(&tc);
        assert!(lines.contains(&"+ fn a() {}".to_string()), "{lines:?}");
        assert!(lines.contains(&"+ fn b() {}".to_string()), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("- ")),
            "a new file has no removals: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("Successfully wrote")),
            "{lines:?}"
        );
    }

    /// AC2 second half — Write over an existing file with a site diff:
    /// renders `  x`, `- y`, `+ z`.
    #[test]
    fn ac2_write_overwrite_renders_site_diff() {
        let tc = make_write_edit_call(
            "Write",
            serde_json::json!({
                "file_path": "f.rs",
                "content": "x\nz"
            }),
            WriteDiffState::from_original(b"x\ny", "x\nz"),
        );
        let (lines, _theme) = render_expanded(&tc);
        assert!(lines.contains(&"  x".to_string()), "{lines:?}");
        assert!(lines.contains(&"- y".to_string()), "{lines:?}");
        assert!(lines.contains(&"+ z".to_string()), "{lines:?}");
    }

    /// AC4 — overwrite whose original was not captured (site marker
    /// `NotCaptured`): exactly one muted honest line, no `+` content line.
    /// Mutant: falling through to the all-additions branch turns this RED.
    #[test]
    fn ac4_overwrite_without_snapshot_renders_honest_line_only() {
        let tc = make_write_edit_call(
            "Write",
            serde_json::json!({
                "file_path": "f.rs",
                "content": "abc"
            }),
            WriteDiffState::NotCaptured {
                reason: NotCapturedReason::NoActiveCheckpoint,
            },
        );
        let (lines, theme) = render_expanded(&tc);
        assert_eq!(
            lines
                .iter()
                .find(|l| l.contains("previous content not captured")),
            Some(
                &"overwrote 3 bytes — previous content not captured (no active checkpoint)"
                    .to_string()
            ),
            "exactly the A4 line: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("+ ") || l.starts_with("- ")),
            "no diff content lines may be fabricated: {lines:?}"
        );
        // Muted colour.
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        let rendered = render_tool_block_lines(&tc, &theme, &state, 80, &test_clock());
        let honest = rendered
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("previous content not captured")
            })
            .expect("honest line rendered");
        assert_eq!(
            honest.spans.first().map(|s| s.style.fg),
            Some(Some(theme.colors.fg_muted))
        );
        // Height lockstep: 3 frame lines + 1 honest line.
        assert_eq!(tool_block_height(&tc, &state), 4);
        assert_eq!(rendered.len(), 4);
    }

    /// AC5 — a 500-line diff renders 200 lines then `… 300 more lines`;
    /// height stays in lockstep with render. Collapsed stays 1 line.
    #[test]
    fn ac5_expanded_diff_caps_at_200_lines() {
        let content: String = (0..500).map(|i| format!("line{i}\n")).collect();
        let tc = make_write_edit_call(
            "Write",
            serde_json::json!({
                "file_path": "big.rs",
                "content": content
            }),
            WriteDiffState::NewFile,
        );
        let (lines, _theme) = render_expanded(&tc);
        let added = lines.iter().filter(|l| l.starts_with("+ ")).count();
        assert_eq!(added, 200, "cap at DIFF_MAX_LINES: {lines:?}");
        assert!(lines.contains(&"… 300 more lines".to_string()), "{lines:?}");
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
        assert_eq!(
            tool_block_height(&tc, &state),
            lines.len(),
            "height must equal rendered line count"
        );
        assert_eq!(tool_block_height(&tc, &ToolBlockState::default()), 1);
    }

    /// Height/render lockstep for the site-diff and ordinary-tool bodies.
    #[test]
    fn height_matches_render_for_write_and_other_tools() {
        let state = ToolBlockState {
            collapsed: false,
            peek_active: false,
        };
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
                }),
            ),
        ];
        for tc in cases {
            let rendered = render_tool_block_lines(&tc, &theme, &state, 80, &clock);
            assert_eq!(
                tool_block_height(&tc, &state),
                rendered.len(),
                "height/render lockstep for {}",
                tc.name
            );
        }
    }
}
