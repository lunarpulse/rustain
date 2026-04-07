use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::adapters::tui::state::CommandPaletteState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::palette::PaletteScope;

/// Maximum number of visible items in the palette result list.
const MAX_VISIBLE_ITEMS: usize = 12;

/// Render the command palette as a centered overlay.
// Covers: UX-DR18 (command palette overlay)
pub fn render(frame: &mut Frame, area: Rect, state: &CommandPaletteState, theme: &Theme) {
    if !state.active {
        return;
    }

    let palette_area = calculate_centered_area(area);

    // Clear the area behind the overlay
    frame.render_widget(Clear, palette_area);

    // Split palette into title bar + input + result list
    let chunks = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Input line with borders
            Constraint::Min(1),    // Result list
        ])
        .split(palette_area);

    let input_area = chunks[0];
    let results_area = chunks[1];

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

    // Render the input box
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(title);

    let input_text = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.colors.accent)),
        Span::styled(
            &state.filter_text,
            Style::default().fg(theme.colors.fg_primary),
        ),
        Span::styled("_", Style::default().fg(theme.colors.fg_muted)),
    ]))
    .block(input_block)
    .style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_surface),
    );

    frame.render_widget(input_text, input_area);

    // Render results
    if state.filtered_entries.is_empty() {
        let empty_block = Block::default()
            .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
            .border_style(Style::default().fg(theme.colors.accent));

        let empty_text = Paragraph::new(Line::from(Span::styled(
            "  No matches",
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        )))
        .block(empty_block)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        );

        frame.render_widget(empty_text, results_area);
        return;
    }

    let visible_count = state
        .filtered_entries
        .len()
        .saturating_sub(state.scroll_offset)
        .min(MAX_VISIBLE_ITEMS);
    let has_more = state.scroll_offset + visible_count < state.filtered_entries.len();

    let results_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(Style::default().fg(theme.colors.accent));

    let results_block = if has_more {
        results_block.title_bottom(format!(
            " {}/{} ",
            state.selected_index + 1,
            state.filtered_entries.len()
        ))
    } else {
        results_block
    };

    let items: Vec<ListItem> = state
        .filtered_entries
        .iter()
        .skip(state.scroll_offset)
        .take(visible_count)
        .map(|entry| format_palette_entry(entry, theme))
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(
        state.selected_index.saturating_sub(state.scroll_offset),
    ));

    let list = List::new(items)
        .block(results_block)
        .highlight_style(
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        )
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        );

    frame.render_stateful_widget(list, results_area, &mut list_state);
}

/// Calculate a centered overlay area within the terminal.
/// Size: 60% width (min 40, max 80), 50% height (min 8, max 20).
fn calculate_centered_area(area: Rect) -> Rect {
    let width = (area.width * 60 / 100).clamp(40, 80).min(area.width);
    let height = (area.height * 50 / 100).clamp(8, 20).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// Format a palette entry as a ListItem.
/// Shows: name (bold, left-aligned), shortcut (dimmed, right-aligned), description (dimmed).
fn format_palette_entry<'a>(
    entry: &crate::domain::models::palette::PaletteEntry,
    theme: &Theme,
) -> ListItem<'a> {
    // Fixed column widths: name fills up to 30 chars, shortcut right-aligned in 16 chars.
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
        let name_len = entry.name.len();
        let padding = if name_len < NAME_WIDTH {
            NAME_WIDTH - name_len
        } else {
            1
        };
        let shortcut_str = format!("{:>width$}", shortcut, width = SHORTCUT_WIDTH);
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

    ListItem::new(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_centered_area() {
        let area = Rect::new(0, 0, 120, 40);
        let centered = calculate_centered_area(area);
        assert_eq!(centered.width, 72); // 60% of 120
        assert_eq!(centered.height, 20); // 50% of 40 = 20 (max)
        assert_eq!(centered.x, 24); // (120-72)/2
        assert_eq!(centered.y, 10); // (40-20)/2
    }

    #[test]
    fn test_calculate_centered_area_small_terminal() {
        let area = Rect::new(0, 0, 60, 16);
        let centered = calculate_centered_area(area);
        assert!(centered.width >= 36 && centered.width <= 60);
        assert!(centered.height >= 8 && centered.height <= 16);
    }

    #[test]
    fn test_calculate_centered_area_respects_max() {
        let area = Rect::new(0, 0, 200, 60);
        let centered = calculate_centered_area(area);
        assert_eq!(centered.width, 80); // capped at 80
        assert_eq!(centered.height, 20); // capped at 20
    }
}
