use std::sync::Arc;

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use unicode_width::UnicodeWidthStr as _;

use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{SemanticMessageType, TeamProposal};

const HEADING: &str =
    "Your agent proposes a message. Nothing has been sent. Awaiting your decision.";
const MORE: &str = "… more not shown — [r] Review first";
const DRAFT: &str = "Finish or clear your draft to decide.";

fn type_token(message_type: SemanticMessageType) -> &'static str {
    crate::domain::models::semantic_message_type_metadata(message_type).token
}

fn aliases(proposal: &TeamProposal) -> String {
    proposal.recipients.join(", ")
}

/// Wrap without normalizing source whitespace and without splitting graphemes.
/// The caller may bound the number of rows, so large proposal bodies stay cheap.
fn wrap_prefix(text: &str, width: usize, limit: usize) -> (Vec<String>, bool) {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0usize;
    let mut pieces = text.split_inclusive(['\n', '\t']).peekable();
    while let Some(piece) = pieces.next() {
        // Span filters controls; reintroduce only the two permitted controls.
        let span = Span::raw(piece);
        let mut graphemes = span
            .styled_graphemes(Style::default())
            .map(|grapheme| grapheme.symbol)
            .chain(piece.ends_with('\t').then_some("\t"))
            .chain(piece.ends_with('\n').then_some("\n"))
            .peekable();
        while let Some(grapheme) = graphemes.next() {
            if grapheme == "\n" {
                rows.push(std::mem::take(&mut row));
                cells = 0;
                if rows.len() == limit {
                    return (
                        rows,
                        graphemes.peek().is_some()
                            || pieces.peek().is_some()
                            || text.ends_with('\n'),
                    );
                }
                continue;
            }
            if grapheme == "\t" {
                for _ in 0..8 - cells % 8 {
                    if cells >= width {
                        rows.push(std::mem::take(&mut row));
                        cells = 0;
                        if rows.len() == limit {
                            return (rows, true);
                        }
                    }
                    row.push(' ');
                    cells += 1;
                }
                continue;
            }
            let w = grapheme.width();
            if cells > 0 && cells.saturating_add(w) > width {
                rows.push(std::mem::take(&mut row));
                cells = 0;
                if rows.len() == limit {
                    return (rows, true);
                }
            }
            row.push_str(grapheme);
            cells = cells.saturating_add(w);
        }
    }
    if rows.len() < limit && (!row.is_empty() || rows.is_empty() || text.ends_with('\n')) {
        rows.push(row);
    }
    (rows, false)
}

fn prefixed_body(
    proposal: &TeamProposal,
    width: usize,
    limit: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, bool) {
    let (rows, more) = wrap_prefix(&proposal.body, width.saturating_sub(2), limit);
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

/// Render the uncapped Review Info content; body rows remain prefixed on resize.
pub fn render_review_lines(
    proposal: &TeamProposal,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
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

/// Owned frame snapshot avoids borrowing the rest of the split TUI state.
/// The proposal body itself is never copied.
pub struct CardView {
    proposal: Option<Arc<TeamProposal>>,
    draft_empty: bool,
    reviewing: bool,
    scroll: usize,
}

/// Prepare both overlays from one confirmation-precedence check.
pub fn frame(state: &TuiState) -> (bool, CardView) {
    let higher = crate::adapters::tui::app::proposal_higher_confirmation_active(state);
    let proposal = if higher {
        None
    } else {
        state
            .team_proposal_conversation_id
            .as_ref()
            .and_then(|id| state.pending_team_proposals.get(id))
            .cloned()
    };
    let reviewing = proposal.as_ref().is_some_and(|proposal| {
        state
            .tab_render_states
            .get(&state.active_tab_id)
            .is_some_and(|tab| {
                tab.team_proposal_reviews
                    .values()
                    .any(|review| Arc::ptr_eq(review, proposal))
            })
    });
    let blocked = higher || proposal.is_some();
    (
        blocked,
        CardView {
            proposal,
            draft_empty: state.input_buffer.is_empty(),
            reviewing,
            scroll: state.team_proposal_scroll,
        },
    )
}

impl CardView {
    /// Attached clients own a separate composer and tab render cache, but use
    /// the same card preparation and painting as the standalone surface.
    pub(crate) fn attached(
        proposal: Option<Arc<TeamProposal>>,
        draft_empty: bool,
        reviewing: bool,
    ) -> Self {
        Self {
            proposal,
            draft_empty,
            reviewing,
            scroll: 0,
        }
    }

    pub fn from_state(state: &TuiState) -> Self {
        frame(state).1
    }

    pub(crate) fn with_scroll(mut self, scroll: usize) -> Self {
        self.scroll = scroll;
        self
    }

    /// Prepare once per frame; Review reserves a real, scrollable chat viewport.
    pub fn prepare(
        self,
        layout: &mut crate::adapters::tui::layout::AppLayout,
        viewport: Rect,
        theme: &Theme,
    ) -> Option<CardPaint> {
        let proposal = self.proposal?;
        let top = layout
            .tab_bar
            .map_or(viewport.y, |bar| bar.bottom())
            .max(viewport.y);
        let area = Rect::new(
            layout.chat_pane.x,
            top,
            layout.chat_pane.width,
            layout.chat_pane.bottom().saturating_sub(top),
        )
        .intersection(viewport);
        let width = area.width.min(72);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let card_area = Rect::new(x, area.y, width, area.height);
        let inner_width = width.saturating_sub(4) as usize;
        if width < 20 || area.height < 9 || inner_width == 0 {
            return None;
        }

        let footer = if self.draft_empty {
            format!(
                "[y] Send to {}   [r] Review first   [n] Dismiss",
                aliases(&proposal)
            )
        } else {
            DRAFT.to_owned()
        };
        let mut content = Vec::new();
        content.extend(
            wrap_prefix(HEADING, inner_width, usize::MAX)
                .0
                .into_iter()
                .map(Line::from),
        );
        content.extend(
            wrap_prefix(
                &format!("To: {}", aliases(&proposal)),
                inner_width,
                usize::MAX,
            )
            .0
            .into_iter()
            .map(Line::from),
        );
        content.extend(
            wrap_prefix(
                &format!("Type: {}", type_token(proposal.message_type)),
                inner_width,
                usize::MAX,
            )
            .0
            .into_iter()
            .map(Line::from),
        );
        let footer_rows = wrap_prefix(&footer, inner_width, usize::MAX).0;
        // Preview yields space first; overflowing chrome stays complete and scrollable.
        let chrome = content.len() + footer_rows.len();
        // Keep Review text readable even when streaming uses its final row.
        let max_height = area
            .height
            .saturating_sub(if self.reviewing { 3 } else { 0 });
        let available_body = usize::from(max_height).saturating_sub(3 + chrome);
        let overflow = available_body == 0;
        let (mut body, more) = if self.reviewing {
            (Vec::new(), true)
        } else {
            prefixed_body(&proposal, inner_width, available_body.clamp(1, 4), theme)
        };
        content.append(&mut body);
        if more
            && (overflow
                || content.len() + footer_rows.len() < usize::from(max_height.saturating_sub(2)))
        {
            content.extend(
                wrap_prefix(MORE, inner_width, 1)
                    .0
                    .into_iter()
                    .map(Line::from),
            );
        }
        content.extend(footer_rows.into_iter().map(Line::from));
        let height = (content.len() + 2).min(usize::from(max_height)) as u16;
        let y = area.bottom() - height;
        let draw_area = Rect::new(card_area.x, y, width, height);
        if self.reviewing {
            layout.chat_pane.y = area.y;
            layout.chat_pane.height = draw_area.y - area.y;
            layout.dashboard_panel = None;
        }
        Some(CardPaint {
            proposal,
            area: draw_area,
            lines: content,
            scroll: self.scroll,
            has_decision_footer: self.draft_empty,
        })
    }
}

pub struct CardPaint {
    proposal: Arc<TeamProposal>,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: usize,
    has_decision_footer: bool,
}

impl CardPaint {
    pub fn scroll_max(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport_rows())
    }

    pub fn viewport_rows(&self) -> usize {
        usize::from(self.area.height.saturating_sub(2))
    }

    pub fn footer_visible(&self) -> bool {
        self.has_decision_footer && self.scroll >= self.scroll_max()
    }
}

/// Paint the prepared card and return only the snapshot actually visible.
pub fn render(
    card: Option<CardPaint>,
    buffer: &mut Buffer,
    theme: &Theme,
) -> Option<Arc<TeamProposal>> {
    let CardPaint {
        proposal,
        area,
        lines,
        scroll,
        has_decision_footer: _,
    } = card?;
    let viewport_rows = usize::from(area.height.saturating_sub(2));
    let scroll_max = lines.len().saturating_sub(viewport_rows);
    Clear.render(area, buffer);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme.colors.decision_border))
        .style(Style::default().bg(theme.colors.bg_primary));
    if scroll_max > 0 {
        block = block.title(" PgUp/PgDn: scroll proposal ");
    }
    let inner = block.inner(area);
    block.render(area, buffer);
    for (i, line) in lines
        .into_iter()
        .skip(scroll.min(scroll_max))
        .take(viewport_rows)
        .enumerate()
    {
        buffer.set_line(inner.x, inner.y + i as u16, &line, inner.width);
    }
    Some(proposal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_preserves_whitespace_and_graphemes() {
        let (rows, more) = wrap_prefix("  e\u{301}界\t\n", 3, 8);
        assert!(!more);
        assert_eq!(rows.concat(), "  e\u{301}界      ");
        assert_eq!(rows.last().map(String::as_str), Some(""));
    }
}
