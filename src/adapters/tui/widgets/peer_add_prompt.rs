//! Peer-add confirmation body, sharing its wording with the CLI.

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::PendingPeerAdd;
use crate::adapters::tui::theme::Theme;

/// Build the peer-add body, leaving the action rail to the frame.
pub fn lines(pending: &PendingPeerAdd, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for raw in pending.card.lines() {
        let yes = crate::adapters::tui::keymap::render::rail(
            crate::adapters::tui::keymap::Context::DecisionConfirm,
            crate::adapters::tui::keymap::table_card::CardCmd::Yes,
        );
        let text = if raw.starts_with("Awaiting your decision") {
            raw.split_once(&format!("  {yes}"))
                .map_or(raw, |(body, _)| body)
        } else {
            raw
        };
        let style = if raw.starts_with("Awaiting your decision") {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else if raw.starts_with("Add peer") {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_secondary)
        };
        lines.push(Line::from(vec![Span::styled(format!("  {text}"), style)]));
    }
    lines
}
