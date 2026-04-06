use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};

use crate::adapters::tui::state::AutocompleteState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

/// Maximum number of visible items in the popup.
const MAX_VISIBLE_ITEMS: usize = 8;

/// Render the autocomplete popup above the input box.
// Covers: UX-DR75 (inline autocomplete)
pub fn render(frame: &mut Frame, input_area: Rect, state: &AutocompleteState, theme: &Theme) {
    if !state.active {
        return;
    }

    let title = match state.kind {
        AutocompleteKind::SlashCommand => " Commands ",
        AutocompleteKind::FileMention => " Files ",
    };

    if state.suggestions.is_empty() {
        // "No matches" state
        let popup_height = 3u16; // 1 line + 2 borders
        let popup_area = calculate_popup_area(input_area, popup_height);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.colors.fg_muted))
            .title(title);

        let content = Paragraph::new(Line::from(Span::styled(
            "  No matches",
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        )))
        .block(block)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        );

        frame.render_widget(Clear, popup_area);
        frame.render_widget(content, popup_area);
        return;
    }

    let visible_count = state.suggestions.len().min(MAX_VISIBLE_ITEMS);
    let popup_height = visible_count as u16 + 2; // +2 for borders
    let popup_area = calculate_popup_area(input_area, popup_height);

    let has_more = state.suggestions.len() > MAX_VISIBLE_ITEMS;
    let title_with_scroll = if has_more {
        format!("{} [{}/{}]", title.trim(), state.selected_index + 1, state.suggestions.len())
    } else {
        title.to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.tool_border_expanded))
        .title(format!(" {} ", title_with_scroll.trim()));

    let items: Vec<ListItem> = state
        .suggestions
        .iter()
        .skip(state.scroll_offset)
        .take(MAX_VISIBLE_ITEMS)
        .map(|suggestion| format_suggestion(suggestion, theme))
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(state.selected_index.saturating_sub(state.scroll_offset)));

    let list = List::new(items)
        .block(block)
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

    frame.render_widget(Clear, popup_area);
    frame.render_stateful_widget(list, popup_area, &mut list_state);
}

/// Calculate popup area positioned above the input box.
fn calculate_popup_area(input_area: Rect, popup_height: u16) -> Rect {
    let popup_width = input_area.width.min(60);
    Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(popup_height + 1),
        width: popup_width,
        height: popup_height,
    }
}

/// Format a suggestion as a ListItem with styled name and description.
fn format_suggestion<'a>(suggestion: &AutocompleteSuggestion, theme: &Theme) -> ListItem<'a> {
    match suggestion {
        AutocompleteSuggestion::SlashCommand { name, description } => {
            let line = Line::from(vec![
                Span::styled(
                    format!("/{}", name),
                    Style::default()
                        .fg(theme.colors.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", description),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
            ]);
            ListItem::new(line)
        }
        AutocompleteSuggestion::FilePath { path, .. } => {
            let display = path.clone();
            let line = Line::from(Span::styled(
                display,
                Style::default().fg(theme.colors.fg_primary),
            ));
            ListItem::new(line)
        }
    }
}

use ratatui::widgets::Paragraph;
