use std::collections::HashMap;

use ratatui::{
    prelude::*,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem},
};

use crate::domain::models::NodeState;
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
                "No agents active.",
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::ITALIC),
            )),
            Line::from(Span::styled(
                "Ask the assistant to delegate or use plan mode.",
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
                + 2
                + if entry.isolated {
                    super::orchestration_glyph::isolation_glyph()
                        .chars()
                        .count()
                        + 1
                } else {
                    0
                };
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

            // P9 (TUI): ⊙ iso indicator for isolated children (AC3 "shown, not silent").
            if entry.isolated {
                spans.push(Span::styled(
                    format!(" {}", super::orchestration_glyph::isolation_glyph()),
                    Style::default().fg(theme.colors.fg_muted),
                ));
            }

            // Status suffix: trust-building cue for non-obvious states.
            // 17.5b (AC8): a `Waiting` node renders its TYPED reason —
            // `awaiting your answer` for an MCP elicitation — not the static
            // `"waiting"`. An unstamped `Waiting` node (none exist in 17.5b
            // production, but defensive) keeps `"waiting"` unchanged.
            let status_suffix = match entry.current_status {
                NodeState::Suspended => Some("resumable"),
                NodeState::Waiting => Some(match entry.wait_reason {
                    Some(crate::domain::models::WaitReason::AwaitingHumanInput) => {
                        "awaiting your answer"
                    }
                    Some(crate::domain::models::WaitReason::AwaitingSpoke) => "awaiting spoke",
                    Some(crate::domain::models::WaitReason::BudgetPaused) => "budget paused",
                    Some(crate::domain::models::WaitReason::AwaitingUpstreamArtifact) => {
                        "awaiting artifact"
                    }
                    Some(crate::domain::models::WaitReason::AwaitingPeerResponse) => {
                        "awaiting your decision"
                    }
                    None => "waiting",
                }),
                NodeState::Created => Some("queued"),
                _ => None,
            };
            if let Some(suffix) = status_suffix {
                spans.push(Span::styled(
                    format!(" {}", suffix),
                    Style::default()
                        .fg(theme.colors.fg_muted)
                        .add_modifier(Modifier::ITALIC),
                ));
            }

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
    status: NodeState,
    theme: &crate::adapters::tui::theme::Theme,
) -> (&'static str, Color) {
    match status {
        NodeState::Running => (
            super::orchestration_glyph::node_state_glyph(NodeState::Running),
            theme.colors.tool_status_executing,
        ),
        NodeState::Created => (
            super::orchestration_glyph::node_state_glyph(NodeState::Created),
            theme.colors.tool_status_awaiting,
        ),
        NodeState::Waiting => (
            super::orchestration_glyph::node_state_glyph(NodeState::Waiting),
            theme.colors.tool_status_awaiting,
        ),
        NodeState::Suspended => (
            super::orchestration_glyph::node_state_glyph(NodeState::Suspended),
            theme.colors.tool_status_awaiting,
        ),
        NodeState::Completed => (
            super::orchestration_glyph::node_state_glyph(NodeState::Completed),
            theme.colors.tool_status_success,
        ),
        NodeState::Failed => (
            super::orchestration_glyph::node_state_glyph(NodeState::Failed),
            theme.colors.tool_status_error,
        ),
        NodeState::Cancelled => (
            super::orchestration_glyph::node_state_glyph(NodeState::Cancelled),
            theme.colors.tool_status_cancelled,
        ),
    }
}

fn ownership_glyph(kind: OwnershipKind) -> &'static str {
    match kind {
        OwnershipKind::Self_(_) => "\u{2605}",
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

    fn make_entry(name: &str, depth: usize, status: NodeState) -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: name.to_string(),
            spawned_at: 0,
            depth,
            current_status: status,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
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
        let entries = vec![make_entry("code-reviewer", 1, NodeState::Created)];
        let text = render_to_text(&entries, 0, true, 60, 5);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_agent_panel_three_level() {
        let entries = vec![
            make_entry("orchestrator", 1, NodeState::Running),
            make_entry("coder", 2, NodeState::Created),
            make_entry("reviewer", 3, NodeState::Created),
        ];
        let text = render_to_text(&entries, 0, true, 60, 5);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_agent_panel_terminal_states() {
        let entries = vec![
            make_entry("agent-ok", 1, NodeState::Completed),
            make_entry("agent-fail", 1, NodeState::Failed),
            make_entry("agent-killed", 1, NodeState::Cancelled),
        ];
        let text = render_to_text(&entries, 1, true, 60, 5);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn test_focused_row_has_reversed_modifier() {
        let entries = vec![
            make_entry("alpha", 1, NodeState::Created),
            make_entry("beta", 1, NodeState::Running),
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
    // P9 keystone (AC3 "shown, not silent"): an isolated node renders the ⊙ iso
    // indicator; a non-isolated node does not. Kill-criterion: dropping the
    // `isolation_glyph()` render call-site makes the positive assertion RED.
    #[test]
    fn p9_isolated_node_renders_iso_glyph() {
        let mut entry = make_entry("isolated-coder", 1, NodeState::Running);
        entry.isolated = true;
        let text = render_to_text(&[entry], 0, true, 60, 5);
        assert!(
            text.contains("\u{2299} iso"),
            "P9: an isolated node must render the ⊙ iso indicator (AC3 shown, not silent):\n{text}"
        );
        // Negative control: a non-isolated node must NOT render it.
        let plain = make_entry("plain-coder", 1, NodeState::Running);
        let plain_text = render_to_text(&[plain], 0, true, 60, 5);
        assert!(
            !plain_text.contains("\u{2299} iso"),
            "P9: a non-isolated node must not render the iso indicator:\n{plain_text}"
        );
    }

    /// 17.5b (AC8 / DF-14.4-WR-1): a `Waiting` node renders its TYPED reason.
    /// The mutant: stamping the reason but reading the static `"waiting"` —
    /// killed by asserting the typed text appears AND the static string does not.
    #[test]
    fn waiting_node_renders_typed_reason_not_the_static_string() {
        let mut entry = make_entry("mcp-task", 1, NodeState::Waiting);
        entry.wait_reason = Some(crate::domain::models::WaitReason::AwaitingHumanInput);
        let text = render_to_text(&[entry], 0, true, 60, 5);
        assert!(
            text.contains("awaiting your answer"),
            "AC8: a Waiting MCP-task node must render its typed reason:\n{text}"
        );
        assert!(
            !text.contains(" waiting"),
            "AC8: the typed reason must replace the static \"waiting\" string:\n{text}"
        );
    }

    #[test]
    fn peer_response_wait_renders_operator_decision_voice() {
        let mut entry = make_entry("peer-task", 1, NodeState::Waiting);
        entry.wait_reason = Some(crate::domain::models::WaitReason::AwaitingPeerResponse);
        let text = render_to_text(&[entry], 0, true, 60, 5);
        assert!(
            text.contains("awaiting your decision"),
            "peer response waits must name the operator decision:\n{text}"
        );
        assert!(!text.contains("awaiting your answer"));
    }

    /// AC8 no-regression: an UNSTAMPED `Waiting` node still renders the static
    /// `"waiting"` (every existing node today). The mutant: dropping the
    /// `None => \"waiting\"` arm — killed by this positive control.
    #[test]
    fn unstamped_waiting_node_still_renders_waiting() {
        let entry = make_entry("plain-waiter", 1, NodeState::Waiting);
        let text = render_to_text(&[entry], 0, true, 60, 5);
        assert!(
            text.contains("waiting"),
            "AC8: an unstamped Waiting node must keep rendering \"waiting\":\n{text}"
        );
    }
}
