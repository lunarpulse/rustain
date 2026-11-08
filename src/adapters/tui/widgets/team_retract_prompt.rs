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
        lines.push(Line::from(vec![Span::styled(format!("  {raw}"), style)]));
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
}
