use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::help_data::{help_categories, tmux_conflicts};
use crate::adapters::tui::state::HelpOverlayState;
use crate::adapters::tui::theme::Theme;

/// Prose introduction shown at the top of the help overlay.
const INTRO: &str =
    "Rustain is a composable AI agent platform. Navigate with j/k, type with i, \
     search with Ctrl+P, extend with Ctrl+X chords. Press ? or Esc to close.";

/// Render the help overlay as a large centered popup.
// Covers: FR108, UX-DR94
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &HelpOverlayState,
    theme: &Theme,
    tmux_detected: bool,
) {
    if !state.active {
        return;
    }

    let overlay_area = centered_area(area, 80, 90);

    // Dim the background behind the overlay
    let bg_block = Block::default()
        .style(Style::default().bg(Color::Black));
    frame.render_widget(Clear, overlay_area);
    frame.render_widget(bg_block, overlay_area);

    // Build the content lines
    let content_lines = build_content_lines(theme, tmux_detected, state.scroll_offset, overlay_area);

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

    let para = Paragraph::new(content_lines)
        .block(block)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_secondary),
        );

    frame.render_widget(para, overlay_area);
}

/// Build the text content for the overlay, applying scroll_offset to skip lines at the top.
fn build_content_lines<'a>(
    theme: &'a Theme,
    tmux_detected: bool,
    scroll_offset: usize,
    area: Rect,
) -> Vec<Line<'a>> {
    let mut all_lines: Vec<Line> = Vec::new();

    // Prose introduction
    all_lines.push(Line::default());
    for intro_line in wrap_text(INTRO, area.width.saturating_sub(4) as usize) {
        all_lines.push(Line::from(Span::styled(
            intro_line,
            Style::default().fg(theme.colors.fg_secondary),
        )));
    }
    all_lines.push(Line::default());

    // Keybinding categories
    let categories = help_categories();
    for category in categories {
        // Category header
        all_lines.push(Line::from(Span::styled(
            category.name,
            theme.typography.heading.fg(theme.colors.accent),
        )));

        for binding in &category.bindings {
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
                Span::styled(binding.description, desc_style),
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

/// Compute a centered Rect occupying `percent_x`% of the width and `percent_y`% of the height.
fn centered_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    // Use u32 arithmetic to prevent overflow on wide terminals (>819 cols at 80%)
    let w = (area.width as u32 * percent_x as u32 / 100) as u16;
    let h = (area.height as u32 * percent_y as u32 / 100) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w.max(1), h.max(1))
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

    fn make_state(active: bool) -> HelpOverlayState {
        let mut s = HelpOverlayState::new();
        s.active = active;
        s
    }

    #[test]
    fn test_render_no_crash_when_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state(true);
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
        let state = make_state(true);
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
        let state = make_state(true);
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, false);
            })
            .unwrap();
    }

    #[test]
    fn test_render_inactive_skips() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state(false);
        let theme = make_theme();
        // Should complete without touching the frame
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, false);
            })
            .unwrap();
        // Buffer should be all spaces (nothing rendered)
        let buf = terminal.backend().buffer().clone();
        let all_space = buf.content().iter().all(|cell| cell.symbol() == " ");
        assert!(all_space, "Inactive overlay should render nothing");
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
    fn test_centered_area() {
        let area = Rect::new(0, 0, 100, 50);
        let centered = centered_area(area, 80, 90);
        // Width: 100 * 80 / 100 = 80
        assert_eq!(centered.width, 80);
        // Height: 50 * 90 / 100 = 45
        assert_eq!(centered.height, 45);
        // Horizontally centered: x = (100 - 80) / 2 = 10
        assert_eq!(centered.x, 10);
        // Vertically centered: y = (50 - 45) / 2 = 2
        assert_eq!(centered.y, 2);
    }
}
