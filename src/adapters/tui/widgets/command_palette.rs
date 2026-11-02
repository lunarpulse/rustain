use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::modal::{ModalPart, fit_modal_indexed};
use crate::adapters::tui::keymap::{Context, render as keys, table_overlay::OverlayCmd};
use crate::adapters::tui::state::CommandPaletteState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::palette::PaletteScope;

/// Maximum number of visible items in the palette result list.
const MAX_VISIBLE_ITEMS: usize = 12;

/// Render the palette into the frame rectangle supplied by compose.
// Covers: UX-DR18 (command palette overlay)
pub fn render(frame: &mut Frame, area: Rect, state: &CommandPaletteState, theme: &Theme) {
    // Build title based on scope; suppress scope header when scope has no entries (AC8, Task 4.7)
    let scope_has_entries = state.current_scope.is_none() || !state.filtered_entries.is_empty();
    let title = if scope_has_entries {
        match state.current_scope {
            Some(PaletteScope::SlashCommand) => " Commands / ",
            Some(PaletteScope::FileMention) => " Files @ ",
            Some(PaletteScope::Model) => " Models : ",
            Some(PaletteScope::Profile) => " Profiles > ",
            Some(PaletteScope::Adapter) => " Adapters ! ",
            Some(PaletteScope::All) | None => " Command Palette ",
        }
    } else {
        " Command Palette "
    };

    let selected = state
        .selected_index
        .min(state.filtered_entries.len().saturating_sub(1));
    let start = state
        .scroll_offset
        .min(selected)
        .max(selected.saturating_sub(MAX_VISIBLE_ITEMS - 1));
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(title);
    let inner = block.inner(area);
    let visible_count = usize::from(inner.height.saturating_sub(2)).min(MAX_VISIBLE_ITEMS);
    if start + visible_count < state.filtered_entries.len() {
        block = block.title_bottom(format!(
            " {}/{} ",
            selected + 1,
            state.filtered_entries.len()
        ));
    }
    let mut query = Some(Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.colors.accent)),
        Span::styled(
            state.filter_text.as_str(),
            Style::default().fg(theme.colors.fg_primary),
        ),
        Span::styled("_", Style::default().fg(theme.colors.fg_muted)),
    ]));
    let rows = if state.filtered_entries.is_empty() {
        vec![Line::from(Span::styled(
            "  No matches",
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        ))]
    } else {
        state
            .filtered_entries
            .iter()
            .skip(start)
            .take(MAX_VISIBLE_ITEMS)
            .enumerate()
            .map(|(index, entry)| {
                let mut line = format_palette_entry(entry, theme);
                if start + index == selected {
                    let highlight = Style::default()
                        .fg(theme.colors.accent)
                        .add_modifier(Modifier::BOLD);
                    for span in &mut line.spans {
                        span.style = span.style.patch(highlight);
                    }
                }
                line
            })
            .collect()
    };
    let footer = state
        .refusal
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| {
            std::borrow::Cow::Owned(format!(
                "{} move  {} run  {} close",
                keys::rail_pair(
                    Context::CommandPalette,
                    OverlayCmd::PaletteUp,
                    OverlayCmd::PaletteDown
                ),
                keys::rail_key(Context::CommandPalette, OverlayCmd::PaletteRun),
                keys::rail_key(Context::CommandPalette, OverlayCmd::PaletteClose),
            ))
        });
    let selected_row = selected.saturating_sub(start);
    let lines = fit_modal_indexed(
        vec![
            ModalPart::Header(vec![Line::default()]),
            ModalPart::Rows {
                lines: rows,
                selected: (selected_row, selected_row + 1),
            },
            ModalPart::Footer(vec![Line::from(Span::styled(
                footer,
                Style::default().fg(theme.colors.fg_muted),
            ))]),
        ],
        usize::from(inner.height),
        usize::from(inner.width),
    );
    // Reserve the query row through the shared fit, then borrow its text.
    let lines = lines
        .into_iter()
        .map(|(part, _, line)| {
            if part == 0 {
                query.take().expect("one query row")
            } else {
                line
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
}

/// Format a result row, preserving the display-cell aligned shortcut column.
fn format_palette_entry(
    entry: &crate::domain::models::palette::PaletteEntry,
    theme: &Theme,
) -> Line<'static> {
    // Fixed column widths in display cells: name fills up to 30 cells, shortcut
    // right-aligned in 16 cells.
    const NAME_WIDTH: usize = 30;
    const SHORTCUT_WIDTH: usize = 16;

    let name_span = Span::styled(
        entry.name.clone(),
        Style::default()
            .fg(theme.colors.fg_primary)
            .add_modifier(Modifier::BOLD),
    );

    let mut spans = vec![name_span];

    // Right-align shortcut within SHORTCUT_WIDTH (AC7, Task 4.3)
    if let Some(ref shortcut) = entry.shortcut {
        // Pad name to NAME_WIDTH, then right-align shortcut in SHORTCUT_WIDTH columns
        let name_cells = geom::cells(&entry.name);
        let padding = if name_cells < NAME_WIDTH {
            NAME_WIDTH - name_cells
        } else {
            1
        };
        let shortcut_str = geom::pad_left(shortcut, SHORTCUT_WIDTH);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(
            shortcut_str,
            Style::default().fg(theme.colors.fg_muted),
        ));
    }

    // Add description
    if !entry.description.is_empty() {
        spans.push(Span::styled(
            format!("  {}", entry.description),
            Style::default().fg(theme.colors.fg_secondary),
        ));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_geometry_small_palette_keeps_query_selection_and_footer() {
        assert_palette_fit(Rect::new(0, 0, 60, 5));
    }

    #[test]
    fn test_shared_geometry_wide_palette_keeps_query_selection_and_footer() {
        assert_palette_fit(Rect::new(0, 0, 130, 30));
    }

    #[test]
    fn test_render_respects_caller_rectangle() {
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        let area = Rect::new(7, 3, 80, 20);
        terminal
            .draw(|frame| render(frame, area, &CommandPaletteState::new(), &Theme::dark()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(area.x, area.y)].symbol(), "┌");
        assert_eq!(buffer[(area.right() - 1, area.bottom() - 1)].symbol(), "┘");
        assert_eq!(buffer[(0, 0)].symbol(), " ");
    }

    #[test]
    fn test_depth_one_scoped_titles_and_empty_scope_fallback_are_unchanged() {
        for (scope, title) in [
            (None, "Command Palette"),
            (Some(PaletteScope::All), "Command Palette"),
            (Some(PaletteScope::SlashCommand), "Commands /"),
            (Some(PaletteScope::FileMention), "Files @"),
            (Some(PaletteScope::Model), "Models :"),
            (Some(PaletteScope::Profile), "Profiles >"),
            (Some(PaletteScope::Adapter), "Adapters !"),
        ] {
            for populated in [false, true] {
                let mut state = CommandPaletteState::new();
                state.current_scope = scope;
                if populated {
                    state.filtered_entries.push(model_entry("model"));
                }
                let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
                terminal
                    .draw(|frame| render(frame, frame.area(), &state, &Theme::dark()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let top = (0..80).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
                let expected = if populated { title } else { "Command Palette" };
                assert!(top.contains(&format!(" {expected} ")), "{top}");
            }
        }
    }

    fn assert_palette_fit(region: Rect) {
        use crate::adapters::tui::presentation::geometry;

        let area = geometry::frame_rect(region);
        let mut state = CommandPaletteState::new();
        state.filter_text = "needle".to_owned();
        state.filtered_entries = (0..20)
            .map(|index| model_entry(&format!("model-{index}")))
            .collect();
        state.selected_index = 15;
        state.scroll_offset = 4;
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, area, &state, &Theme::dark()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(area.y).contains("Command Palette"));
        assert!(row(area.y + 1).contains("> needle_"));
        let text = (area.y + 2..area.bottom() - 1)
            .map(row)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("model-15"), "{text}");
        let footer = format!(
            "{} move  {} run  {} close",
            keys::rail_pair(
                Context::CommandPalette,
                OverlayCmd::PaletteUp,
                OverlayCmd::PaletteDown
            ),
            keys::rail_key(Context::CommandPalette, OverlayCmd::PaletteRun),
            keys::rail_key(Context::CommandPalette, OverlayCmd::PaletteClose)
        );
        assert!(text.contains(&footer), "{text}");
        assert!(row(area.bottom() - 1).contains("16/20"));
        if region.width >= 100 {
            assert!(text.contains("200K ctx"), "{text}");
        }
        assert!((area.y + 2..area.bottom() - 1).any(|y| {
            row(y).contains("model-15")
                && buffer[(area.x + 1, y)].style().fg == Some(Theme::dark().colors.accent)
        }));
        assert_eq!(state.selected_index, 15);
        assert_eq!(state.scroll_offset, 4);
    }

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────
    // The production constructors (`event_loop`) register external model
    // display names with `Ctrl+X, M` and active profile names with
    // `(active)`; these tests paint the same strings through the real
    // renderer.

    use crate::domain::models::palette::{PaletteAction, PaletteEntry};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";

    fn model_entry(name: &str) -> PaletteEntry {
        PaletteEntry {
            name: name.to_string(),
            description: "200K ctx".to_string(),
            shortcut: Some("Ctrl+X, M".to_string()),
            scope: PaletteScope::Model,
            action: PaletteAction::SwitchModel {
                provider_id: "p".to_string(),
                model_id: "m".to_string(),
            },
        }
    }

    /// Paint the real palette in an explicit caller rectangle matching the old
    /// fixture's text origin, isolating cell-width equivalence from geometry.
    fn shortcut_column(entry: PaletteEntry) -> (u16, String) {
        let shortcut = entry.shortcut.clone().unwrap();
        let mut state = CommandPaletteState::new();
        state.reset();
        state.filtered_entries = vec![entry];
        let theme = Theme::dark();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(20, 8, 60, 15), &state, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut glyphs: Vec<(u16, String)> = Vec::new();
        let mut x = 0u16;
        while x < 100 {
            let symbol = buffer.cell((x, 10)).unwrap().symbol().to_string();
            let width = u16::try_from(geom::cells(&symbol).max(1)).unwrap();
            glyphs.push((x, symbol));
            x += width;
        }
        let row: String = glyphs.iter().map(|(_, s)| s.as_str()).collect();
        for i in 0..glyphs.len() {
            let tail: String = glyphs[i..].iter().map(|(_, s)| s.as_str()).collect();
            if tail.starts_with(&shortcut) {
                return (glyphs[i].0, row);
            }
        }
        panic!("shortcut {shortcut:?} not painted: {row:?}");
    }

    #[test]
    fn display_width_palette_model_shortcut_column_matches_ascii_control() {
        // Name column is 30 cells wide from x = 21; the 9-cell shortcut is
        // right-aligned in 16 cells, so it starts at 21 + 30 + 7 = 58.
        let cases: Vec<(String, u16)> = vec![
            // ASCII control.
            ("gpt-4 (openai)".to_string(), 58),
            // Unicode display names, 13 / 12 / 22 / 28 / 29 cells.
            ("模型 (openai)".to_string(), 58),
            (format!("{G} (p)"), 58),
            (format!("{A}{A}{A}"), 58),
            (C.repeat(14), 58),
            (format!("{}x", C.repeat(14)), 58),
            // Name at / beyond the name column: one separating cell.
            (C.repeat(15), 59),
            (C.repeat(16), 61),
            (format!("{}x", G.repeat(15)), 60),
        ];
        for (name, column) in cases {
            let (found, row) = shortcut_column(model_entry(&name));
            assert_eq!(found, column, "name {name:?}: {row:?}");
        }
    }
}
