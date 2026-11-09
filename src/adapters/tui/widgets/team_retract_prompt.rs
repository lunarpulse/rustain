//! Team-retract decision frame body.

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::TeamRetractPayload;
use crate::adapters::tui::theme::Theme;

/// Build the retract body; the frame wraps its rows and supplies the rail.
pub fn lines(payload: &TeamRetractPayload, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, raw) in payload.card.lines().enumerate() {
        let style = if raw.starts_with("Awaiting your decision")
            || raw.starts_with("Cannot verify on")
            || raw.starts_with("Already removed on")
        {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else if index == 0 {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_secondary)
        };
        let safe = crate::domain::services::peer_text::sanitize_tool_text_block(raw);
        lines.push(Line::from(vec![Span::styled(format!("  {safe}"), style)]));
        if index == 0
            && let Some(principal) = &payload.principal
        {
            let safe = crate::domain::services::transparency::sanitize_disclosable(
                &crate::domain::services::peer_text::sanitize_display_line(principal),
                crate::domain::services::transparency::MAX_PEER_ID_BYTES,
            );
            lines.push(Line::from(format!("  Principal: {safe}")));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(card: &str) -> TeamRetractPayload {
        TeamRetractPayload {
            target: crate::adapters::tui::decision_queue::RetractTarget::Item {
                peer: "jun-dev".to_owned(),
                item: "ri_x".to_owned(),
            },
            conversation_id: "conv".to_owned(),
            principal: None,
            task: None,
            card: card.to_owned(),
            armed: true,
            request: None,
        }
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    #[test]
    fn body_preserves_long_unicode_rows_without_a_key_rail() {
        const G: &str = "👨\u{200d}👩\u{200d}👧";
        for word in [
            "界".repeat(40),
            G.repeat(30),
            format!("{}{}", "x".repeat(35), G.repeat(10)),
        ] {
            let source = format!(
                "Retract on jun-dev's host\n\nitem         {word}\n\nAwaiting your decision."
            );
            let rendered = texts(&lines(&pending(&source), &Theme::dark()));
            assert_eq!(
                rendered
                    .iter()
                    .map(|line| line.strip_prefix("  ").unwrap())
                    .collect::<Vec<_>>()
                    .join("\n"),
                source
            );
            assert!(!rendered.last().unwrap().contains("[n]"));
        }
    }

    #[test]
    fn story21_6_attached_retract_alias_is_sanitized_at_span_boundary() {
        let alias = "nina\x1b[31m red\u{202e}\nforged";
        let safe_alias = crate::domain::services::peer_text::sanitize_display_line(alias);
        let body = format!("Retract on {safe_alias}'s host\n\nAwaiting your decision.");
        let rendered = lines(&pending(&body), &Theme::dark());
        assert_eq!(texts(&rendered)[0], "  Retract on nina red forged's host");
        for span in rendered.iter().flat_map(|line| &line.spans) {
            assert!(!span.content.contains('\x1b'));
            assert!(!span.content.contains('\u{202e}'));
            assert!(!span.content.contains('\n'));
        }
    }

    #[test]
    fn story21_6_retract_principal_is_bounded_and_single_line_before_paint() {
        let mut payload = pending("Retract on jun-dev's host\n\nAwaiting your decision.");
        let principal = format!("pin\x1b[31m\nforged\u{202e}{}", "x".repeat(500));
        payload.principal = Some(principal.clone());
        let rendered = texts(&lines(&payload, &Theme::dark()));
        let expected = crate::domain::services::transparency::sanitize_disclosable(
            &crate::domain::services::peer_text::sanitize_display_line(&principal),
            crate::domain::services::transparency::MAX_PEER_ID_BYTES,
        );
        assert_eq!(rendered[1], format!("  Principal: {expected}"));
        assert!(!rendered[1].contains(&['\n', '\x1b', '\u{202e}'][..]));
        assert_eq!(payload.principal.as_deref(), Some(principal.as_str()));
        assert_eq!(
            payload.card,
            "Retract on jun-dev's host\n\nAwaiting your decision."
        );
    }
}
