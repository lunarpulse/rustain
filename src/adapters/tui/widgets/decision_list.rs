//! The Needs-you list (S-QUEUE, Story 21.6 A10): one L3 `Surface::DecisionQueue`
//! frame listing every waiting decision of this host.
//!
//! Row: `<kind title> · <source summary> · <conversation title or —> · <age>` plus
//! `deferred` / `stale` markers, and, for an item of another tab, the passive
//! `Open <conversation> to decide` (it cannot be opened from here). Source text is
//! sanitized on paint; nothing here ever decides.

use std::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::decision_queue::{self, DecisionItem};
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, Context};
use crate::adapters::tui::notification_log::{LogFacts, footer_lines};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::{ModalPart, fit_modal};
use crate::domain::services::peer_text::sanitize_peer_text_line;

/// Longest source summary a row shows before it is cut.
const SUMMARY_CELLS: usize = 36;

/// `12s`, `3m`, `2h`: how long the item has waited.
fn age(waited: Duration) -> String {
    match waited.as_secs() {
        secs @ 0..=59 => format!("{secs}s"),
        secs @ 60..=3599 => format!("{}m", secs / 60),
        secs => format!("{}h", secs / 3600),
    }
}

fn summary_of(item: &DecisionItem) -> String {
    let source = if !item.summary.is_empty() {
        item.summary.as_str()
    } else if !item.detail.is_empty() {
        item.detail.lines().next().unwrap_or_default()
    } else {
        item.source.as_str()
    };
    let clean = sanitize_peer_text_line(source);
    geom::ellipsize(&clean, SUMMARY_CELLS, "…").into_owned()
}

/// Build a row within the available cells, keeping age and whole status markers.
fn build_row(
    kind: &str,
    summary: &str,
    conversation: &str,
    age: &str,
    flags: (bool, bool, bool),
    width: usize,
) -> String {
    let (stale, deferred, passive) = flags;
    let base = geom::cells(kind) + geom::cells(age) + 9;
    if base > width {
        return geom::clip(kind, width).to_owned();
    }
    let mut remaining = width - base;
    let stale = stale && remaining >= geom::cells(" · stale");
    remaining -= usize::from(stale) * geom::cells(" · stale");
    let deferred = deferred && remaining >= geom::cells(" · deferred");
    remaining -= usize::from(deferred) * geom::cells(" · deferred");
    let passive = passive && remaining >= geom::cells(" · Open  to decide");
    remaining -= usize::from(passive) * geom::cells(" · Open  to decide");

    // Preserve the title before the lower-priority summary; a passive row uses
    // the same abbreviated title in both locations.
    let copies = if passive { 2 } else { 1 };
    let title_budget = geom::cells(conversation).min(remaining / copies);
    let title = geom::ellipsize(conversation, title_budget, "…");
    remaining -= geom::cells(&title) * copies;
    let summary = geom::ellipsize(summary, remaining, "…");
    let mut row = format!("{kind} · {summary} · {title} · {age}");
    if stale {
        row.push_str(" · stale");
    }
    if deferred {
        row.push_str(" · deferred");
    }
    if passive {
        row.push_str(&format!(" · Open {title} to decide"));
    }
    row
}

/// The row text of `item`, without the selection marker.
fn row_text(item: &DecisionItem, state: &TuiState, facts: &LogFacts, width: usize) -> String {
    let conversation = item
        .conversation_id
        .as_deref()
        .map_or_else(|| "—".to_owned(), |id| facts.title_of(id));
    let waited = state
        .notifications
        .now()
        .saturating_duration_since(item.arrived);
    build_row(
        item.kind.title(),
        &summary_of(item),
        &sanitize_peer_text_line(&conversation),
        &age(waited),
        (
            item.is_stale(),
            item.deferred,
            !item.is_stale() && !item.actionable(facts),
        ),
        width,
    )
}

/// Paint the list into `area`. False when the area cannot hold a frame.
pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    state: &TuiState,
    facts: &LogFacts,
    theme: &Theme,
    title: &str,
) -> bool {
    if area.width < 12 || area.height < 5 {
        return false;
    }
    let queue = &state.decisions;
    let inner_width = usize::from(area.width.saturating_sub(2));
    let normal = Style::default().fg(theme.colors.fg_primary);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default().fg(theme.colors.accent);
    let selected = decision_queue::list_selected_index(state);

    let rows: Vec<Line<'static>> = queue
        .items()
        .enumerate()
        .map(|(index, item)| {
            let chosen = index == selected;
            let marker = if chosen { "▸ " } else { "  " };
            let text = format!(
                "{marker}{}",
                row_text(item, state, facts, inner_width.saturating_sub(2))
            );
            let style = if chosen {
                accent.add_modifier(Modifier::BOLD)
            } else if item.deferred || item.is_stale() {
                muted
            } else {
                normal
            };
            Line::from(Span::styled(
                geom::clip(&text, inner_width).to_owned(),
                style,
            ))
        })
        .collect();

    let mut rail: Vec<Span<'static>> = Vec::new();
    for (key, label) in [
        (
            keymap::render::rail(Context::DecisionQueue, OverlayCmd::QueueOpen),
            "Open",
        ),
        (
            keymap::render::rail(Context::DecisionQueue, OverlayCmd::QueueBack),
            "Back",
        ),
        (
            format!(
                "[{}]",
                keymap::render::rail_pair(
                    Context::DecisionQueue,
                    OverlayCmd::QueuePrev,
                    OverlayCmd::QueueNext,
                )
            ),
            "Select",
        ),
    ] {
        rail.push(Span::styled(key, accent));
        rail.push(Span::styled(format!(" {label}  "), normal));
    }

    let count = queue.len();
    let layout = vec![
        ModalPart::Notice(vec![Line::from(Span::styled(
            format!("Needs you › {count} waiting"),
            muted,
        ))]),
        ModalPart::Rows {
            lines: rows,
            selected: (selected, selected),
        },
        ModalPart::Footer(footer_lines(rail, inner_width)),
    ];

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(accent)
        .title(format!(" {title} "))
        .style(Style::default().bg(theme.colors.bg_surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = fit_modal(layout, usize::from(inner.height), usize::from(inner.width));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_keeps_exact_format_when_it_fits() {
        assert_eq!(
            build_row(
                "Permission",
                "Bash",
                "Chat",
                "12s",
                (false, true, false),
                80
            ),
            "Permission · Bash · Chat · 12s · deferred"
        );
    }

    #[test]
    fn row_reserves_age_and_whole_markers_at_narrow_widths() {
        for width in [56, 76] {
            for flags in [
                (true, false, false),
                (false, true, false),
                (false, false, true),
            ] {
                let row = build_row(
                    "Context window",
                    "a very long summary that should be abbreviated",
                    "a very long conversation title that should be abbreviated",
                    "12s",
                    flags,
                    width,
                );
                assert!(geom::cells(&row) <= width, "{row}");
                assert!(row.contains(" · 12s"), "{row}");
                if flags.0 {
                    assert!(row.ends_with(" · stale"), "{row}");
                }
                if flags.1 {
                    assert!(row.ends_with(" · deferred"), "{row}");
                }
                if flags.2 {
                    assert!(
                        row.contains(" · Open ") && row.ends_with(" to decide"),
                        "{row}"
                    );
                }
                assert!(row.contains('…'), "{row}");
            }
        }
    }
}
