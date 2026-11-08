//! Plan approval and plan card bodies (Story 6-0d AC4, 6-1a; the rows of the `Decision:
//! Plan approval` / `Decision: Plan card` frames since Story 21.6 6-C).
//!
//! The frame owns the border, the rail and the scrolling. The caller sanitizes what a
//! model or a file wrote before it gets here; the copy path passes the original bytes.

use std::path::Path;

use ratatui::prelude::*;

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::Plan;

/// The approval rows: the header, the summary, a blank row, then every line of the plan
/// file hard-wrapped to `width` cells (the body scrolls, nothing is cut).
pub fn lines(
    plan_path: &Path,
    summary: &str,
    contents: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let slug = plan_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("plan");
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let body = Style::default().fg(theme.colors.fg_primary);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Plan Approval — ", bold),
            Span::raw(format!("{slug}.md")),
        ]),
        Line::from(vec![
            Span::styled("Summary: ", bold),
            Span::raw(summary.to_owned()),
        ]),
        Line::default(),
    ];
    for line in contents.lines() {
        for row in geom::wrap_hard(line, width.max(1)) {
            lines.push(Line::from(Span::styled(row.to_owned(), body)));
        }
    }
    lines
}

/// The plan card's rows: its title, then each task with its description.
pub fn card_lines(plan: &Plan, theme: &Theme) -> Vec<Line<'static>> {
    let bold = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(theme.colors.fg_primary);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let mut lines = vec![Line::from(Span::styled(plan.title.clone(), bold))];
    for task in &plan.tasks {
        lines.push(Line::from(Span::styled(
            format!("{}. {}", task.number, task.title),
            body,
        )));
        for description in task.description.lines() {
            lines.push(Line::from(Span::styled(format!("   {description}"), muted)));
        }
    }
    if let Some(effort) = &plan.estimated_effort {
        let mut parts = Vec::new();
        if let Some(tool_calls) = effort.tool_calls {
            parts.push(format!("{tool_calls} tool calls"));
        }
        if let Some(seconds) = effort.seconds {
            parts.push(format!("~{seconds}s"));
        }
        if !parts.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("Estimated: {}", parts.join(", ")),
                muted.add_modifier(Modifier::ITALIC),
            )));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn story21_6_the_approval_header_names_the_plan_file_and_every_content_line_follows() {
        let theme = Theme::dark();
        let rows = text(&lines(
            Path::new("/tmp/plans/ship-it.md"),
            "three steps",
            "one\ntwo\nthree",
            80,
            &theme,
        ));
        assert_eq!(
            rows,
            [
                "Plan Approval — ship-it.md",
                "Summary: three steps",
                "",
                "one",
                "two",
                "three"
            ]
        );
    }

    #[test]
    fn story21_6_the_card_lists_every_task() {
        use crate::domain::models::plan::{PlanStatus, PlanTask};
        let theme = Theme::dark();
        let task = |number: u32, title: &str, description: &str| PlanTask {
            number,
            title: title.into(),
            description: description.into(),
            depends_on: vec![],
            status: Default::default(),
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        };
        let plan = Plan {
            id: "p".into(),
            title: "Ship".into(),
            tasks: vec![task(1, "Build", "cargo build"), task(2, "Test", "")],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        assert_eq!(
            text(&card_lines(&plan, &theme)),
            ["Ship", "1. Build", "   cargo build", "2. Test"]
        );
    }
}
