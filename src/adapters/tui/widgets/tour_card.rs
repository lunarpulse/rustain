//! The paint of the tour card (Story 21.14, D3): the frame's title (`Tour` or `Tour · <k> of 5`,
//! passed in by the compositor), the card body wrapped at the frame's inner width, and the rail
//! generated from the registry rows of the card's own context. The body is clipped to the rows
//! the frame has; the rail keeps the last row, and drops `Skip step` before it would clip.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;
use crate::adapters::tui::tour::copy;

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str) {
    let theme = &state.theme;
    let Some(tour) = state.tour.as_ref() else {
        return;
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(theme.colors.accent))
            .style(
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .bg(theme.colors.bg_surface),
            ),
        rect,
    );
    let inner = rect.inner(Margin::new(1, 1));
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // The body keeps at least one row; the rail takes the last.
    let rail_rows = u16::from(inner.height > 1);
    let body = Rect::new(inner.x, inner.y, inner.width, inner.height - rail_rows);
    let width = usize::from(body.width);
    let mut rows: Vec<String> = Vec::new();
    for paragraph in copy::body(tour.step, state.multiline_mode) {
        if !rows.is_empty() {
            rows.push(String::new());
        }
        rows.extend(wrap(&paragraph, width));
    }
    for (offset, row) in rows.iter().take(usize::from(body.height)).enumerate() {
        frame.render_widget(
            Line::from(row.as_str()),
            Rect::new(body.x, body.y + offset as u16, body.width, 1),
        );
    }
    if rail_rows > 0 {
        let mut items = copy::rail_items(tour.step);
        let text = loop {
            let text = items.join("  ");
            if text_geometry::cells(&text) <= usize::from(inner.width) || items.len() <= 2 {
                break text_geometry::clip(&text, usize::from(inner.width)).to_owned();
            }
            // `Skip step` goes first; `Continue` and `Skip tour` stay.
            items.remove(1);
        };
        frame.render_widget(
            Line::styled(text, Style::default().fg(theme.colors.fg_secondary)),
            Rect::new(inner.x, inner.y + body.height, inner.width, 1),
        );
    }
}

/// Word-wrap `text` at `width` cells; a word wider than a row is cut at grapheme boundaries.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        if text_geometry::cells(word) > width {
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            rows.extend(
                text_geometry::wrap_hard(word, width)
                    .into_iter()
                    .map(str::to_owned),
            );
            continue;
        }
        let needed =
            text_geometry::cells(&row) + usize::from(!row.is_empty()) + text_geometry::cells(word);
        if needed > width {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_14_wrap_never_exceeds_the_width_and_keeps_every_word() {
        let text =
            "Take a two-minute tour: send a message, inspect a tool call, answer a decision.";
        for width in [10, 24, 58] {
            let rows = wrap(text, width);
            assert!(rows.iter().all(|row| text_geometry::cells(row) <= width));
            assert_eq!(rows.join(" "), text);
        }
        let wide = wrap("界界界界界界", 4);
        assert!(
            wide.iter().all(|row| text_geometry::cells(row) <= 4),
            "{wide:?}"
        );
        assert!(wrap("", 10).is_empty());
    }
}
