use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::modal::{ModalPart, fit_modal};
use crate::adapters::tui::keymap::render_help::{help_groups, intro, tmux_conflicts};
use crate::adapters::tui::state::HelpOverlayState;
use crate::adapters::tui::theme::Theme;

/// Render the help content into the frame rectangle supplied by compose.
// Covers: FR108, UX-DR94
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &HelpOverlayState,
    theme: &Theme,
    tmux_detected: bool,
) {
    // Dim the background behind the overlay
    let bg_block = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(Clear, area);
    frame.render_widget(bg_block, area);

    // Build the content lines
    let content_lines = build_content_lines(theme, tmux_detected, state.scroll_offset, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(
            " Help — Rustain Keybindings ",
            theme.typography.heading,
        ))
        .title_bottom(Span::styled(
            " j/k scroll  ? close ",
            Style::default().fg(theme.colors.fg_muted),
        ));

    let inner = block.inner(area);
    let content_lines = fit_modal(
        vec![ModalPart::Rows {
            lines: content_lines,
            selected: (0, 0),
        }],
        usize::from(inner.height),
        usize::from(inner.width),
    );
    let para = Paragraph::new(content_lines).block(block).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );

    frame.render_widget(para, area);
}

/// Build the text content for the overlay, applying scroll_offset to skip lines at the top.
fn build_content_lines(
    theme: &Theme,
    tmux_detected: bool,
    scroll_offset: usize,
    area: Rect,
) -> Vec<Line<'static>> {
    let mut all_lines: Vec<Line> = Vec::new();

    // Prose introduction
    all_lines.push(Line::default());
    for intro_line in wrap_text(&intro(), area.width.saturating_sub(4) as usize) {
        all_lines.push(Line::from(Span::styled(
            intro_line,
            Style::default().fg(theme.colors.fg_secondary),
        )));
    }
    all_lines.push(Line::default());

    // Keybinding categories
    for group in help_groups() {
        // Category header
        all_lines.push(Line::from(Span::styled(
            group.name,
            theme.typography.heading.fg(theme.colors.accent),
        )));

        for binding in &group.rows {
            let key_style = if binding.available {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::DIM)
            };
            let desc_style = if binding.available {
                Style::default().fg(theme.colors.fg_primary)
            } else {
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::DIM)
            };

            all_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<14}", binding.key), key_style),
                Span::styled(binding.description.clone(), desc_style),
            ]));
        }
        all_lines.push(Line::default());
    }

    // tmux conflict notice
    if tmux_detected {
        all_lines.push(Line::from(Span::styled(
            "⚠ tmux detected — some shortcuts may conflict with your tmux prefix.",
            Style::default().fg(theme.colors.warning),
        )));
        all_lines.push(Line::from(Span::styled(
            "  Alternatives shown where available:",
            Style::default().fg(theme.colors.fg_secondary),
        )));
        for conflict in tmux_conflicts() {
            let alt = conflict.alternative.unwrap_or("—");
            all_lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("{:<12}", conflict.key),
                    Style::default().fg(theme.colors.warning),
                ),
                Span::styled(
                    format!("conflicts with {}  →  {}", conflict.conflict_with, alt),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
            ]));
        }
        all_lines.push(Line::default());
    }

    // Apply scroll offset
    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    let max_offset = all_lines.len().saturating_sub(inner_height);
    let actual_offset = scroll_offset.min(max_offset);
    all_lines.into_iter().skip(actual_offset).collect()
}

/// Split `text` into lines of at most `max_width` characters (simple word-wrap).
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current.clone());
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::adapters::tui::state::HelpOverlayState;
    use crate::adapters::tui::theme::Theme;

    fn make_theme() -> Theme {
        Theme::dark()
    }

    fn make_state() -> HelpOverlayState {
        HelpOverlayState::new()
    }

    #[test]
    fn test_render_no_crash_when_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state();
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, false);
            })
            .unwrap();
    }

    #[test]
    fn test_render_no_crash_with_tmux() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state();
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, true);
            })
            .unwrap();
    }

    #[test]
    fn test_render_no_crash_small_terminal() {
        // Minimum terminal size: 80x24
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state();
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, false);
            })
            .unwrap();
    }

    #[test]
    fn test_render_uses_caller_area_without_ownership_flags() {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let area = Rect::new(7, 3, 80, 20);
        terminal
            .draw(|frame| render(frame, area, &make_state(), &make_theme(), false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(area.x, area.y)].symbol(), "┌");
        assert_eq!(buffer[(area.right() - 1, area.bottom() - 1)].symbol(), "┘");
        assert_eq!(buffer[(0, 0)].symbol(), " ");
    }

    #[test]
    fn test_wrap_text_short() {
        let lines = wrap_text("hello world", 80);
        assert_eq!(lines, vec!["hello world"]);
    }

    #[test]
    fn test_wrap_text_wraps() {
        let lines = wrap_text("a b c d e", 5);
        // each word is 1 char; "a b c" = 5, "d e" = 3
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_shared_geometry_small_and_wide_help_fit() {
        use crate::adapters::tui::presentation::geometry;

        for region in [Rect::new(0, 0, 60, 5), Rect::new(0, 0, 130, 30)] {
            let area = geometry::frame_rect(region);
            let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
            terminal
                .draw(|frame| render(frame, area, &make_state(), &make_theme(), false))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert!(row(area.y).contains("Help — Rustain Keybindings"));
            assert!(row(area.bottom() - 1).contains("j/k scroll  ? close"));
            assert!(
                (area.y + 1..area.bottom() - 1)
                    .any(|y| { row(y).chars().any(|c| c.is_alphanumeric()) })
            );
        }
    }
}
