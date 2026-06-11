use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::adapters::tui::state::ProfileSwitcherState;
use crate::adapters::tui::theme::Theme;
use crate::domain::services::swap_tier::{SwapTier, TransitionPlan};

pub fn render(frame: &mut Frame, area: Rect, state: &ProfileSwitcherState, theme: &Theme) {
    let overlay_area = centered_rect(60, 70, area);
    frame.render_widget(Clear, overlay_area);

    if state.preview.is_some() {
        render_preview(frame, overlay_area, state, theme);
    } else {
        render_list(frame, overlay_area, state, theme);
    }
}

fn render_list(frame: &mut Frame, area: Rect, state: &ProfileSwitcherState, theme: &Theme) {
    let block = Block::default()
        .title(Span::styled(" Profile Switcher ", theme.typography.heading))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .style(Style::default().bg(theme.colors.bg_surface));

    let items: Vec<ListItem> = state
        .profiles
        .iter()
        .enumerate()
        .map(|(i, profile)| {
            let is_active = state.active_index == Some(i);
            let active_marker = if is_active { "● " } else { "  " };
            let name = if profile.preview {
                format!("{} (preview)", profile.name)
            } else {
                profile.name.clone()
            };
            let desc = profile.description.as_deref().unwrap_or("");

            let swatch_color = profile.identity_color.0;

            let line = Line::from(vec![
                Span::raw(active_marker),
                Span::styled("█", Style::default().fg(Color::Indexed(swatch_color))),
                Span::raw(" "),
                Span::raw(name),
                Span::styled(
                    format!("  {}", desc),
                    Style::default().fg(theme.colors.fg_muted),
                ),
            ]);

            if i == state.selected {
                ListItem::new(line).style(Style::default().fg(theme.colors.fg_primary))
            } else {
                ListItem::new(line)
            }
        })
        .collect();

    // Ports-that-differ summary for selected
    let footer = if let Some(selected_profile) = state.selected_profile() {
        if let Some(ref plan) = state.compute_diff_for_selected(selected_profile) {
            if plan.diffs.is_empty() {
                "No differences — already active".to_string()
            } else {
                let names: Vec<&str> = plan.diffs.iter().map(|d| d.port_name()).collect();
                format!(
                    "Δ {} — {}/7 ports change",
                    names.join(", "),
                    plan.diffs.len()
                )
            }
        } else {
            "Unable to compute diff".to_string()
        }
    } else {
        String::new()
    };

    let list_area = if footer.is_empty() || area.height < 4 {
        area
    } else {
        Rect {
            height: area.height.saturating_sub(2),
            ..area
        }
    };

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    frame.render_widget(list, list_area);

    if !footer.is_empty() && area.height >= 4 {
        let footer_area = Rect {
            y: area.y + area.height.saturating_sub(2),
            height: 2,
            ..area
        };
        let footer_para = Paragraph::new(footer).style(Style::default().fg(theme.colors.fg_muted));
        frame.render_widget(footer_para, footer_area);
    }
}

fn render_preview(frame: &mut Frame, area: Rect, state: &ProfileSwitcherState, theme: &Theme) {
    let plan = match state.preview.as_ref() {
        Some(p) => p,
        None => return,
    };

    let block = Block::default()
        .title(Span::styled(
            format!(" Preview: {} ", plan.profile_name),
            theme.typography.heading,
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .style(Style::default().bg(theme.colors.bg_surface));

    let mut lines: Vec<Line> = Vec::new();

    // Section 1: Hot swaps
    let hot: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Hot)
        .collect();
    if !hot.is_empty() {
        lines.push(Line::from(Span::styled(
            "▸ Hot swaps (< 10ms — imperceptible):",
            Style::default().fg(theme.colors.warning),
        )));
        for diff in &hot {
            lines.push(Line::from(format!(
                "  {}: {} → {}",
                diff.port_name(),
                diff.from_adapter,
                diff.to_adapter
            )));
        }
        lines.push(Line::from(""));
    }

    // Section 2: Warm swaps
    let warm: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Warm)
        .collect();
    if !warm.is_empty() {
        lines.push(Line::from(Span::styled(
            "▸ Warm swaps (< 5s — brief pause):",
            Style::default().fg(theme.colors.info),
        )));
        for diff in &warm {
            lines.push(Line::from(format!(
                "  {}: {} → {}  [policy: {:?}]",
                diff.port_name(),
                diff.from_adapter,
                diff.to_adapter,
                diff.policy
            )));
        }
        lines.push(Line::from(""));
    }

    // Section 3: Cold swaps
    let cold: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Cold)
        .collect();
    if !cold.is_empty() {
        lines.push(Line::from(Span::styled(
            "▸ Cold swaps (< 2s — adapter loops restart):",
            Style::default().fg(theme.colors.profile_personal),
        )));
        for diff in &cold {
            lines.push(Line::from(format!(
                "  {}: {} → {}",
                diff.port_name(),
                diff.from_adapter,
                diff.to_adapter
            )));
        }
        lines.push(Line::from(""));
    }

    // Section 4: No changes
    if plan.diffs.is_empty() {
        lines.push(Line::from("No changes — already active"));
        lines.push(Line::from(""));
    }

    // Footer
    lines.push(Line::from(Span::styled(
        format!(
            "Estimated time: ~{}ms  |  [y] confirm  [Esc] cancel",
            plan.estimated_ms
        ),
        Style::default().fg(theme.colors.fg_muted),
    )));

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });

    frame.render_widget(paragraph, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
