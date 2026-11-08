//! Memory-consolidation decision frame body.

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::PendingConsolidationCard;
use crate::adapters::tui::theme::Theme;

/// Build the consolidation review body; the decision frame wraps its rows.
pub fn lines(card: &PendingConsolidationCard, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let count = card.proposals.len();
    // Header: [mem] badge + count.
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  [mem] Consolidate memory — {} proposal{}",
            count,
            if count == 1 { "" } else { "s" }
        ),
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(String::new()));

    // One selectable row per proposal: marker + `## Category — fact`.
    for (fact, selected) in &card.proposals {
        let marker = if *selected { "  [x] " } else { "  [ ] " };
        let category = format!("## {} — ", fact.category);
        lines.push(Line::from(vec![
            Span::styled(
                marker.to_string(),
                Style::default().fg(if *selected {
                    theme.colors.tool_status_success
                } else {
                    theme.colors.fg_muted
                }),
            ),
            Span::styled(category, Style::default().fg(theme.colors.fg_muted)),
            Span::styled(
                fact.fact.clone(),
                Style::default().fg(theme.colors.fg_primary),
            ),
        ]));
        if let Some(detail) = &fact.detail {
            for seg in detail.split('\n') {
                if seg.trim().is_empty() {
                    continue;
                }
                lines.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(seg.to_owned(), Style::default().fg(theme.colors.fg_muted)),
                ]));
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;
    use crate::domain::models::MemoryFact;

    fn sample_card() -> PendingConsolidationCard {
        PendingConsolidationCard {
            conversation_id: "c1".to_string(),
            proposals: vec![
                (
                    MemoryFact {
                        category: "Preferences".into(),
                        fact: "User prefers snake_case".into(),
                        detail: Some("in Rust code".into()),
                    },
                    true,
                ),
                (
                    MemoryFact {
                        category: "Build".into(),
                        fact: "Use cargo nextest".into(),
                        detail: None,
                    },
                    false,
                ),
            ],
        }
    }

    fn render_text(card: &PendingConsolidationCard) -> String {
        let theme = Theme::dark();
        lines(card, &theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_a_row_per_proposal_with_category_and_fact() {
        let text = render_text(&sample_card());
        assert!(text.contains("Preferences"));
        assert!(text.contains("User prefers snake_case"));
        assert!(text.contains("Build"));
        assert!(text.contains("Use cargo nextest"));
        // memory signal vocabulary + count header
        assert!(text.contains("[mem]"));
        assert!(text.contains("2 proposals"));
        assert!(!text.contains("[y]"));
        assert!(!text.contains("[n]"));
    }

    #[test]
    fn selection_marker_reflects_bool() {
        let text = render_text(&sample_card());
        assert!(text.contains("[x]")); // first proposal is selected
        assert!(text.contains("[ ]")); // second proposal is not
    }

    #[test]
    fn singular_header_for_one_proposal() {
        let card = PendingConsolidationCard {
            conversation_id: "c1".to_string(),
            proposals: vec![(
                MemoryFact {
                    category: "C".into(),
                    fact: "f".into(),
                    detail: None,
                },
                true,
            )],
        };
        let text = render_text(&card);
        assert!(text.contains("1 proposal"));
        assert!(!text.contains("1 proposals"));
    }
}
