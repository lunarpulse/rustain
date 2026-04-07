use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::adapters::tui::state::{ChordAction, WhichKeyState};
use crate::adapters::tui::theme::Theme;

/// Chord labels in display order.
const CHORD_LABELS: &[(char, &str)] = &[
    ('P', "rofile"),
    ('M', "odel"),
    ('A', "dapter"),
    ('S', "ubagent"),
    ('L', "og"),
    ('T', "ask"),
    ('U', "sage"),
    ('W', "atch"),
    ('D', "ashboard"),
    ('?', "all"),
];

/// Render the which-key hint bar at the bottom of the screen.
// Covers: UX-DR19, UX-DR60
pub fn render(frame: &mut Frame, area: Rect, state: &WhichKeyState, theme: &Theme) {
    if !state.active {
        return;
    }

    // Calculate bar height (1-2 rows depending on width)
    let total_label_width: usize = CHORD_LABELS
        .iter()
        .map(|(_, label)| 3 + UnicodeWidthStr::width(*label) + 2) // [X]label + spacing
        .sum();

    let bar_height = if total_label_width > area.width as usize {
        4
    } else {
        3
    }; // +2 for borders
    let bar_area = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(bar_height as u16),
        area.width,
        bar_height as u16,
    );

    frame.render_widget(Clear, bar_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(" Ctrl+X ");

    // Build spans for all chord labels
    let mut spans: Vec<Span> = Vec::new();
    for (i, (key, label)) in CHORD_LABELS.iter().enumerate() {
        let is_implemented = state
            .chord_map
            .get(&key.to_ascii_lowercase())
            .is_some_and(|a| !matches!(a, ChordAction::Noop(_)));

        // Bracket letter style: accent + bold for implemented, fg_muted for stubs
        let key_style = if is_implemented {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_muted)
        };

        let label_style = if is_implemented {
            Style::default().fg(theme.colors.fg_primary)
        } else {
            Style::default().fg(theme.colors.fg_muted)
        };

        spans.push(Span::styled("[", key_style));
        spans.push(Span::styled(format!("{}", key), key_style));
        spans.push(Span::styled("]", key_style));
        spans.push(Span::styled(label.to_string(), label_style));

        if i < CHORD_LABELS.len() - 1 {
            spans.push(Span::raw("  "));
        }
    }

    let content = Paragraph::new(Line::from(spans))
        .block(block)
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        );

    frame.render_widget(content, bar_area);
}
