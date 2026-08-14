//! Inline `/peer add` confirm card — Story 18.4b (AC3, `UX-DR-PT-02`).
//!
//! Renders the card body [`crate::adapters::cli::peer::rows::confirm_card_text`]
//! built, so the CLI confirm and this one carry the same words. This module owns
//! only the styling.
//!
//! # It binds no key of its own
//!
//! The two keystrokes this card paints resolve through
//! `InputAction::PeerAddConfirm` / `PeerAddDecline`, gated on
//! `ConfirmationType::PeerAdd`. ⛔ It never consults
//! `apply_card::choice_for_key`, which is single-sourced from
//! `APPLY_CARD_BINDINGS` and called mode-blind — borrowing it would give a card
//! that paints no `y` an unpainted `y` that pins a key.
//!
//! # ⛔ Not the alarm
//!
//! A key mismatch presents no choice, so it is not a card at all: it renders as a
//! never-truncated `FeedbackLevel::Error` block with no keys.

use ratatui::prelude::*;

use crate::adapters::tui::state::PendingPeerAdd;
use crate::adapters::tui::theme::Theme;

/// Render the confirm card as chat-pane lines.
///
/// The decision line is emphasised; everything else is plain, because the card's
/// job is to make a fingerprint comparable rather than to look urgent.
pub fn render_peer_add_lines<'a>(
    pending: &PendingPeerAdd,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'a>> {
    let inner = (width as usize).saturating_sub(4).max(20);
    let mut lines: Vec<Line<'a>> = Vec::new();
    for raw in pending.card.lines() {
        let text: String = if raw.chars().count() > inner {
            raw.chars().take(inner).collect()
        } else {
            raw.to_owned()
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
