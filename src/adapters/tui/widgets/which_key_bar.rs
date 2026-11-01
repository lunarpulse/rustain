use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::keymap::AvailCtx;
use crate::adapters::tui::keymap::render::{self, MenuItem};
use crate::adapters::tui::state::WhichKeyState;
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
/// nothing reserves Stage rows for it.
// Covers: UX-DR19, UX-DR60
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &WhichKeyState,
    theme: &Theme,
    avail: &AvailCtx,
) {
    if !state.active || area.width < 4 || area.height < 3 {
        return;
    }

    let items = render::leader_menu(avail);
    let inner_width = usize::from(area.width.saturating_sub(2));
    let rows = render::wrap_rows(&items, inner_width);
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(format!(
            " {} ",
            crate::adapters::tui::keymap::Key::Ctrl('x').label()
        ));

    let visible_rows = usize::from(height.saturating_sub(2));
    let lines: Vec<Line> = rows
        .iter()
        .take(visible_rows)
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            for (position, &index) in row.iter().enumerate() {
                if position > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.extend(item_spans(&items[index], theme));
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
}

fn item_spans<'a>(item: &MenuItem, theme: &Theme) -> Vec<Span<'a>> {
    let (key_style, label_style) = if item.available {
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
    let suffix = if item.available { "" } else { " (n/a)" };
    vec![
        Span::styled(format!("[{}]", item.key), key_style),
        Span::styled(format!(" {}{}", item.name, suffix), label_style),
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
        let mut wk = WhichKeyState::new();
        wk.open(crate::domain::models::FocusState::Input);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(frame, area, &wk, &Theme::dark(), avail);
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
    fn story21_3_which_key_qualifies_panels_below_the_sidebar_width() {
        let text = paint(80, 12, &local(80));
        assert!(text.contains("[H] History (n/a)"), "{text}");
        assert!(text.contains("[P] Profile"), "{text}");
        assert!(!text.contains("[P] Profile (n/a)"), "{text}");
    }

    #[test]
    fn story21_2_which_key_stays_above_the_status_row() {
        let mut wk = WhichKeyState::new();
        wk.open(crate::domain::models::FocusState::Input);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| {
                let full = frame.area();
                // Status row is the last row; the menu area ends above it.
                let above = Rect::new(full.x, full.y, full.width, full.height - 1);
                frame.render_widget(Paragraph::new("STATUS-ROW-CANARY"), Rect::new(0, 11, 80, 1));
                render(frame, above, &wk, &Theme::dark(), &local(80));
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let last: String = (0..80)
            .map(|x| buffer[(x, 11)].symbol().to_string())
            .collect();
        assert!(last.contains("STATUS-ROW-CANARY"));
    }
}
