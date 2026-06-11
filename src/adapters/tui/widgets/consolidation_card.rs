//! Inline memory-consolidation review card — Story 11.2a (completes 11.2 AC4).
//!
//! Reuses the plan-card / delegation-card UI *grammar* (a `pending_*_card`
//! state field → `render_*_lines` → key intercept → resolution event), NOT a
//! new approval *model* (UX principle 5). It IS a genuinely new *widget*
//! because no existing card renders a *list* the user accepts / declines —
//! that list-review is the one new shape this story adds.
//!
//! Rendered inline in the chat pane (like plan_card / delegation_card), never a
//! modal overlay — "helpful review, never an interruption" (UX). Uses the memory
//! signal vocabulary (`[mem]` source badge, `## Category` headers) and stays
//! monochrome-safe (the selection symbol carries meaning, not just colour).

use ratatui::prelude::*;

use crate::adapters::tui::state::PendingConsolidationCard;
use crate::adapters::tui::theme::Theme;

/// Render the consolidation review card as a list of chat-pane lines.
pub fn render_consolidation_card_lines<'a>(
    card: &PendingConsolidationCard,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'a>> {
    let mut lines: Vec<Line<'a>> = Vec::new();
    let inner = (width as usize).saturating_sub(8).max(8);

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
        lines.push(Line::from(vec![
            Span::styled(
                marker.to_string(),
                Style::default().fg(if *selected {
                    theme.colors.tool_status_success
                } else {
                    theme.colors.fg_muted
                }),
            ),
            Span::styled(
                format!("## {} — ", fact.category),
                Style::default().fg(theme.colors.fg_muted),
            ),
            Span::styled(
                truncate(&fact.fact, inner),
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
                    Span::styled(
                        truncate(seg, inner),
                        Style::default().fg(theme.colors.fg_muted),
                    ),
                ]));
            }
        }
    }

    lines.push(Line::from(String::new()));
    // Key-hint footer (MVP floor: accept-all / decline-all).
    lines.push(Line::from(vec![Span::styled(
        "    [y] promote all   [n] decline".to_string(),
        Style::default().fg(theme.colors.fg_muted),
    )]));

    lines
}

/// Char-safe truncation with an ellipsis marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
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
        render_consolidation_card_lines(card, &theme, 80)
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
        // key-hint footer
        assert!(text.contains("[y]"));
        assert!(text.contains("[n]"));
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
