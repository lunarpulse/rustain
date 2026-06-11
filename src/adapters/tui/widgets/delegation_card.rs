use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::Widget,
};

use crate::adapters::tui::state::PendingDelegationCard;
use crate::adapters::tui::theme::Theme;

/// Render a delegation suggestion modal card.
/// Mirrors the plan_card.rs signature.
pub fn render(area: Rect, buf: &mut Buffer, card: &PendingDelegationCard, theme: &Theme) {
    let block = ratatui::widgets::Block::default()
        .title(format!(" Delegate Task {}? ", card.task_number))
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(area);
    block.render(area, buf);

    let agent_name = &card.suggestion.agent_name;
    let reason_text = match &card.suggestion.reason {
        crate::domain::services::delegation_decider::DelegationReason::ExplicitAgentMention => {
            "explicit mention".to_string()
        }
        crate::domain::services::delegation_decider::DelegationReason::DescriptionMatch {
            overlap_score,
        } => format!("keyword match (score {})", overlap_score),
        crate::domain::services::delegation_decider::DelegationReason::Heuristic => {
            "heuristic".to_string()
        }
    };

    let lines = [
        format!("  → {}  ({})", agent_name, reason_text),
        "  [d] Delegate    [l] Run locally    [Esc] Cancel plan".to_string(),
    ];

    for (i, line) in lines.iter().enumerate() {
        let y = inner.y + i as u16;
        if y < inner.bottom() {
            buf.set_stringn(
                inner.x,
                y,
                line,
                inner.width as usize,
                Style::default().fg(theme.colors.fg_primary),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::color_detect::ColorCapability;
    use crate::adapters::tui::state::PendingDelegationCard;
    use crate::domain::models::tab::ConversationId;
    use crate::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};

    fn make_card(reason: DelegationReason) -> PendingDelegationCard {
        PendingDelegationCard {
            conversation_id: ConversationId::new(),
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

    fn test_theme() -> Theme {
        Theme::for_capability(ColorCapability::TrueColor)
    }

    #[test]
    fn snapshot_delegation_card_keyword_match() {
        let card = make_card(DelegationReason::DescriptionMatch { overlap_score: 3 });
        let theme = test_theme();
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
        render(buf.area, &mut buf, &card, &theme);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        insta::assert_snapshot!(text);
    }

    #[test]
    fn snapshot_delegation_card_explicit_mention() {
        let card = make_card(DelegationReason::ExplicitAgentMention);
        let theme = test_theme();
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
        render(buf.area, &mut buf, &card, &theme);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        insta::assert_snapshot!(text);
    }
}
