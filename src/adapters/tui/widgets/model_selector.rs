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
        .title(Span::styled(" Select Model ", theme.typography.heading));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let mut lines: Vec<Line> = Vec::new();

    // Provider tabs
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

    let mut footer_hint = String::new();

    if let Some(col) = state.columns.get(state.selected_provider) {
        // Empty catalog branch (Preflight Consensus #5)
        if col.models.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("No models match your filter for {}.", col.display_name),
                Style::default().fg(theme.colors.fg_muted),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "Edit {} → [providers.{}] model_filter",
                    crate::infrastructure::paths::config_file_path()
                        .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"))
                        .display(),
                    col.provider_id
                ),
                Style::default().fg(theme.colors.fg_muted),
            )));
        } else {
            let show_search_input = state.search_active && col.models.len() > 10;

            if show_search_input {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("/{}", state.search_query),
                        Style::default().fg(theme.colors.accent),
                    ),
                    Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)),
                ]));
            }

            // Determine iteration source
            let use_filtered = state.search_active && !state.search_query.is_empty();
            let indices: Vec<usize> = if use_filtered {
                state.filtered_indices.clone()
            } else {
                (0..col.models.len()).collect()
            };

            if use_filtered && indices.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("No models match \"{}\"", state.search_query),
                    Style::default().fg(theme.colors.fg_muted),
                )));
            } else {
                for (display_pos, &i) in indices.iter().enumerate() {
                    let model = &col.models[i];
                    let is_selected = display_pos == state.selected_model;
                    let prefix = if is_selected { "▸ " } else { "  " };

                    let mut name_style = if is_selected {
                        Style::default()
                            .fg(theme.colors.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };

                    // Ghost model rendering (Preflight Consensus #9)
                    if model.stale {
                        name_style = name_style
                            .fg(theme.colors.fg_muted)
                            .add_modifier(Modifier::CROSSED_OUT);
                    }

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

            // Footer hint line (Preflight Consensus #7)
            if state.search_active && !state.search_query.is_empty() {
                footer_hint = "Esc clear · Esc Esc close".to_string();
            } else {
                footer_hint = "↑↓ navigate · Enter select · Esc close".to_string();
                if col.models.len() > 10 {
                    footer_hint.push_str(" · / search");
                }
            }

            // Stale-row footer tooltip (Preflight Consensus #9)
            if let Some(&idx) = if use_filtered {
                indices.get(state.selected_model)
            } else {
                Some(&state.selected_model)
            } {
                if col.models.get(idx).is_some_and(|m| m.stale) {
                    footer_hint.push_str(" │ Not in latest catalog — may fail at request time.");
                }
            }
        }

        // Refresh indicator
        if state.refreshing.contains(&col.provider_id) {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("↻ Discovering models for {}...", col.display_name),
                Style::default().fg(theme.colors.fg_muted),
            )));
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

    // Footer hint
    if !footer_hint.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            footer_hint,
            Style::default().fg(theme.colors.fg_muted),
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

    // Story 7.6 AC9 — render tests
    use crate::adapters::tui::state::{ModelSelectorState, ProviderColumn};
    use crate::domain::models::provider::ModelDescriptor;

    fn theme() -> Theme {
        Theme::dark()
    }

    fn buffer_text(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let mut s = String::new();
        let term_size = t.size().unwrap();
        for y in 0..term_size.height {
            for x in 0..term_size.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn empty_catalog_renders_hint() {
        let backend = TestBackend::new(120, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = ModelSelectorState::new();
        state.active = true;
        state.columns = vec![ProviderColumn {
            provider_id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            healthy: true,
            models: vec![],
        }];
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        let txt = buffer_text(&term);
        assert!(
            txt.contains("No models match your filter"),
            "empty catalog should show 'No models' hint: {}",
            txt
        );
        assert!(
            txt.contains("config.toml"),
            "empty catalog should reference config path: {}",
            txt
        );
    }

    #[test]
    fn ghost_row_strikethrough() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = ModelSelectorState::new();
        state.active = true;
        state.columns = vec![ProviderColumn {
            provider_id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            healthy: true,
            models: vec![
                ModelDescriptor {
                    model_id: "live-model".to_string(),
                    display_name: "Live Model".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
                ModelDescriptor {
                    model_id: "ghost-model".to_string(),
                    display_name: "Ghost Model".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: true,
                },
            ],
        }];
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        let buf = term.backend().buffer();
        // Find the ghost model cell and verify CROSSED_OUT modifier is present
        let mut found_ghost = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    if cell.symbol() == "G" && cell.style().add_modifier == Modifier::CROSSED_OUT {
                        found_ghost = true;
                    }
                }
            }
        }
        assert!(
            found_ghost,
            "ghost model row should have CROSSED_OUT modifier"
        );
    }
}
