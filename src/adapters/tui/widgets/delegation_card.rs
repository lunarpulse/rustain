//! Delegation suggestion body — Story 10.5: the rows of the `Decision: Delegate task`
//! frame (Story 21.6 6-C). Keys: the frame's rail (`[d] Delegate  [l] Run locally
//! [x] Cancel plan`).

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::PendingDelegationCard;
use crate::adapters::tui::theme::Theme;
use crate::domain::services::delegation_decider::DelegationReason;

/// The suggestion's reason in words.
fn reason_text(reason: &DelegationReason) -> String {
    match reason {
        DelegationReason::ExplicitAgentMention => "explicit mention".to_string(),
        DelegationReason::DescriptionMatch { overlap_score } => {
            format!("keyword match (score {overlap_score})")
        }
        DelegationReason::Heuristic => "heuristic".to_string(),
    }
}

/// The task and the agent the decider suggests for it. The caller sanitizes the agent
/// name before it gets here; the copy path passes the original bytes.
pub fn lines(card: &PendingDelegationCard, theme: &Theme) -> Vec<Line<'static>> {
    let primary = Style::default().fg(theme.colors.fg_primary);
    vec![
        Line::from(Span::styled(
            format!("Delegate Task {}?", card.task_number),
            primary.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "→ {}  ({})",
                card.suggestion.agent_name,
                reason_text(&card.suggestion.reason)
            ),
            primary,
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::services::delegation_decider::DelegationSuggestion;

    fn make_card(reason: DelegationReason) -> PendingDelegationCard {
        PendingDelegationCard {
            conversation_id: "c1".to_string(),
            plan_id: "plan-1".to_string(),
            task_number: 2,
            suggestion: DelegationSuggestion {
                task_number: 2,
                agent_name: "code-reviewer".to_string(),
                reason,
                auto_proceed: false,
            },
        }
    }

    fn rows(card: &PendingDelegationCard) -> Vec<String> {
        lines(card, &Theme::dark())
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn story21_6_the_rows_name_the_task_the_agent_and_the_reason() {
        assert_eq!(
            rows(&make_card(DelegationReason::DescriptionMatch {
                overlap_score: 3
            })),
            [
                "Delegate Task 2?",
                "→ code-reviewer  (keyword match (score 3))"
            ]
        );
        assert_eq!(
            rows(&make_card(DelegationReason::ExplicitAgentMention))[1],
            "→ code-reviewer  (explicit mention)"
        );
        assert_eq!(
            rows(&make_card(DelegationReason::Heuristic))[1],
            "→ code-reviewer  (heuristic)"
        );
    }
}
