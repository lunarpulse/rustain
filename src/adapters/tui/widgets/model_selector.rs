use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::adapters::tui::state::ModelSelectorState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::provider::{DiscoveryStatus, ModelCapability};

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

    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default()
        .fg(theme.colors.accent)
        .add_modifier(Modifier::BOLD);
    let width = inner.width as usize;
    let mut top = Vec::new();
    let mut bottom = Vec::new();
    let mut rows = Vec::new();
    let mut selected_row = 0;
    let mut selected_end = 0;

    if let Some(col) = state.columns.get(state.selected_provider) {
        let status = if col.locked_hint.is_some() {
            "· not signed in"
        } else if col.discovery == Some(DiscoveryStatus::Unauthenticated) {
            "⚠ sign in again"
        } else if col.healthy {
            "✓ connected"
        } else {
            "✗ unavailable"
        };
        top.extend(wrapped(
            &format!(
                "◄ {} {} · {}/{} ►",
                col.display_name,
                status,
                state.selected_provider + 1,
                state.columns.len()
            ),
            accent,
            width,
        ));
        top.push(Line::from(Span::styled("─".repeat(width), muted)));

        if let Some(hint) = &col.locked_hint {
            top.extend(wrapped(
                &format!("{} has no credentials configured", col.display_name),
                muted,
                width,
            ));
            top.extend(wrapped(hint, muted, width));
            top.extend(wrapped(
                &format!("{} models in catalog:", col.models.len()),
                muted,
                width,
            ));
            for (i, model) in col.models.iter().enumerate() {
                if i == state.selected_model {
                    selected_row = rows.len();
                }
                rows.extend(wrapped(&model.model_id, muted, width));
                if i == state.selected_model {
                    selected_end = rows.len();
                }
            }
            bottom.extend(wrapped("←→ provider · Esc close", muted, width));
        } else {
            if state.search_active && col.models.len() > 10 {
                top.extend(wrapped(
                    &format!("/{} ▏", state.search_query),
                    accent,
                    width,
                ));
            }
            let filtered = state.search_active && !state.search_query.is_empty();
            if filtered && state.filtered_indices.is_empty() {
                rows.extend(wrapped(
                    &format!("No models match \"{}\"", state.search_query),
                    muted,
                    width,
                ));
            } else if col.models.is_empty() && col.discovery != Some(DiscoveryStatus::Empty) {
                rows.extend(wrapped(
                    &format!("No models match your filter for {}.", col.display_name),
                    muted,
                    width,
                ));
                rows.extend(wrapped(
                    &format!(
                        "Edit {} → [provider.{}] model_filter",
                        crate::infrastructure::paths::config_file_path()
                            .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"))
                            .display(),
                        col.provider_id
                    ),
                    muted,
                    width,
                ));
            } else {
                let count = if filtered {
                    state.filtered_indices.len()
                } else {
                    col.models.len()
                };
                for position in 0..count {
                    let index = if filtered {
                        state.filtered_indices[position]
                    } else {
                        position
                    };
                    let Some(model) = col.models.get(index) else {
                        continue;
                    };
                    let selected = position == state.selected_model;
                    if selected {
                        selected_row = rows.len();
                    }
                    let mut style = if selected {
                        accent
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    if model.stale {
                        style = style
                            .fg(theme.colors.fg_muted)
                            .add_modifier(Modifier::CROSSED_OUT);
                    }
                    let mut badges = String::new();
                    for (capability, label) in [
                        (ModelCapability::Vision, " vis"),
                        (ModelCapability::ToolUse, " tool"),
                        (ModelCapability::Thinking, " think"),
                        (ModelCapability::ParallelToolCalls, " par"),
                    ] {
                        if model.capabilities.contains(&capability) {
                            badges.push_str(label);
                        }
                    }
                    rows.extend(wrapped(
                        &format!(
                            "{}{} · {} ctx{}",
                            if selected { "▸ " } else { "  " },
                            model.display_name,
                            humanize_ctx(model.context_window),
                            badges
                        ),
                        style,
                        width,
                    ));
                    if selected {
                        selected_end = rows.len();
                    }
                }
            }

            let discovery = match col.discovery {
                Some(DiscoveryStatus::Unauthenticated) => Some(format!(
                    "Provider requires authentication before models can be discovered. {}",
                    col.sign_in_hint
                        .clone()
                        .unwrap_or_else(|| format!("Run `rustain auth login {}`", col.provider_id))
                )),
                Some(DiscoveryStatus::Cached) => {
                    Some("Using cached model list; live refresh failed.".into())
                }
                Some(DiscoveryStatus::Unavailable) => {
                    Some("Provider unavailable — showing bundled models.".into())
                }
                Some(DiscoveryStatus::Empty) => {
                    Some("Discovery succeeded but returned 0 models.".into())
                }
                _ => None,
            };
            if let Some(status) = discovery {
                bottom.extend(wrapped(&status, muted, width));
            }
            if state.selected().is_some_and(|(_, model)| model.stale) {
                bottom.extend(wrapped(
                    "Credential rejected — requests will fail until you sign in again.",
                    muted,
                    width,
                ));
            }
            let mut footer = if filtered {
                "Esc clear · Esc Esc close".to_string()
            } else if col.models.is_empty() {
                "←→ provider · Esc close".to_string()
            } else {
                "↑↓ navigate · Enter select · Esc close".to_string()
            };
            if !filtered && col.models.len() > 10 {
                footer.push_str(" · / search");
            }
            bottom.extend(wrapped(&footer, muted, width));
        }
        if state.refreshing.contains(&col.provider_id) {
            bottom.extend(wrapped(
                &format!("↻ Discovering models for {}...", col.display_name),
                muted,
                width,
            ));
        }
    }
    if let Some(provider) = &state.connecting {
        bottom.extend(wrapped(
            &format!("● Connecting to {}...", provider),
            Style::default().fg(theme.colors.warning),
            width,
        ));
    }
    if let Some(warning) = &state.pending_context_warning {
        bottom.extend(wrapped(
            &format!(
                "⚠ Current context ({} tokens) exceeds {} limit ({}). Compact context? [y/n]",
                humanize_ctx(warning.current_tokens),
                warning.model_display_name,
                humanize_ctx(warning.context_window)
            ),
            Style::default().fg(theme.colors.warning),
            width,
        ));
    }

    // Reserve chrome and wrapped notices before sizing the scrolling catalogue.
    let row_count = (inner.height as usize).saturating_sub(top.len() + bottom.len());
    // Show the selected entry's last wrapped line, but never scroll past its first.
    let start = selected_end.saturating_sub(row_count).min(selected_row);
    let mut lines = top;
    lines.extend(rows.into_iter().skip(start).take(row_count));
    lines.extend(bottom);
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
}

/// Word-wrap metadata by terminal cells; oversized IDs use grapheme-safe wrapping.
fn wrapped(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut line = String::new();
        let mut cells = 0;
        for word in logical.split_whitespace() {
            let word_cells = word.width();
            if cells > 0 && cells + 1 + word_cells > width {
                lines.push(Line::from(Span::styled(std::mem::take(&mut line), style)));
                cells = 0;
            }
            if word_cells > width {
                let (parts, _) = super::delimited_body::wrap_text(word, width, usize::MAX);
                lines.extend(
                    parts
                        .into_iter()
                        .map(|part| Line::from(Span::styled(part, style))),
                );
            } else {
                if cells > 0 {
                    line.push(' ');
                    cells += 1;
                }
                line.push_str(word);
                cells += word_cells;
            }
        }
        if !line.is_empty() || logical.is_empty() {
            lines.push(Line::from(Span::styled(line, style)));
        }
    }
    lines
}

fn calculate_centered_area(area: Rect) -> Rect {
    let width = (area.width * 60 / 100).clamp(40, 80).min(area.width);
    let height = area.height.saturating_sub(4).clamp(8, 20).min(area.height);
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
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
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
    fn rejected_credential_row_strikethrough() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = ModelSelectorState::new();
        state.active = true;
        state.columns = vec![ProviderColumn {
            provider_id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![
                // Kept visible after credential rejection, not a removed live ID.
                ModelDescriptor {
                    model_id: "live-model".to_string(),
                    display_name: "Live Model".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
                ModelDescriptor {
                    model_id: "ghost-model".to_string(),
                    display_name: "Ghost Model".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: true,
                    wire_model_id: None,
                },
            ],
        }];
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        let buf = term.backend().buffer();
        // A rejected credential keeps its models visibly stale.
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
            "rejected-credential model row should have CROSSED_OUT modifier"
        );
    }

    fn catalog_column(id: &str, display: &str, count: usize) -> ProviderColumn {
        ProviderColumn {
            provider_id: id.into(),
            display_name: display.into(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: (0..count)
                .map(|i| ModelDescriptor {
                    model_id: format!("catalog-model-{i}"),
                    display_name: format!("Catalog Model {i}"),
                    provider_id: id.into(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                })
                .collect(),
        }
    }

    fn inner_text(term: &Terminal<TestBackend>) -> String {
        let modal = calculate_centered_area(Rect::new(0, 0, 80, 24));
        let inner = modal.inner(Margin::new(1, 1));
        let buffer = term.backend().buffer();
        let mut text = String::new();
        for y in inner.y..inner.bottom() {
            for x in inner.x..inner.right() {
                text.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            text.push('\n');
            assert_eq!(buffer.cell((modal.x, y)).unwrap().symbol(), "│");
            assert_eq!(buffer.cell((modal.right() - 1, y)).unwrap().symbol(), "│");
        }
        text
    }

    #[test]
    fn locked_hint_catalog_and_single_header_are_fully_visible_at_80x24() {
        use crate::domain::models::FocusState;
        let mut state = ModelSelectorState::new();
        let mut locked = catalog_column("anthropic", "Anthropic", 5);
        locked.locked_hint = Some(
            "Set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY or run `rustain auth login anthropic`"
                .into(),
        );
        locked.discovery = Some(DiscoveryStatus::Unauthenticated);
        state.open(
            FocusState::Input,
            vec![
                catalog_column("available", "Other Provider", 1),
                locked,
                catalog_column("another", "Another Provider", 1),
            ],
            "anthropic",
            "catalog-model-0",
        );
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|frame| render(frame, frame.area(), &state, &theme()))
            .unwrap();
        let text = inner_text(&term);
        let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for expected in [
            "Anthropic · not signed in · 2/3 ►",
            "Anthropic has no credentials configured",
            "ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY",
            "rustain auth login anthropic",
            "5 models in catalog:",
            "catalog-model-4",
            "←→ provider · Esc close",
        ] {
            assert!(plain.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("Other Provider"));
        assert!(!text.contains("Another Provider"));
        assert!(!text.contains("sign in again"));
        assert!(!text.contains("▸"));
        assert!(!text.contains("Enter select"));
    }

    #[test]
    fn available_discovery_statuses_and_footer_survive_long_catalog_at_80x24() {
        use crate::domain::models::FocusState;
        for (status, expected) in [
            (
                DiscoveryStatus::Unauthenticated,
                "Provider requires authentication before models can be discovered.",
            ),
            (
                DiscoveryStatus::Cached,
                "Using cached model list; live refresh failed.",
            ),
            (
                DiscoveryStatus::Unavailable,
                "Provider unavailable — showing bundled models.",
            ),
            (
                DiscoveryStatus::Empty,
                "Discovery succeeded but returned 0 models.",
            ),
        ] {
            let mut column = catalog_column("openrouter", "OpenRouter", 12);
            column.discovery = Some(status);
            if status == DiscoveryStatus::Empty {
                column.models.clear();
            }
            if status == DiscoveryStatus::Unauthenticated {
                for model in &mut column.models {
                    model.stale = true;
                }
            }
            let mut state = ModelSelectorState::new();
            state.open(
                FocusState::Input,
                vec![column],
                "openrouter",
                "catalog-model-11",
            );
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|frame| render(frame, frame.area(), &state, &theme()))
                .unwrap();
            let text = inner_text(&term);
            let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(plain.contains(expected), "{status:?}: {text}");
            assert!(plain.contains("· 1/1 ►"), "{text}");
            assert!(plain.contains("Esc close"), "{text}");
            if status == DiscoveryStatus::Unauthenticated {
                assert!(plain.contains("⚠ sign in again"), "{text}");
                assert!(plain.contains("rustain auth login openrouter"), "{text}");
                assert!(
                    plain.contains(
                        "Credential rejected — requests will fail until you sign in again."
                    ),
                    "{text}"
                );
            }
            if status == DiscoveryStatus::Empty {
                assert!(!text.contains("model_filter"));
                assert!(!text.contains("No models match"));
            } else {
                assert!(plain.contains("▸ Catalog Model 11"), "{text}");
            }
        }
    }
}
