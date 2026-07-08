use std::collections::HashMap;

use ratatui::{
    prelude::*,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::domain::models::subagent_view::AgentRowView;

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    entry: &AgentRowView,
    scroll_offset: u16,
    pending_kill_confirm: Option<&crate::domain::models::AgentId>,
    theme: &crate::adapters::tui::theme::Theme,
    spool_tail_cache: &HashMap<crate::domain::models::AgentId, String>,
    cached_entries: &[AgentRowView],
    now_unix: i64,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    Clear.render(area, buf);

    let title = format!(" Agent Inspector \u{B7} {} ", entry.subagent_type);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent));
    let inner = block.inner(area);
    block.render(area, buf);

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("Agent: {}", entry.subagent_type),
        Style::default()
            .fg(theme.colors.fg_primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "Model: {}",
            if entry.effective_model.is_empty() {
                "(unresolved)"
            } else {
                &entry.effective_model
            }
        ),
        Style::default().fg(theme.colors.fg_muted),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "Tools: {}",
            if entry.tools_summary.is_empty() {
                "(unresolved)"
            } else {
                &entry.tools_summary
            }
        ),
        Style::default().fg(theme.colors.fg_muted),
    )));
    let spawned_elapsed = crate::domain::services::plan_runtime::format_elapsed_ms(
        (now_unix - entry.spawned_at).max(0),
    );
    lines.push(Line::from(Span::styled(
        format!("Spawned: {} ago", spawned_elapsed),
        Style::default().fg(theme.colors.fg_muted),
    )));
    lines.push(Line::from(Span::raw("")));

    lines.push(Line::from(Span::styled(
        "Current task:",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));

    let tail_text = spool_tail_cache
        .get(&entry.agent_id)
        .map(|s| {
            let last3: Vec<&str> = s
                .lines()
                .rev()
                .take(3)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if last3.is_empty() {
                "(no output yet)".to_string()
            } else {
                last3.join("\n")
            }
        })
        .unwrap_or_else(|| "(no output yet)".to_string());

    for line in tail_text.lines() {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(theme.colors.fg_primary),
        )));
    }

    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "Resource usage:",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "  Tokens: {} in / {} out",
            entry.tokens_in, entry.tokens_out
        ),
        Style::default().fg(theme.colors.fg_muted),
    )));
    static PRICING: std::sync::OnceLock<
        std::collections::HashMap<String, crate::domain::models::pricing::PricingConfig>,
    > = std::sync::OnceLock::new();
    let pricing = PRICING.get_or_init(crate::domain::models::AppConfig::default_pricing_catalog);
    let cost = if entry.effective_model.is_empty() {
        None
    } else {
        crate::domain::services::cost_calculator::cost_for_model_tokens(
            &entry.effective_model,
            entry.tokens_in,
            entry.tokens_out,
            pricing,
        )
    };
    lines.push(Line::from(Span::styled(
        format!(
            "  Cost: {}",
            crate::adapters::tui::widgets::usage_panel::cost_or_na(cost)
        ),
        Style::default().fg(theme.colors.fg_muted),
    )));
    lines.push(Line::from(Span::styled(
        format!("  Turns: {}", entry.turns),
        Style::default().fg(theme.colors.fg_muted),
    )));
    lines.push(Line::from(Span::styled(
        "  Memory: (n/a \u{2014} in-process tier)",
        Style::default().fg(theme.colors.fg_muted),
    )));

    fn collect_children_recursive<'a>(
        cached_entries: &'a [AgentRowView],
        parent_id: &crate::domain::models::AgentId,
        depth: usize,
        out: &mut Vec<(&'a AgentRowView, usize)>,
    ) {
        for entry in cached_entries {
            if &entry.parent_id == parent_id {
                out.push((entry, depth));
                collect_children_recursive(cached_entries, &entry.agent_id, depth + 1, out);
            }
        }
    }

    let mut children: Vec<(&AgentRowView, usize)> = Vec::new();
    collect_children_recursive(cached_entries, &entry.agent_id, 1, &mut children);
    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "Children:",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));
    if children.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        for (child, depth) in &children {
            let indent = "  ".repeat(*depth);
            lines.push(Line::from(Span::styled(
                format!(
                    "{}\u{251C} {} ({:?})",
                    indent, child.subagent_type, child.current_status
                ),
                Style::default().fg(theme.colors.fg_primary),
            )));
        }
    }

    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "Conversation tail:",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));

    let full_tail = spool_tail_cache
        .get(&entry.agent_id)
        .cloned()
        .unwrap_or_default();
    if full_tail.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no output yet)",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        for line in full_tail.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                Style::default().fg(theme.colors.fg_primary),
            )));
        }
    }

    lines.push(Line::from(Span::raw("")));

    if pending_kill_confirm == Some(&entry.agent_id) {
        // Double-bordered kill confirmation card per AC-10-4-7
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            format!(
                "\u{26A0} Kill {} and its subtree?  [y] Confirm   [n/Esc] Cancel",
                entry.subagent_type.replace('{', "{{").replace('}', "}}")
            ),
            Style::default()
                .fg(theme.colors.tool_status_error)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "[p] Pause/Resume   [x] Kill (cascade)   [m] Change model   [t] Update tools   [c] Conversation tab   [Esc] Back",
            Style::default().fg(theme.colors.fg_secondary),
        )));
    }

    let content_height = inner.height as usize;
    let total_lines = lines.len();
    let scroll_offset = scroll_offset as usize;
    let visible_start = scroll_offset.min(total_lines.saturating_sub(content_height));
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(visible_start)
        .take(content_height)
        .collect();

    let para = Paragraph::new(visible_lines).wrap(Wrap { trim: false });
    para.render(inner, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::subagent_view::OwnershipKind;
    use crate::domain::models::{AgentId, NodeState};

    fn make_entry(name: &str, status: NodeState) -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: name.to_string(),
            spawned_at: 0,
            depth: 1,
            current_status: status,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
        }
    }

    /// Fixed clock so the "Spawned: … ago" line is deterministic in snapshots.
    /// With `make_entry`'s `spawned_at: 0`, this renders a stable elapsed value
    /// (the widget no longer reads the wall clock — `now` is injected).
    const MOCK_NOW_UNIX: i64 = 7_200_000;

    fn render_to_text(
        entry: &AgentRowView,
        pending_kill: Option<&AgentId>,
        cache: &HashMap<AgentId, String>,
        width: u16,
        height: u16,
    ) -> String {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(
            area,
            &mut buf,
            entry,
            0,
            pending_kill,
            &theme,
            cache,
            &[],
            MOCK_NOW_UNIX,
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
    fn snapshot_inspector_idle() {
        let entry = make_entry("code-reviewer", NodeState::Created);
        let cache = HashMap::new();
        let text = render_to_text(&entry, None, &cache, 60, 20);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_inspector_running_with_tail() {
        let entry = make_entry("coder", NodeState::Running);
        let mut cache = HashMap::new();
        cache.insert(
            entry.agent_id.clone(),
            "hello\nworld\nworking...".to_string(),
        );
        let text = render_to_text(&entry, None, &cache, 60, 20);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_inspector_kill_confirm() {
        let entry = make_entry("reviewer", NodeState::Running);
        let cache = HashMap::new();
        let text = render_to_text(&entry, Some(&entry.agent_id), &cache, 60, 20);
        insta::assert_snapshot!(text);
    }
}
