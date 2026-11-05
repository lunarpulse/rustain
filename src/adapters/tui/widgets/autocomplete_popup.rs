use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};

use crate::adapters::tui::state::AutocompleteState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

/// Maximum number of visible items in the popup.
const MAX_VISIBLE_ITEMS: usize = 8;

/// Render the autocomplete popup above the input box.
/// Returns its painted outer rectangle, or `None` when no cells are painted.
// Covers: UX-DR75 (inline autocomplete)
pub fn render(
    frame: &mut Frame,
    input_area: Rect,
    state: &AutocompleteState,
    theme: &Theme,
) -> Option<Rect> {
    let kind = state.kind?;

    let title = match kind {
        AutocompleteKind::SlashCommand => " Commands ",
        AutocompleteKind::FileMention => " Files ",
        AutocompleteKind::AgentMention => " Agents @ ",
        AutocompleteKind::McpMention => " MCP Tools @ ",
        AutocompleteKind::A2aMention => " A2A Agents @ ",
    };

    if state.suggestions.is_empty() {
        // "No matches" state
        let popup_height = 3u16; // 1 line + 2 borders
        let popup_area = calculate_popup_area(input_area, frame.area(), popup_height);

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
        return (!popup_area.is_empty()).then_some(popup_area);
    }

    let visible_count = state.suggestions.len().min(MAX_VISIBLE_ITEMS);
    let popup_height = visible_count as u16 + 2; // +2 for borders
    let popup_area = calculate_popup_area(input_area, frame.area(), popup_height);

    let has_more = state.suggestions.len() > MAX_VISIBLE_ITEMS;
    let title_with_scroll = if has_more {
        format!(
            "{} [{}/{}]",
            title.trim(),
            state.selected_index + 1,
            state.suggestions.len()
        )
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
    list_state.select(Some(
        state.selected_index.saturating_sub(state.scroll_offset),
    ));

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
    (!popup_area.is_empty()).then_some(popup_area)
}

/// Calculate the popup's outer rectangle above the composer.
///
/// Host-neutral: both the local and attached hosts pass the composer `Rect`
/// they actually laid out (including any sidebar-reduced width or nonzero x)
/// and the `drawable` rectangle (the terminal/frame area) the popup must stay
/// inside. The outer width follows the composer width — there is no fixed cap
/// — and the result is clamped wholly inside `drawable`.
pub fn calculate_popup_area(input_area: Rect, drawable: Rect, popup_height: u16) -> Rect {
    let x = input_area.x.max(drawable.x).min(drawable.right());
    let right = input_area.right().min(drawable.right()).max(x);
    let height = popup_height.min(drawable.height);
    let y = input_area
        .y
        .saturating_sub(popup_height.saturating_add(1))
        .max(drawable.y);
    Rect {
        x,
        y,
        width: right - x,
        height,
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
        AutocompleteSuggestion::Skill { name, description } => {
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
        AutocompleteSuggestion::AgentMention { name, description } => {
            let line = Line::from(vec![
                Span::styled(
                    format!("@Agents/{}", name),
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
        AutocompleteSuggestion::McpTool {
            server,
            name,
            description,
        } => {
            let line = Line::from(vec![
                Span::styled(
                    format!("[{}] {}", server, name),
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
        AutocompleteSuggestion::A2aAgent {
            peer,
            name,
            description,
        } => {
            let peer = crate::domain::services::peer_text::sanitize_peer_text_line(peer);
            let name = crate::domain::services::peer_text::sanitize_peer_text_line(name);
            let description =
                crate::domain::services::peer_text::sanitize_peer_text_line(description);
            let line = Line::from(vec![
                Span::styled(
                    format!("[a2a] {peer} {name}"),
                    Style::default()
                        .fg(theme.colors.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {description}"),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
            ]);
            ListItem::new(line)
        }
    }
}

use ratatui::widgets::Paragraph;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2a_suggestion_sanitizes_remote_fields_before_composing_the_local_row() {
        let theme = Theme::dark();
        let item = format_suggestion(
            &AutocompleteSuggestion::A2aAgent {
                peer: "peer\x1b[31m-red\x1b[0m".to_owned(),
                name: "skill\r\n[urgent]".to_owned(),
                description: "description\x1b]0;forged\x07".to_owned(),
            },
            &theme,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(ratatui::widgets::List::new(vec![item]), frame.area())
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(
            rendered.contains("[a2a] peer-red skill[urgent]"),
            "{rendered}"
        );
        assert!(rendered.contains("description"), "{rendered}");
        assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
        assert!(!rendered.contains("[urgent] peer"), "{rendered}");
    }

    fn slash_state(count: usize, selected: usize, scroll: usize) -> AutocompleteState {
        let mut state = AutocompleteState::new();
        state.open(
            AutocompleteKind::SlashCommand,
            0,
            (0..count)
                .map(|i| AutocompleteSuggestion::SlashCommand {
                    name: format!("cmd{i}"),
                    description: "d".to_owned(),
                })
                .collect(),
        );
        state.selected_index = selected;
        state.scroll_offset = scroll;
        state
    }

    /// Paint the popup and return `(rows, left, right)` of its outer border,
    /// plus the whole buffer text.
    fn paint(
        term_width: u16,
        composer: Rect,
        state: &AutocompleteState,
    ) -> (u16, u16, u16, String) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(term_width, 24)).unwrap();
        let theme = Theme::dark();
        terminal
            .draw(|frame| {
                let _ = render(frame, composer, state, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (mut top, mut left, mut right) = (None, None, None);
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                if symbol == "┌" && top.is_none() {
                    top = Some(y);
                    left = Some(x);
                }
                if symbol == "┐" && right.is_none() {
                    right = Some(x);
                }
            }
            text.push('\n');
        }
        (
            top.expect("popup top-left corner painted"),
            left.unwrap(),
            right.expect("popup top-right corner painted"),
            text,
        )
    }

    #[test]
    fn story21_2_slash_popup_outer_width_follows_composer_at_60_80_130_with_nonzero_x() {
        let mut widths = Vec::new();
        for term_width in [60u16, 80, 130] {
            // Composer offset from the left edge and narrowed by a right sidebar.
            let composer = Rect::new(3, 18, term_width - 3 - 9, 4);
            let (_, left, right, _) = paint(term_width, composer, &slash_state(3, 0, 0));
            assert_eq!(left, composer.x, "popup starts at the composer x");
            assert_eq!(
                right + 1,
                composer.right(),
                "popup ends at the composer edge"
            );
            assert!(right < term_width, "popup is inside the terminal");
            widths.push(right - left + 1);
        }
        assert_eq!(widths, vec![48, 68, 118]);
        assert!(widths[2] > 60, "no 60-column cap");
    }

    #[test]
    fn story21_2_no_matches_popup_uses_the_same_composer_width() {
        let composer = Rect::new(2, 18, 110, 4);
        let (_, left, right, text) = paint(130, composer, &slash_state(0, 0, 0));
        assert!(text.contains("No matches"));
        assert_eq!((left, right - left + 1), (2, 110));
    }

    #[test]
    fn story21_2_wide_popup_retains_scrolled_selection() {
        let composer = Rect::new(5, 18, 120, 4);
        let (_, _, _, text) = paint(130, composer, &slash_state(15, 9, 2));
        assert!(text.contains("[10/15]"), "{text}");
        assert!(text.contains("/cmd9"), "{text}");
        assert!(!text.contains("/cmd0 "), "{text}");
    }

    #[test]
    fn story21_2_popup_area_is_clamped_inside_the_drawable_rect() {
        let drawable = Rect::new(0, 0, 100, 24);
        // Composer reported wider/farther right than the drawable area.
        let area = calculate_popup_area(Rect::new(10, 20, 200, 4), drawable, 5);
        assert!(area.right() <= drawable.right());
        assert_eq!((area.x, area.width), (10, 90));
        // Composer entirely outside the drawable area collapses, never overflows.
        let area = calculate_popup_area(Rect::new(150, 20, 40, 4), drawable, 5);
        assert!(area.right() <= drawable.right());
        assert_eq!(area.width, 0);
        // Near the top there is no room above: stay inside, never underflow.
        let area = calculate_popup_area(Rect::new(0, 2, 80, 3), drawable, 10);
        assert_eq!(area.y, 0);
        assert!(area.bottom() <= drawable.bottom());
    }
}
