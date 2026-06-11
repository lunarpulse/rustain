use std::collections::HashMap;

use ratatui::{
    prelude::*,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem},
};

use crate::domain::models::SubagentRunStatus;
use crate::domain::models::subagent_view::{AgentRowView, OwnershipKind};
use crate::domain::services::plan_runtime::format_elapsed_ms;

use super::sidebar::truncate_to_width;

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    entries: &[AgentRowView],
    selected: usize,
    is_focused: bool,
    theme: &crate::adapters::tui::theme::Theme,
    spool_tail_cache: &HashMap<crate::domain::models::AgentId, String>,
    now_fn: &dyn Fn() -> i64,
) {
    Clear.render(area, buf);

    let title = " Agents ".to_string();
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

    if entries.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                "No agents running.",
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::ITALIC),
            )),
            Line::from(Span::styled(
                "Spawn one with `rustain spawn --agent <name>` or via plan delegation.",
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
        return;
    }

    if inner_area.width == 0 || inner_area.height == 0 {
        return;
    }

    let available_height = inner_area.height as usize;
    if available_height == 0 {
        return;
    }

    let scroll_offset = if selected >= available_height {
        selected.saturating_sub(available_height - 1)
    } else {
        0
    };
    let visible_count = entries.len().min(available_height);
    let items: Vec<ListItem> = entries
        .iter()
        .skip(scroll_offset)
        .take(visible_count)
        .enumerate()
        .map(|(i, entry)| {
            let (icon_sym, icon_color) = subagent_icon_for(entry.current_status, theme);
            let glyph = ownership_glyph(entry.ownership);
            let indent = indent_for_depth(entry.depth);
            let now = now_fn();
            let elapsed_ms = (now - entry.spawned_at).max(0);
            let elapsed = format_elapsed_ms(elapsed_ms);

            let task_summary = spool_tail_cache
                .get(&entry.agent_id)
                .and_then(|s| s.lines().last().map(|l| l.to_string()))
                .unwrap_or_default();

            let prefix_width = icon_sym.chars().count()
                + 1
                + glyph.chars().count()
                + 1
                + indent.chars().count()
                + entry.subagent_type.chars().count()
                + 2;
            let max_task_width = (inner_area.width as usize).saturating_sub(prefix_width + 12);
            let truncated_task = if task_summary.len() > max_task_width && max_task_width > 3 {
                truncate_to_width(&task_summary, max_task_width)
            } else {
                task_summary
            };

            let mut spans: Vec<Span> = vec![
                Span::styled(icon_sym.to_string(), Style::default().fg(icon_color)),
                Span::raw(" "),
                Span::styled(
                    glyph.to_string(),
                    Style::default().fg(theme.colors.fg_primary),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("{}{}", indent, entry.subagent_type),
                    Style::default().fg(theme.colors.fg_primary),
                ),
            ];

            if !truncated_task.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", truncated_task),
                    Style::default().fg(theme.colors.fg_secondary),
                ));
            }

            spans.push(Span::styled(
                format!("  {}", elapsed),
                Style::default().fg(theme.colors.fg_muted),
            ));

            let row_style = if i == selected {
                if is_focused {
                    Style::default()
                        .fg(theme.colors.fg_primary)
                        .add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(theme.colors.fg_primary)
                }
            } else {
                Style::default()
            };

            ListItem::new(Line::from(spans)).style(row_style)
        })
        .collect();

    let list = List::new(items);
    let list_area = Rect {
        x: inner_area.x,
        y: inner_area.y,
        width: inner_area.width,
        height: visible_count as u16,
    };
    <List<'_> as Widget>::render(list, list_area, buf);
}

fn subagent_icon_for(
    status: SubagentRunStatus,
    theme: &crate::adapters::tui::theme::Theme,
) -> (&'static str, Color) {
    match status {
        SubagentRunStatus::RunningFg => ("\u{25CF}", theme.colors.tool_status_executing),
        SubagentRunStatus::Idle => ("\u{23F8}", theme.colors.tool_status_awaiting),
        SubagentRunStatus::Completed => ("\u{2713}", theme.colors.tool_status_success),
        SubagentRunStatus::Failed => ("\u{2717}", theme.colors.tool_status_error),
        SubagentRunStatus::Killed => ("\u{2298}", theme.colors.tool_status_cancelled),
        SubagentRunStatus::RunningBg => ("\u{25D0}", theme.colors.tool_status_awaiting),
    }
}

fn ownership_glyph(kind: OwnershipKind) -> &'static str {
    match kind {
        OwnershipKind::Owned => "\u{2666}",
        OwnershipKind::Peer => "\u{25C7}",
    }
}

fn indent_for_depth(depth: usize) -> String {
    if depth <= 1 {
        String::new()
    } else {
        "  ".repeat((depth - 1).min(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::AgentId;

    fn make_entry(name: &str, depth: usize, status: SubagentRunStatus) -> AgentRowView {
        AgentRowView {
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: name.to_string(),
            spawned_at: 0,
            depth,
            current_status: status,
            ownership: OwnershipKind::Owned,
        }
    }

    fn render_to_text(
        entries: &[AgentRowView],
        selected: usize,
        is_focused: bool,
        width: u16,
        height: u16,
    ) -> String {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let cache = HashMap::new();
        let now_fn: fn() -> i64 = || 0i64;
        render(
            area, &mut buf, entries, selected, is_focused, &theme, &cache, &now_fn,
        );
        let mut text = String::new();
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            text.push_str(&format!("{}\n", line.trim_end()));
        }
        text
    }

    #[test]
    fn snapshot_agent_panel_empty() {
        let entries: Vec<AgentRowView> = vec![];
        let text = render_to_text(&entries, 0, true, 60, 8);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_agent_panel_single() {
        let entries = vec![make_entry("code-reviewer", 1, SubagentRunStatus::Idle)];
        let text = render_to_text(&entries, 0, true, 60, 5);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_agent_panel_three_level() {
        let entries = vec![
            make_entry("orchestrator", 1, SubagentRunStatus::RunningFg),
            make_entry("coder", 2, SubagentRunStatus::Idle),
            make_entry("reviewer", 3, SubagentRunStatus::Idle),
        ];
        let text = render_to_text(&entries, 0, true, 60, 5);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_agent_panel_terminal_states() {
        let entries = vec![
            make_entry("agent-ok", 1, SubagentRunStatus::Completed),
            make_entry("agent-fail", 1, SubagentRunStatus::Failed),
            make_entry("agent-killed", 1, SubagentRunStatus::Killed),
        ];
        let text = render_to_text(&entries, 1, true, 60, 5);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn test_focused_row_has_reversed_modifier() {
        let entries = vec![
            make_entry("alpha", 1, SubagentRunStatus::Idle),
            make_entry("beta", 1, SubagentRunStatus::RunningFg),
        ];
        let theme = crate::adapters::tui::theme::Theme::dark();
        let area = Rect::new(0, 0, 60, 5);
        let mut buf = Buffer::empty(area);
        let cache = HashMap::new();
        let now_fn: fn() -> i64 = || 0i64;
        render(area, &mut buf, &entries, 1, true, &theme, &cache, &now_fn);
        let inner_y = area.y + 1;
        let selected_cell = buf.cell((area.x + 1, inner_y + 1)).unwrap();
        assert!(
            selected_cell.modifier.contains(Modifier::REVERSED),
            "selected focused row should have REVERSED modifier"
        );
        let unselected_cell = buf.cell((area.x + 1, inner_y)).unwrap();
        assert!(
            !unselected_cell.modifier.contains(Modifier::REVERSED),
            "unselected row should NOT have REVERSED modifier"
        );
    }
}
