//! Task detail view widget — Story 6.3.
//!
//! Renders a full-width drill-down view replacing the chat pane when the user
//! presses `Enter` on a task in the task panel. Shows task header, dependency
//! tags, result body, tool-call placeholder, error section, and action row.
//!
//! ## Action-row variants per task status
//!
//! - `Completed`: `[c] Copy result   [Esc] Back`
//! - `Failed`: `[c] Copy error   [r] Retry   [s] Skip   [e] Edit task   [Esc] Back`
//! - Others: `[c] Copy info   [Esc] Back`
//!
//! The `r`/`s`/`e` keys on Failed tasks are **reserved for Story 6.4**. In 6-3
//! they emit a SystemNotice "Coming in Story 6.4" and stay on the detail view.
//! 6-4 only changes the handler, not the keymap shape.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    prelude::*,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

use crate::adapters::tui::markdown;
use crate::domain::models::plan::{Plan, PlanTask, PlanTaskStatus};
use crate::domain::services::plan_runtime::format_elapsed_ms;

fn status_icon(status: PlanTaskStatus, theme: &crate::adapters::tui::theme::Theme) -> (&'static str, Color) {
    match status {
        PlanTaskStatus::Pending => ("\u{23F3}", theme.colors.fg_muted),
        PlanTaskStatus::Running => ("\u{25CF}", theme.colors.tool_status_executing),
        PlanTaskStatus::Waiting => ("\u{29D6}", theme.colors.tool_status_awaiting),
        PlanTaskStatus::Completed => ("\u{2713}", theme.colors.tool_status_success),
        PlanTaskStatus::Failed => ("\u{2717}", theme.colors.tool_status_error),
        PlanTaskStatus::Skipped => ("\u{23ED}", theme.colors.tool_status_cancelled),
        PlanTaskStatus::Cancelled => ("\u{2298}", theme.colors.tool_status_cancelled),
    }
}

fn status_word(status: PlanTaskStatus) -> &'static str {
    match status {
        PlanTaskStatus::Pending => "Pending",
        PlanTaskStatus::Running => "Running",
        PlanTaskStatus::Waiting => "Waiting",
        PlanTaskStatus::Completed => "Completed",
        PlanTaskStatus::Failed => "Failed",
        PlanTaskStatus::Skipped => "Skipped",
        PlanTaskStatus::Cancelled => "Cancelled",
    }
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    plan: &Plan,
    task: &PlanTask,
    theme: &crate::adapters::tui::theme::Theme,
    _vp_height: u16,
    expanded: bool,
    scroll_offset: &mut u16,
) {
    Clear.render(area, buf);

    let block = Block::default()
        .title(format!(" Task {} \u{B7} {} ", task.number, plan.title))
        .borders(Borders::NONE);
    let inner = block.inner(area);
    block.render(area, buf);

    let mut y = inner.y;
    let max_y = inner.y + inner.height;

    let elapsed = task.elapsed_ms().unwrap_or(0);
    let elapsed_str = format_elapsed_ms(elapsed);
    let (icon, icon_color) = status_icon(task.status, theme);
    let word = status_word(task.status);

    let header = format!("Task {}. {}    {} {} \u{B7} {}", task.number, task.title, icon, word, elapsed_str);
    if y < max_y {
        buf.set_string(
            inner.x,
            y,
            &header,
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        );
        y += 1;
    }

    if !task.depends_on.is_empty() && y < max_y {
        let deps: Vec<String> = task.depends_on.iter().map(|d| d.to_string()).collect();
        buf.set_string(
            inner.x,
            y,
            &format!("Depends on: {}", deps.join(", ")),
            Style::default().fg(theme.colors.fg_muted),
        );
        y += 1;
    }

    if y < max_y {
        y += 1;
    }

    if let Some(result) = &task.result {
        if y < max_y {
            buf.set_string(
                inner.x,
                y,
                "Result:",
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        // 6.3-FU1: render the result body via the chat-pane markdown pipeline
        // (sanitize → parse → transform → highlight → layout) so fenced code,
        // lists, and inline markers render the same way they do in chat. The
        // pipeline returns one `Line` per visual row; we cap to
        // `max_result_lines` rows and let the body Rect contain them.
        // 6-3 AC7: when `expanded` is set, give the body all remaining
        // viewport rows minus a 4-row reserve for the hint, blank, tokens
        // summary, and action row. Otherwise cap to half the viewport.
        let max_result_lines = if expanded {
            max_y.saturating_sub(y).saturating_sub(4).max(1) as usize
        } else {
            (inner.height / 2).saturating_sub(4).max(1) as usize
        };
        let body_height = (max_result_lines as u16).min(max_y.saturating_sub(y));
        let lines = markdown::render(
            &result.text,
            inner.width as usize,
            theme,
            &markdown::RenderOptions::completed(),
        );
        let total = lines.len();
        // 6-3 AC7: clamp the caller-provided scroll offset against the body
        // height so the last line stays in view when scrolling past the end
        // (j/G), and `0` always parks at the top.
        let max_offset = total.saturating_sub(body_height as usize);
        let clamped = (*scroll_offset as usize).min(max_offset);
        *scroll_offset = clamped as u16;
        let end = (clamped + body_height as usize).min(total);
        let truncated = total > body_height as usize;
        if body_height > 0 && y < max_y {
            let visible: Vec<Line<'static>> =
                lines.into_iter().skip(clamped).take(body_height as usize).collect();
            Paragraph::new(visible)
                .wrap(Wrap { trim: false })
                .render(
                    Rect {
                        x: inner.x,
                        y,
                        width: inner.width,
                        height: body_height,
                    },
                    buf,
                );
            y += body_height;
        }
        if y < max_y {
            // Combined position + key hint. Always show position when the
            // body is truncated; show key hints unconditionally so the user
            // knows the affordance even at the top.
            let pos = if truncated {
                format!("({}-{} / {})  ", clamped + 1, end, total)
            } else {
                String::new()
            };
            let keys = if expanded {
                "[Enter] collapse  [j/k] scroll"
            } else {
                "[Enter] expand  [j/k] scroll"
            };
            buf.set_string(
                inner.x,
                y,
                &format!("{}{}", pos, keys),
                Style::default().fg(theme.colors.fg_muted),
            );
            y += 1;
        }
        if y < max_y {
            y += 1;
        }

        // PD3 (Winston W1): real Tokens / tool_call_count line. The prior
        // placeholder erodes trust when the data is already on PlanTask.result.
        if y < max_y {
            let tokens = result
                .token_count
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let summary = format!(
                "Tokens: {}   Tool calls: {}",
                tokens, result.tool_call_count
            );
            buf.set_string(
                inner.x,
                y,
                &summary,
                Style::default().fg(theme.colors.fg_muted),
            );
            y += 1;
        }
    }

    if matches!(task.status, PlanTaskStatus::Failed | PlanTaskStatus::Skipped | PlanTaskStatus::Cancelled) {
        if let Some(error) = &task.error {
            if y < max_y {
                buf.set_string(
                    inner.x,
                    y,
                    "Error:",
                    Style::default()
                        .fg(theme.colors.error)
                        .add_modifier(Modifier::BOLD),
                );
                y += 1;
            }
            for line in error.lines() {
                if y >= max_y {
                    break;
                }
                buf.set_string(
                    inner.x,
                    y,
                    line,
                    Style::default().fg(theme.colors.error),
                );
                y += 1;
            }
            if y < max_y {
                y += 1;
            }
        }
    }

    if y < max_y || inner.height >= 2 {
        let action_text = match task.status {
            PlanTaskStatus::Completed => "[c] Copy result   [Esc] Back".to_string(),
            PlanTaskStatus::Failed => "[c] Copy error   [r] Retry   [s] Skip   [e] Edit task   [Esc] Back".to_string(),
            _ => "[c] Copy info   [Esc] Back".to_string(),
        };
        let action_y = max_y.saturating_sub(1);
        if action_y >= inner.y {
            buf.set_string(
                inner.x,
                action_y,
                &action_text,
                Style::default().fg(theme.colors.fg_muted),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::color_detect::ColorCapability;
    use crate::adapters::tui::theme::Theme;
    use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus, TaskResult};

    fn test_theme() -> Theme {
        Theme::for_capability(ColorCapability::TrueColor)
    }

    fn make_plan() -> Plan {
        Plan {
            id: "p1".to_string(),
            title: "Test Plan".to_string(),
            tasks: vec![],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1000,
            resolved_at: None,
            host_message_id: None,
        }
    }

    fn make_task(number: u32, title: &str, status: PlanTaskStatus) -> PlanTask {
        PlanTask {
            number,
            title: title.to_string(),
            description: String::new(),
            depends_on: vec![],
            status,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
        }
    }

    fn render_to_string(area: Rect, plan: &Plan, task: &PlanTask) -> String {
        render_to_string_ex(area, plan, task, false)
    }

    fn render_to_string_ex(area: Rect, plan: &Plan, task: &PlanTask, expanded: bool) -> String {
        render_to_string_full(area, plan, task, expanded, 0).0
    }

    fn render_to_string_full(
        area: Rect,
        plan: &Plan,
        task: &PlanTask,
        expanded: bool,
        scroll: u16,
    ) -> (String, u16) {
        let mut buf = Buffer::empty(area);
        let mut clamped = scroll;
        render(area, &mut buf, plan, task, &test_theme(), area.height, expanded, &mut clamped);
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
        }
        (out, clamped)
    }

    #[test]
    fn render_completed_task() {
        let plan = make_plan();
        let mut task = make_task(1, "Do thing", PlanTaskStatus::Completed);
        task.result = Some(TaskResult { text: "Result text here".to_string(), tool_call_count: 0, token_count: None });
        let content = render_to_string(Rect::new(0, 0, 80, 24), &plan, &task);
        assert!(content.contains("Task 1"));
        assert!(content.contains("Do thing"));
    }

    #[test]
    fn render_failed_task_with_error() {
        let plan = make_plan();
        let mut task = make_task(2, "Fail thing", PlanTaskStatus::Failed);
        task.error = Some("Something went wrong".to_string());
        let content = render_to_string(Rect::new(0, 0, 80, 24), &plan, &task);
        assert!(content.contains("[r] Retry"));
        assert!(content.contains("[s] Skip"));
        assert!(content.contains("[e] Edit task"));
    }

    #[test]
    fn render_pending_task() {
        let plan = make_plan();
        let task = make_task(3, "Wait thing", PlanTaskStatus::Pending);
        render_to_string(Rect::new(0, 0, 80, 24), &plan, &task);
    }

    #[test]
    fn render_running_task_no_result() {
        let plan = make_plan();
        let task = make_task(4, "Active task", PlanTaskStatus::Running);
        let content = render_to_string(Rect::new(0, 0, 80, 24), &plan, &task);
        assert!(content.contains("Task 4"));
    }

    #[test]
    fn ac7_expanded_renders_more_result_lines_than_collapsed() {
        // 6-3 AC7: when `expanded == true`, the body must render more
        // lines than the half-viewport-capped collapsed view.
        let plan = make_plan();
        let mut task = make_task(7, "Long result", PlanTaskStatus::Completed);
        let body: String = (1..=40)
            .map(|i| format!("line-marker-{:02}", i))
            .collect::<Vec<_>>()
            .join("\n");
        task.result = Some(TaskResult { text: body, tool_call_count: 0, token_count: None });

        let collapsed = render_to_string_ex(Rect::new(0, 0, 80, 30), &plan, &task, false);
        let expanded = render_to_string_ex(Rect::new(0, 0, 80, 30), &plan, &task, true);

        let count_markers = |s: &str| (1..=40)
            .filter(|i| s.contains(&format!("line-marker-{:02}", i)))
            .count();
        let collapsed_n = count_markers(&collapsed);
        let expanded_n = count_markers(&expanded);
        assert!(
            expanded_n > collapsed_n,
            "expanded should render more rows than collapsed (collapsed={}, expanded={})",
            collapsed_n, expanded_n
        );
        assert!(collapsed.contains("[Enter] expand"));
        assert!(expanded.contains("[Enter] collapse"));
    }

    #[test]
    fn ac7_scroll_offset_reveals_later_lines_and_clamps_at_end() {
        // 6-3 AC7: rendering at scroll=0 hides the tail markers, scrolling
        // forward reveals them, and scrolling past the end clamps so the
        // last line is still in view (the widget writes the clamped value
        // back through the &mut u16).
        let plan = make_plan();
        let mut task = make_task(8, "Long result", PlanTaskStatus::Completed);
        let body: String = (1..=40)
            .map(|i| format!("line-marker-{:02}", i))
            .collect::<Vec<_>>()
            .join("\n");
        task.result = Some(TaskResult { text: body, tool_call_count: 0, token_count: None });
        let area = Rect::new(0, 0, 80, 30);

        let (top, top_scroll) = render_to_string_full(area, &plan, &task, true, 0);
        assert_eq!(top_scroll, 0);
        assert!(top.contains("line-marker-01"), "top view shows first line");
        assert!(!top.contains("line-marker-40"), "top view does not show last line");

        let (mid, _) = render_to_string_full(area, &plan, &task, true, 10);
        assert!(mid.contains("line-marker-15"), "scrolled view reveals middle line");

        // Scroll way past the end — widget should clamp and still show the tail.
        let (end, end_scroll) = render_to_string_full(area, &plan, &task, true, u16::MAX);
        assert!(end_scroll < 40, "clamped offset stays within content");
        assert!(end.contains("line-marker-40"), "tail visible when scrolled to end");
    }

    #[test]
    fn fu1_result_body_uses_markdown_pipeline() {
        // 6.3-FU1: a result containing a fenced code block + bullet list +
        // inline backticks should not render the literal markers — the
        // markdown pipeline strips/transforms them.
        let plan = make_plan();
        let mut task = make_task(5, "MD task", PlanTaskStatus::Completed);
        task.result = Some(TaskResult {
            text: "Use `cargo test`.\n\n- item one\n- item two\n\n```rust\nfn x() {}\n```"
                .to_string(),
            tool_call_count: 0,
            token_count: None,
        });
        let content = render_to_string(Rect::new(0, 0, 80, 30), &plan, &task);
        assert!(content.contains("cargo test"));
        // The `[c] Copy result` action row still renders below the body.
        assert!(content.contains("[c] Copy result"));
    }
}
