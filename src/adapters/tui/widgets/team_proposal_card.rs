use ratatui::prelude::*;

use crate::adapters::tui::widgets::delimited_body::wrap_text as wrap_prefix;

use crate::adapters::tui::keymap::Context;
use crate::adapters::tui::keymap::render::rail;
use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{SemanticMessageType, TeamProposal};
use crate::domain::services::peer_text::{sanitize_display_line, sanitize_tool_text_block};

const HEADING: &str =
    "Your agent proposes a message. Nothing has been sent. Awaiting your decision.";

fn more_marker() -> String {
    format!(
        "… more not shown — {} Review first",
        rail(Context::TeamProposal, CardCmd::Review)
    )
}

fn type_token(message_type: SemanticMessageType) -> &'static str {
    crate::domain::models::semantic_message_type_metadata(message_type).token
}

fn aliases(proposal: &TeamProposal) -> String {
    proposal
        .recipients
        .iter()
        .map(|alias| sanitize_display_line(alias))
        .collect::<Vec<_>>()
        .join(", ")
}

fn prefixed_body(
    proposal: &TeamProposal,
    width: usize,
    limit: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, bool) {
    let safe_body = sanitize_tool_text_block(&proposal.body);
    let (rows, more) = wrap_prefix(&safe_body, width.saturating_sub(2), limit);
    let lines = rows
        .into_iter()
        .map(|row| {
            Line::from(vec![
                Span::styled("│ ", Style::default().fg(theme.colors.accent)),
                Span::styled(row, Style::default().fg(theme.colors.fg_primary)),
            ])
        })
        .collect();
    (lines, more)
}

/// The uncapped Review body; rows remain prefixed on resize.
fn render_review_lines(proposal: &TeamProposal, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let text_style = Style::default().fg(theme.colors.fg_primary);
    let mut lines = wrap_prefix(
        &format!(
            "Proposed message (not sent) · {}",
            type_token(proposal.message_type)
        ),
        width as usize,
        usize::MAX,
    )
    .0
    .into_iter()
    .map(|line| Line::from(Span::styled(line, text_style)))
    .collect::<Vec<_>>();
    lines.extend(
        wrap_prefix(
            &format!("To: {}", aliases(proposal)),
            width as usize,
            usize::MAX,
        )
        .0
        .into_iter()
        .map(|line| Line::from(Span::styled(line, text_style))),
    );
    let (body, _) = prefixed_body(proposal, width as usize, usize::MAX, theme);
    lines.extend(body);
    lines
}

/// Borderless proposal content for the shared decision frame.
pub fn body_lines(
    proposal: &TeamProposal,
    reviewing: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if reviewing {
        return render_review_lines(proposal, width, theme);
    }
    let width = usize::from(width);
    let mut lines = Vec::new();
    for text in [
        HEADING.to_owned(),
        format!("To: {}", aliases(proposal)),
        format!("Type: {}", type_token(proposal.message_type)),
    ] {
        lines.extend(
            wrap_prefix(&text, width, usize::MAX)
                .0
                .into_iter()
                .map(Line::from),
        );
    }
    let (body, more) = prefixed_body(proposal, width, 4, theme);
    lines.extend(body);
    if more {
        lines.extend(
            wrap_prefix(&more_marker(), width, usize::MAX)
                .0
                .into_iter()
                .map(Line::from),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_preview_limits_rows_and_review_keeps_the_full_body() {
        let proposal = TeamProposal {
            id: "p".into(),
            conversation_id: "c".into(),
            recipients: vec!["peer\nforged".into()],
            message_type: SemanticMessageType::ArchitectureUpdate,
            body: "one\ntwo\nthree\nfour\nfive".into(),
        };
        for width in [60, 80, 130] {
            let text = |reviewing| {
                body_lines(&proposal, reviewing, width, &Theme::dark())
                    .into_iter()
                    .map(|line| {
                        line.spans
                            .into_iter()
                            .map(|span| span.content.into_owned())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
            };
            let preview = text(false);
            assert_eq!(
                preview
                    .iter()
                    .take_while(|line| !line.starts_with("To: "))
                    .cloned()
                    .collect::<String>(),
                HEADING
            );
            assert!(preview.iter().any(|line| line.contains("To: peer forged")));
            assert!(
                preview
                    .iter()
                    .any(|line| line == &format!("Type: {}", type_token(proposal.message_type)))
            );
            assert_eq!(
                preview.iter().filter(|line| line.starts_with("│ ")).count(),
                4
            );
            for row in ["│ one", "│ two", "│ three", "│ four"] {
                assert!(preview.iter().any(|line| line == row));
            }
            assert!(
                preview
                    .iter()
                    .any(|line| line.contains("… more not shown — [r] Review first"))
            );
            let review = text(true);
            assert!(
                review
                    .iter()
                    .any(|line| line.contains("Proposed message (not sent) ·"))
            );
            assert!(review.iter().any(|line| line == "To: peer forged"));
            assert!(review.iter().any(|line| line == "│ five"));
        }
        assert_eq!(proposal.recipients[0], "peer\nforged");
    }
}
