//! The decision frame body (Story 21.6 A4): one L3 `Surface::Decision` painted in
//! 21-4's `frame_rect`, never bottom-anchored over the Stage.
//!
//! Rows, top to bottom: the breadcrumb `Needs you › <kind> | <N> pending` (and,
//! when the frame interrupted another, `Interrupted: <breadcrumb>`), the kind's
//! delimited body, and the rail generated from the registry. The body shrinks
//! first; the rail is never dropped. Untrusted text is sanitized on paint (the
//! effect and any copy keep the original bytes).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::decision_queue::{self, DecisionItem, Validity};
use crate::adapters::tui::keymap::{self, Context, table_overlay::OverlayCmd};
use crate::adapters::tui::notification_log::{LogFacts, footer_lines};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::modal::wrapped;
use crate::adapters::tui::widgets::{ModalPart, fit_modal};
use crate::domain::services::peer_text::sanitize_tool_text_block;

/// Cells inside the frame's border at `terminal_width` (mirrors
/// `presentation::geometry::frame_rect`: a full sheet below 100 columns, 90 % above).
#[must_use]
pub(crate) fn inner_width(terminal_width: u16) -> usize {
    let width = if terminal_width < 100 {
        terminal_width
    } else {
        (u32::from(terminal_width) * 9 / 10) as u16
    };
    usize::from(width.saturating_sub(2))
}

/// The body rows of `item` at `width` cells: the source text sanitized, or the
/// stale copy once the source settled elsewhere.
fn body_text(item: &DecisionItem) -> String {
    match item.validity {
        Validity::Stale(copy) => copy.0.to_owned(),
        Validity::Live => sanitize_tool_text_block(&item.detail).into_owned(),
    }
}

/// How many rows the body wraps to at `terminal_width` (the scroll bound).
#[must_use]
pub(crate) fn body_rows(item: &DecisionItem, terminal_width: u16) -> usize {
    wrapped(
        &body_text(item),
        Style::default(),
        inner_width(terminal_width),
    )
    .len()
}

/// Paint the focused item into `area`. False when nothing was painted (no
/// decidable focused item, or the area is too small to hold a frame): the host
/// only starts the 500 ms inert clock from a frame the user could really see.
pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    state: &TuiState,
    facts: &LogFacts,
    theme: &Theme,
    title: &str,
    interrupted: Option<&str>,
) -> bool {
    let queue = &state.decisions;
    let Some(item) = decision_queue::current(state, facts) else {
        return false;
    };
    if area.width < 12 || area.height < 5 {
        return false;
    }
    let inner_width = usize::from(area.width.saturating_sub(2));
    let normal = Style::default().fg(theme.colors.fg_primary);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default().fg(theme.colors.accent);

    let mut crumb = format!("Needs you › {}", item.kind.title());
    if !item.source.is_empty() {
        crumb.push(' ');
        crumb.push_str(&sanitize_tool_text_block(&item.source));
    }
    crumb.push_str(&format!(" | {} pending", queue.len()));
    let mut header = wrapped(&crumb, muted, inner_width);
    if let Some(interrupted) = interrupted {
        header.extend(wrapped(
            &format!("Interrupted: {}", sanitize_tool_text_block(interrupted)),
            muted,
            inner_width,
        ));
    }

    // Keys are inert for 500 ms after the first paint: they paint muted until then.
    let key_style = if queue.inert(state) { muted } else { accent };
    let mut rail: Vec<Span<'static>> = Vec::new();
    if matches!(item.validity, Validity::Stale(_)) {
        rail.push(Span::styled(
            keymap::render::rail(Context::Decision, OverlayCmd::PaneEsc),
            key_style,
        ));
        rail.push(Span::styled(" Close  ", normal));
    } else {
        for (key, label) in item.kind.rail() {
            rail.push(Span::styled(key, key_style));
            rail.push(Span::styled(format!(" {label}  "), normal));
        }
    }
    let rail_lines = footer_lines(rail, inner_width);

    let text = queue
        .feedback()
        .map_or_else(|| body_text(item), str::to_owned);
    // The offset was kept at another width: clamp it so a re-wrap never skips every row.
    let wrapped_rows = wrapped(&text, normal, inner_width);
    let first = queue.scroll().min(wrapped_rows.len().saturating_sub(1));
    let body: Vec<Line<'static>> = wrapped_rows.into_iter().skip(first).collect();

    let passive = queue
        .items()
        .filter(|other| !std::ptr::eq(*other, item) && !other.actionable(facts))
        .count();
    // The breadcrumb yields before the body or the rail does: a 60x16 terminal with a
    // full composer leaves a five-row frame (title, one body row, the rail).
    let mut parts = vec![ModalPart::Notice(header)];
    if passive > 0 {
        parts.push(ModalPart::Notice(vec![Line::from(Span::styled(
            format!("{passive} in another tab: open it to decide"),
            muted,
        ))]));
    }
    parts.push(ModalPart::Rows {
        lines: body,
        selected: (0, 0),
    });
    parts.push(ModalPart::Footer(rail_lines));

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(accent)
        .title(format!(" {title} "))
        .style(Style::default().bg(theme.colors.bg_surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = fit_modal(parts, usize::from(inner.height), usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
    true
}
