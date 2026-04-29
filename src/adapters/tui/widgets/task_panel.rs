//! Task panel sidebar widget — Story 6.3.
//!
//! Renders a scrollable list of plan tasks in the sidebar, driven by
//! `AppEvent::PlanTaskStatusChanged` (from 6-2a). The panel is read-only: it
//! reads task state from `conversation.plans[plan_id]` and never mutates plan
//! or runtime state.
//!
//! ## Render signature (architectural rule)
//!
//! `render_task_panel` takes `&mut ratatui::buffer::Buffer`, not `&mut Frame`.
//! See `_bmad-output/implementation-artifacts/6-3-task-panel-and-progress-monitoring.md`
//! AC7 for the canonical decision: all sidebar / pane widgets from Story 6.3
//! onward use `&mut Buffer` to match the `StatefulWidget` convention and stay
//! composable inside other layouts. `&mut Frame` is reserved for top-level
//! layout composition (cursor positioning, popup overlays).
//!
//! ## Plan resolution chain
//!
//! The panel resolves which plan to render via a three-stage fallback:
//! 1. `last_executed_plan_id` — the plan most recently started/completed.
//! 2. First `Executing` plan in `conversation.plans` (defensive).
//! 3. Most recently completed plan by `max(task.completed_at_ms)`.
//!
//! ## Multi-panel sidebar dispatcher
//!
//! This is the second sidebar panel (after History). The render dispatcher in
//! `event_loop.rs` branches on `state.sidebar_panel` to select which panel
//! renders. Story 10.4 (subagent panel) will reuse this pattern — do not
//! introduce a trait-based registry until ≥ 4 panels.
//!
//! ## 10.6 sub-task seat
//!
//! Story 10.6 (sub-task decomposition, rehomed from 6-2b) will add
//! `PlanTask.sub_tasks: Vec<PlanSubTask>`. The panel must eventually render
//! sub-tasks as indented children with a `(k/n sub-tasks)` fraction on the
//! parent. In 6-3 — with no decomposed plans yet — this is a layout-only seat.
//!
//! ## Task control keys (Story 6.4)
//!
//! Panel: `p` (Pause/Resume), `s` (Skip), `x` (Cancel plan).
//! Drill-down Failed: `r` (Retry), `s` (Skip), `e` (Edit).
//! Drill-down Paused: `p` (Resume).

use ratatui::{
    prelude::*,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::domain::models::plan::{Plan, PlanStatus, PlanTaskStatus};
use crate::domain::services::plan_runtime::format_elapsed_ms;

use super::sidebar::truncate_to_width;

struct TaskIcon {
    symbol: &'static str,
    color: Color,
    suffix: String,
}

fn task_icon_for(
    status: PlanTaskStatus,
    theme: &crate::adapters::tui::theme::Theme,
    elapsed_ms: i64,
    waiting_on: &[u32],
) -> TaskIcon {
    match status {
        PlanTaskStatus::Pending => TaskIcon {
            symbol: "\u{23F3}",
            color: theme.colors.fg_muted,
            suffix: String::new(),
        },
        PlanTaskStatus::Running => TaskIcon {
            symbol: "\u{25CF}",
            color: theme.colors.tool_status_executing,
            suffix: format!(" (running {})", format_elapsed_ms(elapsed_ms)),
        },
        PlanTaskStatus::Waiting => TaskIcon {
            symbol: "\u{29D6}",
            color: theme.colors.tool_status_awaiting,
            suffix: if waiting_on.is_empty() {
                " (waiting)".to_string()
            } else {
                let deps = waiting_on
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" (deps: {})", deps)
            },
        },
        PlanTaskStatus::Completed => TaskIcon {
            symbol: "\u{2713}",
            color: theme.colors.tool_status_success,
            suffix: format!(" ({})", format_elapsed_ms(elapsed_ms)),
        },
        PlanTaskStatus::Failed => TaskIcon {
            symbol: "\u{2717}",
            color: theme.colors.tool_status_error,
            suffix: format!(" ({})", format_elapsed_ms(elapsed_ms)),
        },
        PlanTaskStatus::Skipped => TaskIcon {
            symbol: "\u{23ED}",
            color: theme.colors.tool_status_cancelled,
            suffix: " (skipped)".to_string(),
        },
        PlanTaskStatus::Cancelled => TaskIcon {
            symbol: "\u{2298}",
            color: theme.colors.tool_status_cancelled,
            suffix: " (cancelled)".to_string(),
        },
        PlanTaskStatus::Paused => TaskIcon {
            symbol: "\u{23F8}",
            color: theme.colors.tool_status_awaiting,
            suffix: " (paused)".to_string(),
        },
    }
}

pub fn render_task_panel(
    area: Rect,
    buf: &mut Buffer,
    plan: Option<&Plan>,
    selected_index: usize,
    is_focused: bool,
    theme: &crate::adapters::tui::theme::Theme,
) {
    Clear.render(area, buf);

    let (title, is_complete) = match plan {
        Some(p) if p.status == PlanStatus::Executing => {
            let t = format!(" Tasks \u{B7} {} ", p.title);
            (
                truncate_to_width(&t, area.width.saturating_sub(2) as usize),
                false,
            )
        }
        Some(p) if p.status == PlanStatus::Completed || p.status == PlanStatus::Cancelled => {
            let t = format!(" Tasks \u{B7} {} (last) ", p.title);
            (
                truncate_to_width(&t, area.width.saturating_sub(2) as usize),
                true,
            )
        }
        _ => (" Tasks ".to_string(), false),
    };

    let border_style = if is_focused {
        Style::default().fg(theme.colors.accent)
    } else {
        Style::default().fg(theme.colors.fg_secondary)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner_area = block.inner(area);
    block.render(area, buf);

    match plan {
        None => {
            let lines = vec![
                Line::from(Span::styled(
                    "No active plan.",
                    Style::default()
                        .fg(theme.colors.fg_muted)
                        .add_modifier(Modifier::ITALIC),
                )),
                Line::from(Span::styled(
                    "Start one by asking for",
                    Style::default()
                        .fg(theme.colors.fg_muted)
                        .add_modifier(Modifier::ITALIC),
                )),
                Line::from(Span::styled(
                    "a complex task.",
                    Style::default()
                        .fg(theme.colors.fg_muted)
                        .add_modifier(Modifier::ITALIC),
                )),
            ];
            let para = ratatui::widgets::Paragraph::new(lines).alignment(Alignment::Center);
            let centered_y = inner_area.y + inner_area.height.saturating_sub(3) / 2;
            let centered_area = Rect {
                y: centered_y,
                height: 3.min(inner_area.height),
                ..inner_area
            };
            para.render(centered_area, buf);
        }
        Some(plan) => {
            if is_complete {
                let notice = "\u{2504} Plan complete \u{B7} open a new plan to refresh \u{2504}";
                let notice_style = Style::default().fg(theme.colors.fg_muted);
                let notice_line = Line::from(Span::styled(notice.to_string(), notice_style));
                let notice_para =
                    ratatui::widgets::Paragraph::new(notice_line).alignment(Alignment::Center);
                if inner_area.height > 0 {
                    notice_para.render(
                        Rect {
                            height: 1,
                            ..inner_area
                        },
                        buf,
                    );
                }
            }

            let content_start = if is_complete { 1usize } else { 0usize };
            let available_height = inner_area.height.saturating_sub(content_start as u16) as usize;
            if available_height == 0 {
                return;
            }

            let items: Vec<ListItem> = plan
                .tasks
                .iter()
                .enumerate()
                .take(available_height)
                .map(|(i, task)| {
                    let elapsed = task.elapsed_ms().unwrap_or(0);
                    let icon = task_icon_for(task.status, theme, elapsed, &task.waiting_on);

                    let mut spans: Vec<Span> = vec![
                        Span::styled(
                            format!("{}. ", task.number),
                            Style::default().fg(theme.colors.fg_secondary),
                        ),
                        Span::styled(icon.symbol.to_string(), Style::default().fg(icon.color)),
                        Span::styled(
                            format!(" {}", task.title),
                            Style::default().fg(theme.colors.fg_primary),
                        ),
                        Span::styled(
                            icon.suffix.clone(),
                            Style::default().fg(theme.colors.fg_muted),
                        ),
                    ];

                    if !task.depends_on.is_empty() {
                        let deps: Vec<String> =
                            task.depends_on.iter().map(|d| d.to_string()).collect();
                        spans.push(Span::styled(
                            format!(" deps: {}", deps.join(", ")),
                            Style::default().fg(theme.colors.fg_muted),
                        ));
                    }

                    // 10.6: render sub-tasks indented; show (k/n) progress fraction on parent

                    let mut line = Line::from(spans);
                    let display_width = line.width();
                    let max_w = inner_area.width as usize;
                    if display_width > max_w {
                        let composed = format!(
                            "{}. {} {}{}{}",
                            task.number,
                            icon.symbol,
                            task.title,
                            icon.suffix,
                            if task.depends_on.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    " deps: {}",
                                    task.depends_on
                                        .iter()
                                        .map(|d| d.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            }
                        );
                        let truncated = truncate_to_width(&composed, max_w);
                        line = Line::from(Span::styled(
                            truncated,
                            Style::default().fg(theme.colors.fg_primary),
                        ));
                    }

                    if i == selected_index && is_focused {
                        ListItem::new(
                            line.style(
                                Style::default()
                                    .fg(theme.colors.fg_primary)
                                    .add_modifier(Modifier::REVERSED),
                            ),
                        )
                    } else {
                        ListItem::new(line)
                    }
                })
                .collect();

            if !items.is_empty() {
                let list_area = Rect {
                    y: inner_area.y + content_start as u16,
                    height: available_height as u16,
                    ..inner_area
                };
                let mut list_state = ListState::default();
                list_state.select(if is_focused && !plan.tasks.is_empty() {
                    Some(selected_index.min(plan.tasks.len() - 1))
                } else {
                    None
                });
                let list = List::new(items).highlight_style(
                    Style::default()
                        .fg(theme.colors.fg_primary)
                        .add_modifier(Modifier::REVERSED),
                );
                ratatui::widgets::StatefulWidget::render(list, list_area, buf, &mut list_state);
            }
        }
    }
}

pub fn resolve_panel_plan<'a>(
    conversation: &'a crate::domain::models::Conversation,
    last_id: Option<&str>,
) -> Option<&'a Plan> {
    last_id
        .and_then(|id| conversation.plans.get(id))
        .or_else(|| {
            conversation
                .plans
                .values()
                .filter(|p| p.status == PlanStatus::Executing)
                .max_by_key(|p| (p.created_at, p.id.as_str()))
        })
        .or_else(|| {
            conversation.plans.values().max_by_key(|p| {
                let last_completed = p
                    .tasks
                    .iter()
                    .filter_map(|t| t.completed_at_ms)
                    .max()
                    .unwrap_or(0);
                (last_completed, p.created_at, p.id.as_str())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::color_detect::ColorCapability;
    use crate::adapters::tui::theme::Theme;
    use crate::domain::models::Conversation;
    use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus, TaskResult};

    fn test_theme() -> Theme {
        Theme::for_capability(ColorCapability::TrueColor)
    }

    fn make_plan(status: PlanStatus, tasks: Vec<PlanTask>) -> Plan {
        Plan {
            id: "test-plan".to_string(),
            title: "Test Plan".to_string(),
            tasks,
            estimated_effort: None,
            status,
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

    fn collect_buffer(buf: &Buffer, area: Rect) -> String {
        let mut s = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                s.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
        }
        s
    }

    #[test]
    fn render_empty_state() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        let area = Rect::new(0, 0, 40, 20);
        render_task_panel(area, &mut buf, None, 0, true, &test_theme());
        let content = collect_buffer(&buf, area);
        assert!(content.contains("No active plan"));
    }

    #[test]
    fn render_executing_plan() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        let area = Rect::new(0, 0, 40, 20);
        let plan = make_plan(
            PlanStatus::Executing,
            vec![
                make_task(1, "Task one", PlanTaskStatus::Completed),
                make_task(2, "Task two", PlanTaskStatus::Running),
                make_task(3, "Task three", PlanTaskStatus::Pending),
                make_task(4, "Task four", PlanTaskStatus::Failed),
            ],
        );
        render_task_panel(area, &mut buf, Some(&plan), 0, true, &test_theme());
        let content = collect_buffer(&buf, area);
        assert!(content.contains("Task one"));
        assert!(content.contains("Task two"));
    }

    #[test]
    fn render_completed_plan_shows_last() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        let area = Rect::new(0, 0, 40, 20);
        let plan = make_plan(
            PlanStatus::Completed,
            vec![make_task(1, "Done task", PlanTaskStatus::Completed)],
        );
        render_task_panel(area, &mut buf, Some(&plan), 0, true, &test_theme());
        let content = collect_buffer(&buf, area);
        assert!(content.contains("(last)"));
        assert!(content.contains("Plan complete"));
    }

    #[test]
    fn render_narrow_truncates_title() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 20));
        let area = Rect::new(0, 0, 30, 20);
        let plan = make_plan(
            PlanStatus::Executing,
            vec![make_task(
                1,
                "A very long task title that exceeds width",
                PlanTaskStatus::Pending,
            )],
        );
        render_task_panel(area, &mut buf, Some(&plan), 0, true, &test_theme());
    }

    #[test]
    fn resolve_panel_plan_prefers_last_id() {
        let mut conv = Conversation {
            id: "conv1".to_string(),
            title: String::new(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };
        let p1 = make_plan(
            PlanStatus::Completed,
            vec![make_task(1, "A", PlanTaskStatus::Completed)],
        );
        let p2 = make_plan(
            PlanStatus::Executing,
            vec![make_task(1, "B", PlanTaskStatus::Running)],
        );
        conv.plans.insert("p1".to_string(), p1);
        conv.plans.insert("p2".to_string(), p2);
        let result = resolve_panel_plan(&conv, Some("p1"));
        assert!(result.is_some());
        assert_eq!(result.unwrap().tasks[0].title, "A");
    }

    #[test]
    fn resolve_panel_plan_fallback_executing() {
        let mut conv = Conversation {
            id: "conv1".to_string(),
            title: String::new(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };
        let p2 = make_plan(
            PlanStatus::Executing,
            vec![make_task(1, "B", PlanTaskStatus::Running)],
        );
        conv.plans.insert("p2".to_string(), p2);
        let result = resolve_panel_plan(&conv, Some("nonexistent"));
        assert!(result.is_some());
        assert_eq!(result.unwrap().tasks[0].title, "B");
    }
}
