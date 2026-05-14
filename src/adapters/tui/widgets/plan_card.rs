use ratatui::prelude::*;
use ratatui::widgets::BorderType;

use crate::domain::models::plan::{Plan, PlanStatus, PlanTaskStatus};
use crate::domain::services::plan_runtime::format_elapsed_ms;

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
    let header_display = if header_text.len() > inner_width {
        format!(" {}... ", &plan.title[..inner_width.saturating_sub(7)])
    } else {
        header_text
    };
    let header_pad = inner_width.saturating_sub(header_display.len());
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
        let title_display = if task_title.len() > inner_width.saturating_sub(2) {
            format!(
                "{}. {}...",
                task.number,
                &task.title[..inner_width.saturating_sub(6)]
            )
        } else {
            task_title
        };
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

        let suffix = if icon_str.is_empty() && elapsed_str.is_empty() {
            String::new()
        } else if icon_str.is_empty() {
            format!(" {}", elapsed_str)
        } else if elapsed_str.is_empty() {
            format!(" {}", icon_str)
        } else {
            format!(" {} {}", icon_str, elapsed_str)
        };

        let full_width_needed = 2 + title_display.len() + suffix.len();
        let padding = inner_width.saturating_sub(full_width_needed);

        let mut spans: Vec<Span<'_>> = vec![
            Span::styled(
                vertical_char,
                Style::default().fg(theme.colors.decision_border),
            ),
            Span::styled(format!("  {}", title_display), title_style),
        ];
        if !icon_str.is_empty() {
            spans.push(Span::styled(
                format!(" {}", icon_str),
                Style::default().fg(icon_color),
            ));
        }
        if !elapsed_str.is_empty() {
            spans.push(Span::styled(
                format!(" {}", elapsed_str),
                Style::default().fg(theme.colors.fg_muted),
            ));
        }
        spans.push(Span::raw(" ".repeat(padding)));
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
                    Span::raw(" ".repeat(inner_width.saturating_sub(4 + desc_line.len()))),
                    Span::styled(
                        vertical_char,
                        Style::default().fg(theme.colors.decision_border),
                    ),
                ]));
            }
        }
    }

    if is_pending {
        if let Some(ref effort) = plan.estimated_effort {
            let mut effort_parts = Vec::new();
            if let Some(tc) = effort.tool_calls {
                effort_parts.push(format!("{} tool calls", tc));
            }
            if let Some(secs) = effort.seconds {
                effort_parts.push(format!("~{}s", secs));
            }
            if !effort_parts.is_empty() {
                let effort_text = format!("Estimated: {}", effort_parts.join(", "));
                lines.push(Line::from(vec![
                    Span::styled(
                        vertical_char,
                        Style::default().fg(theme.colors.decision_border),
                    ),
                    Span::styled(
                        format!("  {}", effort_text),
                        Style::default()
                            .fg(theme.colors.fg_muted)
                            .add_modifier(Modifier::ITALIC)
                            .add_modifier(Modifier::DIM),
                    ),
                    Span::raw(" ".repeat(inner_width.saturating_sub(2 + effort_text.len()))),
                    Span::styled(
                        vertical_char,
                        Style::default().fg(theme.colors.decision_border),
                    ),
                ]));
            }
        }

        let action_text = "[y] Approve  [e] Edit  [n] Reject";
        let action_display = if action_text.len() > inner_width {
            "[y] [e] [n]".to_string()
        } else {
            action_text.to_string()
        };
        let action_pad = inner_width.saturating_sub(action_display.len());
        let action_left = action_pad / 2;
        let action_right = action_pad - action_left;
        lines.push(Line::from(vec![
            Span::styled(
                vertical_char,
                Style::default().fg(theme.colors.decision_border),
            ),
            Span::raw(" ".repeat(action_left)),
            Span::styled("[y]", Style::default().fg(theme.colors.success)),
            Span::raw(" Approve  "),
            Span::styled("[e]", Style::default().fg(theme.colors.info)),
            Span::raw(" Edit  "),
            Span::styled("[n]", Style::default().fg(theme.colors.error)),
            Span::raw(" Reject"),
            Span::raw(" ".repeat(action_right)),
            Span::styled(
                vertical_char,
                Style::default().fg(theme.colors.decision_border),
            ),
        ]));
    } else {
        let status_text = match plan.status {
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
            Span::raw(" ".repeat(inner_width.saturating_sub(2 + status_text.len()))),
            Span::styled(
                vertical_char,
                Style::default().fg(theme.colors.decision_border),
            ),
        ]));
    }

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

fn wrap_description(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if word.len() > max_width {
            // Flush current line first
            if !current.is_empty() {
                result.push(current);
            }
            let mut remaining = word;
            while remaining.len() > max_width {
                result.push(remaining[..max_width].to_string());
                remaining = &remaining[max_width..];
            }
            current = remaining.to_string();
        } else if current.is_empty() {
            current = word.to_string();
        } else if current.len() + 1 + word.len() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            result.push(current);
            current = word.to_string();
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
        assert!(text.contains("[y]"));
        assert!(text.contains("[e]"));
        assert!(text.contains("[n]"));
        assert!(text.contains("Estimated:"));
        assert!(text.contains("5 tool calls"));
        assert!(!text.contains("~5 tool calls"));
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
                },
            ],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1700000000,
            resolved_at: Some(1700000060),
            host_message_id: None,
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
}
