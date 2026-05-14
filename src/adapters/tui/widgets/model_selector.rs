use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::ModelSelectorState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::provider::ModelCapability;

pub fn render(frame: &mut Frame, area: Rect, state: &ModelSelectorState, theme: &Theme) {
    if !state.active {
        return;
    }

    let modal_area = calculate_centered_area(area);
    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(" Select Model ", theme.typography.heading))
        .title_bottom(Span::styled(
            " Enter: select  Esc: close ",
            Style::default().fg(theme.colors.fg_muted),
        ));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let mut lines: Vec<Line> = Vec::new();

    if !state.columns.is_empty() {
        let show_arrows = state.columns.len() > 1;
        let mut tab_parts: Vec<Span> = Vec::new();
        if show_arrows {
            tab_parts.push(Span::styled(
                "◄ ",
                Style::default().fg(theme.colors.fg_muted),
            ));
        }
        for (i, col) in state.columns.iter().enumerate() {
            if i > 0 {
                tab_parts.push(Span::styled(
                    " │ ",
                    Style::default().fg(theme.colors.fg_muted),
                ));
            }
            let (status_sym, status_text) = if col.healthy {
                ("✓", "connected")
            } else {
                ("✗", "unavailable")
            };
            let is_selected = i == state.selected_provider;
            let name_style = if is_selected {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.colors.fg_primary)
            };
            tab_parts.push(Span::styled(
                format!("{} {} {}", col.display_name, status_sym, status_text),
                name_style,
            ));
        }
        if show_arrows {
            tab_parts.push(Span::styled(
                " ►",
                Style::default().fg(theme.colors.fg_muted),
            ));
        }
        lines.push(Line::from(tab_parts));
    }

    lines.push(Line::from(Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(theme.colors.fg_muted),
    )));

    if let Some(col) = state.columns.get(state.selected_provider) {
        for (i, model) in col.models.iter().enumerate() {
            let is_selected = i == state.selected_model;
            let prefix = if is_selected { "▸ " } else { "  " };

            let name_style = if is_selected {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };

            let ctx = humanize_ctx(model.context_window);
            let ctx_str = format!("{} ctx", ctx);

            let mut badges: Vec<&'static str> = Vec::new();
            if model.capabilities.contains(&ModelCapability::Vision) {
                badges.push("vis");
            }
            if model.capabilities.contains(&ModelCapability::ToolUse) {
                badges.push("tool");
            }
            if model.capabilities.contains(&ModelCapability::Thinking) {
                badges.push("think");
            }
            if model
                .capabilities
                .contains(&ModelCapability::ParallelToolCalls)
            {
                badges.push("par");
            }
            let badge_str = badges.join(" ");

            let name_text = format!("{}{}", prefix, model.display_name);

            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::styled(name_text.clone(), name_style));

            let name_len = name_text.len();
            let right_start = inner.width as usize;
            if right_start > name_len {
                spans.push(Span::raw(" ".repeat(right_start - name_len)));
            }
            spans.push(Span::styled(
                ctx_str,
                Style::default().fg(theme.colors.fg_muted),
            ));
            if !badge_str.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    badge_str,
                    Style::default()
                        .fg(theme.colors.fg_secondary)
                        .add_modifier(Modifier::DIM),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    if let Some(ref provider_id) = state.connecting {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("● Connecting to {}...", provider_id),
            Style::default().fg(theme.colors.warning),
        )));
    }

    if let Some(ref warning) = state.pending_context_warning {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!(
                "⚠ Current context ({} tokens) exceeds {} limit ({}). Compact context? [y/n]",
                humanize_ctx(warning.current_tokens),
                warning.model_display_name,
                humanize_ctx(warning.context_window)
            ),
            Style::default().fg(theme.colors.warning),
        )));
    }

    let content = Paragraph::new(lines).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_surface),
    );
    frame.render_widget(content, inner);
}

fn calculate_centered_area(area: Rect) -> Rect {
    let width = (area.width * 60 / 100).clamp(40, 80).min(area.width);
    let height = (area.height * 50 / 100).clamp(8, 20).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

pub(crate) fn humanize_ctx(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        format!("{}", tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_theme() -> Theme {
        Theme::dark()
    }

    fn make_state() -> ModelSelectorState {
        ModelSelectorState::new()
    }

    #[test]
    fn test_render_no_crash_80x24() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = make_state();
        state.active = true;
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme);
            })
            .unwrap();
    }

    #[test]
    fn test_render_no_crash_120x40() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = make_state();
        state.active = true;
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme);
            })
            .unwrap();
    }

    #[test]
    fn test_calculate_centered_area() {
        let area = Rect::new(0, 0, 120, 40);
        let centered = calculate_centered_area(area);
        assert_eq!(centered.width, 72);
        assert_eq!(centered.height, 20);
        assert_eq!(centered.x, 24);
        assert_eq!(centered.y, 10);
    }

    #[test]
    fn test_calculate_centered_area_min() {
        let area = Rect::new(0, 0, 60, 16);
        let centered = calculate_centered_area(area);
        assert!(centered.width >= 36);
        assert!(centered.height >= 8);
    }
}
