use ratatui::prelude::*;
use ratatui::widgets::BorderType;

use crate::adapters::tui::keymap::render::leader_follow_up_compact;
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::text_geometry as geom;
use crate::domain::models::plan::{Plan, PlanStatus, PlanSubTask, PlanTaskStatus};
use crate::domain::services::plan_runtime::format_elapsed_ms;

/// Render a single sub-task line indented inside the plan card border.
fn render_sub_task_card_line(
    sub: &PlanSubTask,
    theme: &crate::adapters::tui::theme::Theme,
    vertical_char: &str,
    inner_width: usize,
    is_pending: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let (icon_str, icon_color) = if !is_pending && sub.started_at_ms.is_some() {
        match sub.status {
            PlanTaskStatus::Pending => (String::new(), theme.colors.fg_muted),
            PlanTaskStatus::Running => ("●".to_string(), theme.colors.tool_status_executing),
            PlanTaskStatus::Waiting => ("⧖".to_string(), theme.colors.tool_status_awaiting),
            PlanTaskStatus::Completed => ("✓".to_string(), theme.colors.tool_status_success),
            PlanTaskStatus::Failed => ("✗".to_string(), theme.colors.tool_status_error),
            PlanTaskStatus::Skipped => ("⏭".to_string(), theme.colors.tool_status_cancelled),
            PlanTaskStatus::Cancelled => ("⊘".to_string(), theme.colors.tool_status_cancelled),
            PlanTaskStatus::Paused => ("⏸".to_string(), theme.colors.tool_status_awaiting),
        }
    } else {
        (String::new(), theme.colors.fg_muted)
    };

    let elapsed_str = if !is_pending && sub.started_at_ms.is_some() {
        match sub.elapsed_ms() {
            Some(ms) => {
                let formatted = format_elapsed_ms(ms);
                if sub.status == PlanTaskStatus::Running {
                    format!("(running {})", formatted)
                } else {
                    format!("({})", formatted)
                }
            }
            None => String::new(),
        }
    } else {
        String::new()
    };

    let mut suffix_spans: Vec<Span<'static>> = Vec::new();
    if !icon_str.is_empty() {
        suffix_spans.push(Span::styled(
            format!(" {}", icon_str),
            Style::default().fg(icon_color),
        ));
    }
    if !elapsed_str.is_empty() {
        suffix_spans.push(Span::styled(
            format!(" {}", elapsed_str),
            Style::default().fg(theme.colors.fg_muted),
        ));
    }
    if let Some(info) = &sub.delegated_to {
        suffix_spans.push(Span::styled(
            format!(" [delegated → {}]", info.agent_name),
            Style::default().fg(theme.colors.accent),
        ));
    }
    let prefix = format!("    {}. ", sub.number);
    let suffix_cells: usize = suffix_spans.iter().map(|s| geom::cells(&s.content)).sum();
    // The prefix and the suffix come out of the row's cells; the "..." marker
    // is inside what is left for the title.
    let title_display = geom::ellipsize(
        &sub.title,
        inner_width.saturating_sub(geom::cells(&prefix) + suffix_cells),
        "...",
    );

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            vertical_char.to_string(),
            Style::default().fg(theme.colors.decision_border),
        ),
        Span::styled(
            format!("{prefix}{title_display}"),
            Style::default().fg(theme.colors.fg_primary),
        ),
    ];
    spans.extend(suffix_spans);

    // The leading border span is outside `inner_width`.
    let content_cells: usize = spans.iter().skip(1).map(|s| geom::cells(&s.content)).sum();
    let padding = inner_width.saturating_sub(content_cells);
    spans.push(Span::raw(geom::spaces(padding)));
    spans.push(Span::styled(
        vertical_char.to_string(),
        Style::default().fg(theme.colors.decision_border),
    ));
    lines.push(Line::from(spans));

    // Render sub-task description if present
    if !sub.description.is_empty() {
        let desc_lines = wrap_description(&sub.description, inner_width.saturating_sub(8));
        for desc_line in desc_lines {
            lines.push(Line::from(vec![
                Span::styled(
                    vertical_char.to_string(),
                    Style::default().fg(theme.colors.decision_border),
                ),
                Span::styled(
                    format!("        {}", desc_line),
                    Style::default().fg(theme.colors.fg_muted),
                ),
                Span::raw(geom::spaces(
                    inner_width.saturating_sub(8 + geom::cells(&desc_line)),
                )),
                Span::styled(
                    vertical_char.to_string(),
                    Style::default().fg(theme.colors.decision_border),
                ),
            ]));
        }
    }

    lines
}

pub fn render_plan_card_lines<'a>(
    plan: &Plan,
    theme: &crate::adapters::tui::theme::Theme,
    width: u16,
    is_pending: bool,
) -> Vec<Line<'a>> {
    if width < 4 {
        return vec![Line::from(Span::styled(
            format!("[plan unavailable: {}]", plan.id),
            Style::default().fg(theme.colors.fg_muted),
        ))];
    }

    let border_type = if width < 64 {
        BorderType::Plain
    } else {
        BorderType::Double
    };

    let inner_width = (width as usize).saturating_sub(2);
    let mut lines: Vec<Line<'a>> = Vec::new();

    let top_border = match border_type {
        BorderType::Double => {
            let mut left = "╔".to_string();
            left.push_str(&"═".repeat(inner_width));
            left.push('╗');
            left
        }
        _ => {
            let mut left = "┌".to_string();
            left.push_str(&"─".repeat(inner_width));
            left.push('┐');
            left
        }
    };
    lines.push(Line::from(Span::styled(
        top_border,
        Style::default().fg(theme.colors.decision_border),
    )));

    let header_text = format!(" Plan: {} ", plan.title);
    let header_display = if geom::cells(&header_text) > inner_width {
        format!(
            " {}... ",
            geom::clip(&plan.title, inner_width.saturating_sub(7))
        )
    } else {
        header_text
    };
    let header_pad = inner_width.saturating_sub(geom::cells(&header_display));
    let left_pad = header_pad / 2;
    let right_pad = header_pad - left_pad;
    let vertical_char = match border_type {
        BorderType::Double => "║",
        _ => "│",
    };
    lines.push(Line::from(vec![
        Span::styled(
            vertical_char,
            Style::default().fg(theme.colors.decision_border),
        ),
        Span::styled(" ".repeat(left_pad), Style::default()),
        Span::styled(
            header_display,
            Style::default()
                .fg(theme.colors.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(right_pad), Style::default()),
        Span::styled(
            vertical_char,
            Style::default().fg(theme.colors.decision_border),
        ),
    ]));

    let separator = match border_type {
        BorderType::Double => {
            format!("╠{}╣", "═".repeat(inner_width))
        }
        _ => {
            format!("├{}┤", "─".repeat(inner_width))
        }
    };
    lines.push(Line::from(Span::styled(
        separator,
        Style::default().fg(theme.colors.decision_border),
    )));

    for task in &plan.tasks {
        let task_title = format!("{}. {}", task.number, task.title);
        let title_style = if is_pending {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let (icon_str, icon_color) = if !is_pending && task.started_at_ms.is_some() {
            match task.status {
                PlanTaskStatus::Pending => (String::new(), theme.colors.fg_muted),
                PlanTaskStatus::Running => ("●".to_string(), theme.colors.tool_status_executing),
                PlanTaskStatus::Waiting => (
                    format!(
                        "⧖ (deps: {})",
                        task.waiting_on
                            .iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    theme.colors.tool_status_awaiting,
                ),
                PlanTaskStatus::Completed => ("✓".to_string(), theme.colors.tool_status_success),
                PlanTaskStatus::Failed => ("✗".to_string(), theme.colors.tool_status_error),
                PlanTaskStatus::Skipped => ("⏭".to_string(), theme.colors.tool_status_cancelled),
                PlanTaskStatus::Cancelled => ("⊘".to_string(), theme.colors.tool_status_cancelled),
                PlanTaskStatus::Paused => ("⏸".to_string(), theme.colors.tool_status_awaiting),
            }
        } else {
            (String::new(), theme.colors.fg_muted)
        };

        let elapsed_str = if !is_pending && task.started_at_ms.is_some() {
            match task.elapsed_ms() {
                Some(ms) => {
                    let formatted = format_elapsed_ms(ms);
                    if task.status == PlanTaskStatus::Running {
                        format!("(running {})", formatted)
                    } else {
                        format!("({})", formatted)
                    }
                }
                None => String::new(),
            }
        } else {
            String::new()
        };

        // Suffix spans first: their cells come out of the title's budget.
        let mut suffix_spans: Vec<Span<'_>> = Vec::new();
        if !icon_str.is_empty() {
            suffix_spans.push(Span::styled(
                format!(" {}", icon_str),
                Style::default().fg(icon_color),
            ));
        }
        if !elapsed_str.is_empty() {
            suffix_spans.push(Span::styled(
                format!(" {}", elapsed_str),
                Style::default().fg(theme.colors.fg_muted),
            ));
        }
        if let Some(info) = &task.delegated_to {
            suffix_spans.push(Span::styled(
                format!(" [delegated → {}]", info.agent_name),
                Style::default().fg(theme.colors.accent),
            ));
        }
        let suffix_cells: usize = suffix_spans.iter().map(|s| geom::cells(&s.content)).sum();
        // Two cells of indent and the suffix precede the right border; the
        // "..." marker is inside what is left for `N. title`.
        let title_display = geom::ellipsize(
            &task_title,
            inner_width.saturating_sub(2 + suffix_cells),
            "...",
        );
        let mut spans: Vec<Span<'_>> = vec![
            Span::styled(
                vertical_char,
                Style::default().fg(theme.colors.decision_border),
            ),
            Span::styled(format!("  {}", title_display), title_style),
        ];
        spans.extend(suffix_spans);
        // The leading border span is outside `inner_width`.
        let content_cells: usize = spans.iter().skip(1).map(|s| geom::cells(&s.content)).sum();
        spans.push(Span::raw(geom::spaces(
            inner_width.saturating_sub(content_cells),
        )));
        spans.push(Span::styled(
            vertical_char,
            Style::default().fg(theme.colors.decision_border),
        ));
        lines.push(Line::from(spans));

        if !task.description.is_empty() {
            let desc_lines = wrap_description(&task.description, inner_width.saturating_sub(4));
            for desc_line in desc_lines {
                lines.push(Line::from(vec![
                    Span::styled(
                        vertical_char,
                        Style::default().fg(theme.colors.decision_border),
                    ),
                    Span::styled(
                        format!("    {}", desc_line),
                        Style::default().fg(theme.colors.fg_muted),
                    ),
                    Span::raw(geom::spaces(
                        inner_width.saturating_sub(4 + geom::cells(&desc_line)),
                    )),
                    Span::styled(
                        vertical_char,
                        Style::default().fg(theme.colors.decision_border),
                    ),
                ]));
            }
        }

        // Story 10.6: render sub-tasks indented under parent
        if task.has_sub_tasks() {
            for sub in &task.sub_tasks {
                lines.extend(render_sub_task_card_line(
                    sub,
                    theme,
                    vertical_char,
                    inner_width,
                    is_pending,
                ));
            }
        }
    }

    // The decision itself is answered through its `Decision: Plan card` frame (FR-21-6-f):
    // the Stage row paints no rail, and the pending marker takes the status row's place, so
    // the row is as tall with the decision as without it.
    let status_text = if is_pending {
        format!(
            "[awaiting your decision — {}]",
            leader_follow_up_compact(LeaderCmd::Decisions)
        )
    } else {
        match plan.status {
            PlanStatus::Executing => {
                if let Some(ts) = plan.resolved_at {
                    format!("[approved {}]", format_timestamp(ts))
                } else {
                    "[approved]".to_string()
                }
            }
            PlanStatus::Completed => {
                if let Some(ts) = plan.resolved_at {
                    format!("[completed {}]", format_timestamp(ts))
                } else {
                    "[completed]".to_string()
                }
            }
            PlanStatus::Rejected => {
                if let Some(ts) = plan.resolved_at {
                    format!("[rejected {}]", format_timestamp(ts))
                } else {
                    "[rejected]".to_string()
                }
            }
            PlanStatus::Editing => "[editing]".to_string(),
            PlanStatus::Pending => "[pending]".to_string(),
            PlanStatus::Cancelled => {
                if let Some(ts) = plan.resolved_at {
                    format!("[cancelled {}]", format_timestamp(ts))
                } else {
                    "[cancelled]".to_string()
                }
            }
        }
    };
    lines.push(Line::from(vec![
        Span::styled(
            vertical_char,
            Style::default().fg(theme.colors.decision_border),
        ),
        Span::styled(
            format!("  {}", status_text),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::raw(geom::spaces(
            inner_width.saturating_sub(2 + geom::cells(&status_text)),
        )),
        Span::styled(
            vertical_char,
            Style::default().fg(theme.colors.decision_border),
        ),
    ]));

    let bottom_border = match border_type {
        BorderType::Double => {
            format!("╚{}╝", "═".repeat(inner_width))
        }
        _ => {
            format!("└{}┘", "─".repeat(inner_width))
        }
    };
    lines.push(Line::from(Span::styled(
        bottom_border,
        Style::default().fg(theme.colors.decision_border),
    )));

    lines
}

/// Word-wrap `text` into rows of at most `max_width` display cells.
///
/// Whitespace is normalised to single spaces. A word wider than the budget is
/// hard-wrapped on grapheme boundaries; an oversized single grapheme stays one
/// intact row. Shared by the plan, ask, fork and rewind cards.
pub(super) fn wrap_description(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_cells = 0usize;
    for word in text.split_whitespace() {
        let word_cells = geom::cells(word);
        if word_cells > max_width {
            // Flush current line first
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
            // Hard-wrap on grapheme boundaries; the last row keeps filling.
            let mut rows = geom::wrap_hard(word, max_width);
            let last = rows.pop().unwrap_or_default();
            result.extend(rows.into_iter().map(str::to_string));
            current = last.to_string();
            current_cells = geom::cells(last);
        } else if current.is_empty() {
            current = word.to_string();
            current_cells = word_cells;
        } else if current_cells + 1 + word_cells <= max_width {
            current.push(' ');
            current.push_str(word);
            current_cells += 1 + word_cells;
        } else {
            result.push(std::mem::take(&mut current));
            current = word.to_string();
            current_cells = word_cells;
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn format_timestamp(unix_ts: i64) -> String {
    use std::time::SystemTime;
    if unix_ts < 0 {
        return format!("{}", unix_ts);
    }
    let duration = std::time::Duration::from_secs(unix_ts as u64);
    if let Some(time) = SystemTime::UNIX_EPOCH.checked_add(duration) {
        let datetime: chrono::DateTime<chrono::Local> = time.into();
        datetime.format("%H:%M:%S").to_string()
    } else {
        format!("{}", unix_ts)
    }
}

pub fn plan_card_height(plan: &Plan, width: u16, is_pending: bool) -> usize {
    render_plan_card_lines(
        plan,
        &crate::adapters::tui::theme::Theme::dark(),
        width,
        is_pending,
    )
    .len()
}

pub fn missing_plan_lines<'a>(
    plan_id: &str,
    theme: &crate::adapters::tui::theme::Theme,
) -> Vec<Line<'a>> {
    vec![Line::from(Span::styled(
        format!("[plan unavailable: {}]", plan_id),
        Style::default()
            .fg(theme.colors.fg_muted)
            .add_modifier(Modifier::DIM),
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::{EffortEstimate, PlanTask, PlanTaskStatus};

    fn make_plan() -> Plan {
        Plan {
            id: "test-plan-id".to_string(),
            title: "Test Plan".to_string(),
            tasks: vec![
                PlanTask {
                    number: 1,
                    title: "Step 1".to_string(),
                    description: "Do the first thing".to_string(),
                    depends_on: vec![],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                },
                PlanTask {
                    number: 2,
                    title: "Step 2".to_string(),
                    description: String::new(),
                    depends_on: vec![1],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                },
            ],
            estimated_effort: Some(EffortEstimate {
                tool_calls: Some(5),
                seconds: Some(30),
            }),
            status: PlanStatus::Pending,
            created_at: 1700000000,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        }
    }

    #[test]
    fn render_pending_plan_card() {
        let plan = make_plan();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, true);
        assert!(!lines.is_empty());
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains("Plan: Test Plan"));
        // 21-6 6-C (FR-21-6-f): the Stage row has no rail and no pending-only rows; the
        // decision is answered through its frame, and the estimate shows there.
        assert!(!text.contains("[y]") && !text.contains("Approve"));
        assert!(!text.contains("Estimated:"));
        assert!(text.contains("[awaiting your decision"));
    }

    #[test]
    fn story21_6_the_stage_row_is_as_tall_with_the_decision_as_without_it() {
        let plan = make_plan();
        for width in [60u16, 80, 130] {
            assert_eq!(
                plan_card_height(&plan, width, true),
                plan_card_height(&plan, width, false),
                "{width}"
            );
        }
    }

    #[test]
    fn render_resolved_plan_card() {
        let plan = make_plan();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, false);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains("[pending]"));
        assert!(!text.contains("[y]"));
    }

    #[test]
    fn narrow_width_uses_plain_border() {
        let plan = make_plan();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 50, true);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains('┌') || text.contains('└'));
        assert!(!text.contains('╔'));
    }

    #[test]
    fn wide_width_uses_double_border() {
        let plan = make_plan();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, true);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains('╔') || text.contains('╚'));
    }

    #[test]
    fn missing_plan_lines_are_dim() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = missing_plan_lines("missing-id", &theme);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("missing-id"));
    }

    #[test]
    fn wrap_description_basic() {
        let result = wrap_description("hello world foo bar", 11);
        assert_eq!(result, vec!["hello world", "foo bar"]);
    }

    #[test]
    fn wrap_description_single_word() {
        let result = wrap_description("hello", 10);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn wrap_description_long_word() {
        let result = wrap_description("supercalifragilisticexpialidocious", 10);
        assert_eq!(
            result,
            vec!["supercalif", "ragilistic", "expialidoc", "ious"]
        );
    }

    #[test]
    fn render_with_empty_effort() {
        let mut plan = make_plan();
        plan.estimated_effort = None;
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, true);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(!text.contains("Estimated:"));
    }

    #[test]
    fn executing_status_shows_approved() {
        let mut plan = make_plan();
        plan.status = PlanStatus::Executing;
        plan.resolved_at = Some(1700000060);
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, false);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(
            text.contains("[approved"),
            "status footer should say 'approved', got: {}",
            text
        );
        assert!(!text.contains("[executing"));
    }

    #[test]
    fn task_titles_bold_when_pending() {
        let plan = make_plan();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, true);
        let task_line = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.as_ref().contains("1. Step 1"))
            })
            .unwrap();
        assert!(
            task_line
                .spans
                .iter()
                .any(|s| s.style.add_modifier == Modifier::BOLD),
            "task title should be BOLD when pending"
        );
    }

    #[test]
    fn plain_border_separator_uses_single_chars() {
        let plan = make_plan();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 50, true);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Plain border should use ├ and ┤, not ╠ and ╣
        assert!(text.contains('├'), "plain separator should use ├");
        assert!(text.contains('┤'), "plain separator should use ┤");
        assert!(!text.contains('╠'), "plain separator should NOT use ╠");
        assert!(!text.contains('╣'), "plain separator should NOT use ╣");
    }

    #[test]
    fn negative_timestamp_handled() {
        assert_eq!(format_timestamp(-1), "-1");
        assert_eq!(format_timestamp(0), "00:00:00");
    }

    fn make_plan_executing() -> Plan {
        Plan {
            id: "exec-plan-id".to_string(),
            title: "Exec Plan".to_string(),
            tasks: vec![
                PlanTask {
                    number: 1,
                    title: "Init".to_string(),
                    description: String::new(),
                    depends_on: vec![],
                    status: PlanTaskStatus::Completed,
                    started_at_ms: Some(1700000000000),
                    completed_at_ms: Some(1700000005000),
                    result: Some(crate::domain::models::plan::TaskResult {
                        text: "Done".to_string(),
                        tool_call_count: 1,
                        token_count: Some(20),
                    }),
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                },
                PlanTask {
                    number: 2,
                    title: "Build".to_string(),
                    description: String::new(),
                    depends_on: vec![1],
                    status: PlanTaskStatus::Running,
                    started_at_ms: None, // None keeps elapsed out of snapshot (avoids wall-clock drift)
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                },
                PlanTask {
                    number: 3,
                    title: "Test".to_string(),
                    description: String::new(),
                    depends_on: vec![2],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                },
                PlanTask {
                    number: 4,
                    title: "Deploy".to_string(),
                    description: String::new(),
                    depends_on: vec![3],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                },
            ],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1700000000,
            resolved_at: Some(1700000060),
            host_message_id: None,
            origin_restrictions: None,
        }
    }

    #[test]
    fn snapshot_mid_execution_plan_card() {
        let plan = make_plan_executing();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, false);
        let text: String = lines
            .iter()
            .map(|l| {
                let row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                row + "\n"
            })
            .collect();
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_failed_with_skipped_downstream() {
        let mut plan = make_plan_executing();
        plan.status = PlanStatus::Completed;
        plan.tasks[0].status = PlanTaskStatus::Completed;
        plan.tasks[0].completed_at_ms = Some(1700000005000);
        plan.tasks[1].status = PlanTaskStatus::Failed;
        plan.tasks[1].started_at_ms = Some(1700000005000);
        plan.tasks[1].error = Some("compilation error".to_string());
        plan.tasks[1].completed_at_ms = Some(1700000010000);
        plan.tasks[2].status = PlanTaskStatus::Skipped;
        plan.tasks[2].error = Some("Skipped — blocked by upstream task(s) 2".to_string());
        plan.tasks[2].completed_at_ms = Some(1700000010001);
        plan.tasks[3].status = PlanTaskStatus::Skipped;
        plan.tasks[3].error = Some("Skipped — blocked by upstream task(s) 2".to_string());
        plan.tasks[3].completed_at_ms = Some(1700000010001);
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, false);
        let text: String = lines
            .iter()
            .map(|l| {
                let row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                row + "\n"
            })
            .collect();
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_plan_card_with_delegation_badge() {
        let mut plan = make_plan_executing();
        plan.tasks[1].delegated_to = Some(crate::domain::models::plan::DelegationInfo {
            agent_name: "code-reviewer".to_string(),
            agent_id: None,
            delegated_at_ms: 1_700_000_000,
            spool_task_id: None,
        });
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_plan_card_lines(&plan, &theme, 80, false);
        let text: String = lines
            .iter()
            .map(|l| {
                let row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                row + "\n"
            })
            .collect();
        insta::assert_snapshot!(text);
    }

    // ---- Story 21.11 display-width primitive ---------------------------------
    // Whole-card keystones run through `render_plan_card_lines` in
    // tests/conformance_21_11_display_width/k5a_cards.rs.

    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    #[test]
    fn display_width_wrap_description_packs_graphemes_by_cells() {
        assert_eq!(
            wrap_description(&C.repeat(5), 4),
            vec![C.repeat(2), C.repeat(2), C.to_string()]
        );
        assert_eq!(
            wrap_description(&G.repeat(3), 4),
            vec![G.repeat(2), G.to_string()]
        );
        assert_eq!(
            wrap_description(&A.repeat(5), 2),
            vec![A.repeat(2), A.repeat(2), A.to_string()]
        );
        assert_eq!(wrap_description("ab 界界 cd", 5), vec!["ab", "界界", "cd"]);
        // An oversized single grapheme stays one intact row.
        assert_eq!(wrap_description(G, 1), vec![G]);
        // Short words keep packing after a hard-wrapped tail.
        assert_eq!(
            wrap_description(&format!("{} z", C.repeat(3)), 4),
            vec![C.repeat(2), format!("{C} z")]
        );
    }
}
