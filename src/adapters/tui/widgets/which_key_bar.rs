use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::keymap::AvailCtx;
use crate::adapters::tui::keymap::render::{self, MenuItem};
use crate::adapters::tui::state::LeaderFeedback;
use crate::adapters::tui::text_geometry;
use crate::adapters::tui::theme::Theme;

/// Rows (borders included) the menu needs inside `width` columns.
pub fn bar_height(avail: &AvailCtx, width: u16) -> u16 {
    let items = render::leader_menu(avail);
    let inner = usize::from(width.saturating_sub(2));
    let rows = render::wrap_rows(&items, inner).len();
    u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2)
}

/// Render the complete, untimed, wrapped Ctrl+X chord menu anchored to the
/// bottom of `area` (Story 21.2 AC6), generated from the keymap registry (Story
/// 21.3 AC3): `avail` carries the host and the terminal facts that qualify a row
/// `(n/a)`. The menu paints over feedback/ambient rows; the caller passes an
/// `area` that ends above the status row so the status row is never covered, and
/// nothing reserves Stage rows for it. When full labels cannot fit vertically,
/// a denser grid shortens labels, never the registry keys or entry inventory.
/// Returns the painted bar rectangle, or `None` when `area` is too small.
// Covers: UX-DR19, UX-DR60
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    avail: &AvailCtx,
    feedback: Option<&LeaderFeedback>,
) -> Option<Rect> {
    if area.width < 4 || area.height < 3 {
        return None;
    }

    let items = render::leader_menu(avail);
    let inner_width = usize::from(area.width.saturating_sub(2));
    let mut rows = render::wrap_rows(&items, inner_width);
    let height = u16::try_from(rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    let bar_area = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    );

    frame.render_widget(Clear, bar_area);

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(format!(
            " {} ",
            crate::adapters::tui::keymap::Key::Ctrl('x').label()
        ));
    if let Some(feedback) = feedback {
        block = block.title_bottom(Line::from(Span::styled(
            feedback.message.as_ref(),
            Style::default().fg(theme.colors.accent),
        )));
    }

    let visible_rows = usize::from(height.saturating_sub(2));
    let column_widths = if rows.len() > visible_rows {
        let columns = items.len().div_ceil(visible_rows);
        let mut widths = vec![0; columns];
        for (index, item) in items.iter().enumerate() {
            let width = &mut widths[index % columns];
            *width = (*width).max(text_geometry::cells(&item.key) + 2);
        }
        let spare = inner_width.saturating_sub(widths.iter().sum::<usize>() + 2 * (columns - 1));
        for (index, width) in widths.iter_mut().enumerate() {
            *width += spare / columns + usize::from(index < spare % columns);
        }
        // Reuse the already allocated wrapped rows for the row-major grid.
        rows.truncate(items.len().div_ceil(columns));
        for row in &mut rows {
            row.clear();
        }
        for index in 0..items.len() {
            rows[index / columns].push(index);
        }
        Some(widths)
    } else {
        None
    };
    let lines: Vec<Line> = rows
        .iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            for (position, &index) in row.iter().enumerate() {
                if position > 0 {
                    spans.push(Span::raw("  "));
                }
                let pressed =
                    feedback.is_some_and(|feedback| feedback.command == Some(items[index].command));
                let width = column_widths.as_ref().map(|widths| widths[position]);
                let entry = item_spans(&items[index], theme, pressed, width);
                let padding = width.map_or(0, |width| {
                    width.saturating_sub(entry.iter().map(Span::width).sum::<usize>())
                });
                spans.extend(entry);
                if padding > 0 && position + 1 < row.len() {
                    spans.push(Span::raw(text_geometry::spaces(padding)));
                }
            }
            Line::from(spans)
        })
        .collect();

    let content = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        );

    frame.render_widget(content, bar_area);
    Some(bar_area)
}

fn item_spans<'a>(
    item: &MenuItem,
    theme: &Theme,
    pressed: bool,
    width: Option<usize>,
) -> [Span<'a>; 2] {
    let (mut key_style, mut label_style) = if item.available {
        (
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.colors.fg_primary),
        )
    } else {
        (
            Style::default().fg(theme.colors.fg_muted),
            Style::default().fg(theme.colors.fg_muted),
        )
    };
    if pressed {
        let emphasis = Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD);
        key_style = key_style.patch(emphasis);
        label_style = label_style.patch(emphasis);
    }
    let suffix = if item.available { "" } else { " (n/a)" };
    let label_width = width.map(|width| width.saturating_sub(text_geometry::cells(&item.key) + 2));
    let label = if label_width.is_some_and(|width| width <= 1) {
        String::new()
    } else {
        let label = format!(" {}{}", item.name, suffix);
        match label_width {
            Some(width) => match text_geometry::ellipsize(&label, width, "…") {
                std::borrow::Cow::Borrowed(_) => label,
                std::borrow::Cow::Owned(shortened) => shortened,
            },
            None => label,
        }
    };
    [
        Span::styled(format!("[{}]", item.key), key_style),
        Span::styled(label, label_style),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::visual::DensityMode;
    use ratatui::{Terminal, backend::TestBackend};

    fn local(width: u16) -> AvailCtx {
        AvailCtx::local(width, DensityMode::Focus)
    }

    fn paint(width: u16, height: u16, avail: &AvailCtx) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let _ = render(frame, area, &Theme::dark(), avail, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn story21_2_which_key_paints_every_chord_at_60_80_130_in_both_hosts() {
        for attached in [false, true] {
            for width in [60u16, 80, 130] {
                let avail = if attached {
                    AvailCtx::attached(width, false)
                } else {
                    local(width)
                };
                let text = paint(width, 12, &avail);
                for item in render::leader_menu(&avail) {
                    let painted = item.text();
                    assert!(
                        text.contains(&painted),
                        "attached={attached} {width}: `{painted}` missing/clipped in\n{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn story21_3_which_key_shows_r_e_and_enter_send_generated_from_the_registry() {
        let text = paint(130, 12, &AvailCtx::local(130, DensityMode::Monitor));
        assert!(text.contains("[R] Room"), "{text}");
        assert!(text.contains("[E] Artifacts"), "{text}");
        assert!(text.contains("[H] History"), "{text}");
        assert!(text.contains("[Enter] Send"), "{text}");
        assert!(
            !text.contains("[F] Focus") && !text.contains("[D] Dashboard"),
            "{text}"
        );
    }

    #[test]
    fn story21_7_which_key_offers_history_at_every_local_width() {
        // History is a frame (A21): no sidebar-width qualifier at 80 columns.
        let text = paint(80, 12, &local(80));
        assert!(text.contains("[H] History"), "{text}");
        assert!(!text.contains("[H] History (n/a)"), "{text}");
        assert!(text.contains("[P] Profile"), "{text}");
        assert!(!text.contains("[P] Profile (n/a)"), "{text}");
    }

    #[test]
    fn story21_2_which_key_stays_above_the_status_row() {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| {
                let full = frame.area();
                // Status row is the last row; the menu area ends above it.
                let above = Rect::new(full.x, full.y, full.width, full.height - 1);
                frame.render_widget(Paragraph::new("STATUS-ROW-CANARY"), Rect::new(0, 11, 80, 1));
                let _ = render(frame, above, &Theme::dark(), &local(80), None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let last: String = (0..80)
            .map(|x| buffer[(x, 11)].symbol().to_string())
            .collect();
        assert!(last.contains("STATUS-ROW-CANARY"));
    }
}
