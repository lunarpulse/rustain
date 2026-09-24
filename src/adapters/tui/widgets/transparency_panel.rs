//! Transparency Log sidebar panel (`Ctrl+X, L`). Story 18.2, AC5 / FR95.
//!
//! Renders the rows produced by
//! [`crate::domain::services::transparency::fold_transparency`] — the same fold
//! `/team log`, `rustain team log`, and `transparency.jsonl` render. There is
//! one projection; this is one of its faces.
//!
//! # Honesty rules this widget enforces
//!
//! - **Never claims to be live.** The header says "as of <time>", because
//!   `NodeJournal::load()` under a shared `flock` is a consistent read, not a
//!   subscription: the daemon can append between two reads.
//! - **Never moves the viewport under a reader** (UX-DR-ANCHOR-FSM). New rows
//!   are counted into a "N newer entries" affordance at the boundary instead.
//!   A panel that silently withholds rows is worse than one that jumps, so the
//!   count is not optional.
//! - **Never claims tamper-evidence.** The footer states the real guarantee —
//!   append-only, crash-safe, replay-verifiable — and a divergence banner
//!   appears when replay verification fails.
//! - **Monochrome-safe.** Every colour is paired with a glyph; direction and
//!   kind each carry a symbol *and* a word.
//! - **Attribution, not endorsement.** A peer id is only as trustworthy as the
//!   credential scheme behind it; the footer says so rather than implying the
//!   log proves who acted.

use ratatui::prelude::*;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::adapters::tui::state::TransparencyPanelState;
use crate::adapters::tui::theme::Theme;
use crate::domain::services::transparency::{
    ATTRIBUTION_CAVEAT, STRUCTURAL_REPLAY_CLAIM, TransparencyRow, format_unix_millis,
};

use super::sidebar::truncate_to_width;

/// First visible index and exclusive end, clamped to the actual viewport.
pub(crate) fn visible_slice(total: usize, offset: usize, height: usize) -> (usize, usize) {
    let viewport = height.max(1);
    let start = offset.min(total.saturating_sub(viewport));
    let end = (start + viewport).min(total);
    (start, end)
}

/// One row, ordered so the **decision survives truncation**.
///
/// The sidebar is ~35 columns at the minimum width, and the peer id is a
/// 64-char hash. Putting the hash before the verdict meant "refused" was the
/// first thing the cut removed — the panel rendered, and told you nothing.
/// Order: direction + kind (glyph AND word, monochrome rule) → retract mark →
/// time → peer → summary.
///
/// Story 19.16d `AC5(d)` (owner: EXPAND) — the `retracted_at_ms` mark goes
/// **beside the kind**, where truncation reaches it last. ⛔ Never appended
/// after the peer hash: this ordering exists so truncation eats the hash and
/// spares the verdict, and a mark cut first is a mark that is not there. The
/// form and width are `one_line()`'s (`[retracted {full timestamp}]`) — one
/// source, three faces, ⛔ no face restyles it.
fn row_line(row: &TransparencyRow, width: usize, theme: &Theme, selected: bool) -> Line<'static> {
    let time = match row.recorded_at_ms {
        Some(ms) => format_unix_millis(ms),
        // Explicit unknown. Never epoch zero — a fabricated timestamp in an
        // audit log is worse than an admitted gap.
        None => "—".to_owned(),
    };
    let mark = row.retracted_at_ms.map_or_else(String::new, |ms| {
        format!(" [retracted {}]", format_unix_millis(ms))
    });
    let text = format!(
        "{}{} {}{} {} · {} · {} · {}",
        row.direction.glyph(),
        row.kind.glyph(),
        row.kind.label(),
        mark,
        row.direction.label(),
        time,
        truncate_to_width(&row.peer, 12),
        row.summary
    );
    let style = if selected {
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.colors.fg_primary)
    };
    Line::from(Span::styled(truncate_to_width(&text, width), style))
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &mut TransparencyPanelState,
    selected: usize,
    focus: &crate::domain::models::FocusState,
    theme: &Theme,
) {
    Clear.render(area, buf);

    let is_focused = matches!(
        focus,
        crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::TransparencyLog,
            ..
        }
    );
    let border_style = if is_focused {
        Style::default().fg(theme.colors.accent)
    } else {
        Style::default().fg(theme.colors.fg_secondary)
    };
    // "as of", never "live".
    let title = match state.read_at_ms {
        Some(ms) => format!(" Transparency Log · as of {} ", format_unix_millis(ms)),
        None => " Transparency Log ".to_owned(),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
        .title_bottom(Span::styled(
            " j/k move · Enter drill · / search · e export · G newest · Ctrl+X L close ",
            Style::default().fg(theme.colors.fg_muted),
        ));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let muted = Style::default()
        .fg(theme.colors.fg_muted)
        .add_modifier(Modifier::ITALIC);

    let mut rendered_selected = selected;
    if let Some(error) = state.error.clone() {
        state.set_viewport_rows(inner.height as usize, &mut rendered_selected);
        let lines = vec![
            Line::from(Span::styled(
                format!("⚠ could not read the room journal: {error}"),
                Style::default().fg(theme.colors.error),
            )),
            Line::from(Span::styled(
                "This is a read failure, not an empty log.",
                muted,
            )),
        ];
        Paragraph::new(lines).render(inner, buf);
        return;
    }

    if state.visible_len() == 0 {
        state.set_viewport_rows(inner.height as usize, &mut rendered_selected);
        let reason = if state.search.is_some() {
            "· no rows match this search"
        } else {
            "· no A2A interactions recorded yet"
        };
        let lines = vec![
            Line::from(Span::styled(reason, muted)),
            Line::from(Span::styled(
                "Every inbound and outbound A2A exchange lands here.",
                muted,
            )),
        ];
        Paragraph::new(lines).render(inner, buf);
        return;
    }

    let mut chrome: Vec<Line<'static>> = Vec::new();
    if state.search_active {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(
                &format!("/{}_", state.search.as_deref().unwrap_or_default()),
                inner.width as usize,
            ),
            Style::default().fg(theme.colors.accent),
        )));
    } else if let Some(search) = state.search.as_deref().filter(|search| !search.is_empty()) {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(&format!("/ {search}"), inner.width as usize),
            muted,
        )));
    }
    if let Some(divergence) = state.report.as_ref().and_then(
        crate::domain::services::transparency::TransparencyReport::structural_divergence_report,
    ) {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(&format!("⚠ {divergence}"), inner.width as usize),
            Style::default().fg(theme.colors.warning),
        )));
    }
    let newer_chrome_index = if state.newer_entries > 0 {
        let index = chrome.len();
        chrome.push(Line::from(Span::styled(
            format!(
                "↓ {} newer {} — press G to jump",
                state.newer_entries,
                if state.newer_entries == 1 {
                    "entry"
                } else {
                    "entries"
                }
            ),
            Style::default().fg(theme.colors.accent),
        )));
        Some(index)
    } else {
        None
    };

    let list_height = (inner.height as usize).saturating_sub(chrome.len() + 1);
    state.set_viewport_rows(list_height.max(1), &mut rendered_selected);
    let rows = state.visible_rows();
    let (start, end) = visible_slice(rows.len(), state.scroll_offset, list_height.max(1));
    let items: Vec<ListItem<'static>> = rows[start..end]
        .iter()
        .enumerate()
        .flat_map(|(offset, row)| {
            let index = start + offset;
            let mut lines = vec![ListItem::new(row_line(
                row,
                inner.width as usize,
                theme,
                index == rendered_selected,
            ))];
            if state.drill_seq == Some(row.seq) {
                for detail in [
                    format!("    seq {}", row.seq),
                    format!("    peer {}", row.peer),
                    format!("    task {}", row.task.as_deref().unwrap_or("—")),
                    format!("    {}", row.summary),
                ] {
                    lines.push(ListItem::new(Line::from(Span::styled(
                        truncate_to_width(&detail, inner.width as usize),
                        muted,
                    ))));
                }
                if let Some(provenance) = &row.provenance {
                    for detail in [
                        format!("    {}", provenance.response_clause()),
                        format!("    {}", provenance.notification_clause()),
                    ] {
                        lines.push(ListItem::new(Line::from(Span::styled(
                            truncate_to_width(&detail, inner.width as usize),
                            muted,
                        ))));
                    }
                }
            }
            lines
        })
        .collect();

    let mut body: Vec<Line<'static>> = chrome;
    let footer = Line::from(Span::styled(
        truncate_to_width(
            &format!("{STRUCTURAL_REPLAY_CLAIM} · {ATTRIBUTION_CAVEAT}"),
            inner.width as usize,
        ),
        muted,
    ));

    let chrome_height = body.len() as u16;
    let list_area = Rect {
        y: inner.y + chrome_height,
        height: inner.height.saturating_sub(chrome_height + 1),
        ..inner
    };
    if list_area.height > 0 {
        ratatui::prelude::Widget::render(List::new(items), list_area, buf);
    }
    drop(rows);
    if list_area.height > 0 {
        state.acknowledge_rendered_boundary();
    }
    if let Some(index) = newer_chrome_index {
        body[index] = if state.newer_entries == 0 {
            Line::from(Span::styled("✓ caught up", muted))
        } else {
            Line::from(Span::styled(
                format!(
                    "↓ {} newer {} — press G to jump",
                    state.newer_entries,
                    if state.newer_entries == 1 {
                        "entry"
                    } else {
                        "entries"
                    }
                ),
                Style::default().fg(theme.colors.accent),
            ))
        };
    }
    if chrome_height > 0 {
        let chrome_area = Rect {
            height: chrome_height.min(inner.height),
            ..inner
        };
        Paragraph::new(body).render(chrome_area, buf);
    }
    let footer_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    Paragraph::new(vec![footer]).render(footer_area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::Direction;
    use crate::domain::services::transparency::TransparencyKind;

    fn row(seq: u64, recorded_at_ms: Option<i64>) -> TransparencyRow {
        TransparencyRow {
            seq,
            recorded_at_ms,
            retracted_at_ms: None,
            direction: Direction::Inbound,
            kind: TransparencyKind::Rejected,
            peer: "peer-a".to_owned(),
            task: Some("t-1".to_owned()),
            summary: "refused by policy".to_owned(),
            provenance: None,
            principal_collapsed: false,
        }
    }

    fn theme() -> Theme {
        Theme::dark()
    }

    #[test]
    fn a_row_without_a_timestamp_renders_a_dash_not_epoch_zero() {
        let line = row_line(&row(1, None), 120, &theme(), false);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains('—'), "{text}");
        assert!(!text.contains("1970"), "{text}");
    }

    #[test]
    fn a_row_pairs_every_signal_with_a_glyph_and_a_word() {
        // Monochrome rule: colour alone must never carry meaning.
        let line = row_line(&row(1, Some(1_700_000_000_000)), 120, &theme(), false);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains('←') && text.contains("inbound"), "{text}");
        assert!(text.contains('✗'), "{text}");
    }

    /// Story 19.16d AC5(d) (owner: EXPAND) — the `Ctrl+X, L` panel renders the
    /// retract mark, read from `retracted_at_ms`, BESIDE THE KIND: ahead of the
    /// time and the peer hash, so the panel's own truncation reaches it last.
    /// Driven through `render`, the panel's front door, into a real buffer.
    ///
    /// **Mutant → RED (`M22`):** revert the panel's mark read — the marked row
    /// renders no `[retracted …]`.
    /// **Positive control:** the unmarked row renders no mark, so the assertion
    /// is reading the field rather than a constant.
    #[test]
    fn a_retracted_row_renders_its_mark_beside_the_kind_and_an_unmarked_row_none() {
        let mut marked = row(1, Some(1_700_000_000_000));
        marked.kind = TransparencyKind::Disclosed;
        marked.summary = "disclosed 42 bytes".to_owned();
        marked.retracted_at_ms = Some(1_700_000_060_000);
        let unmarked = row(2, Some(1_700_000_000_000));
        let mut state = TransparencyPanelState::default();
        state.apply_read(vec![marked, unmarked], 10);

        let area = Rect::new(0, 0, 140, 10);
        let mut buf = Buffer::empty(area);
        render(
            area,
            &mut buf,
            &mut state,
            0,
            &crate::domain::models::FocusState::Input,
            &theme(),
        );
        let lines: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |cell| cell.symbol()))
                    .collect()
            })
            .collect();

        let marked_line = lines
            .iter()
            .find(|line| line.contains("disclosed 42 bytes"))
            .unwrap_or_else(|| panic!("the marked row renders: {lines:#?}"));
        let mark = marked_line
            .find("[retracted 2023-11-14 22:14:20Z]")
            .unwrap_or_else(|| panic!("the mark renders on the panel: {marked_line}"));
        let kind = marked_line.find("disclosed").expect("the kind label");
        let peer = marked_line.find("peer-a").expect("the peer");
        assert!(
            kind < mark && mark < peer,
            "the mark sits beside the kind, ahead of the peer hash: {marked_line}"
        );

        let unmarked_line = lines
            .iter()
            .find(|line| line.contains("refused by policy"))
            .unwrap_or_else(|| panic!("the unmarked row renders: {lines:#?}"));
        assert!(!unmarked_line.contains("[retracted"), "{unmarked_line}");
    }

    #[test]
    fn a_refresh_while_reading_counts_new_rows_instead_of_moving_the_viewport() {
        let mut state = TransparencyPanelState::default();
        let mut selected = 0;
        state.apply_read(vec![row(1, Some(1)), row(2, Some(2))], 10);
        state.set_viewport_rows(2, &mut selected);
        state.open_at_tail(&mut selected);
        assert_eq!(
            state.newer_entries, 2,
            "selecting the tail must not acknowledge it before render"
        );
        state.acknowledge_rendered_boundary();
        assert_eq!(state.newer_entries, 0, "rendering acknowledges the tail");

        // The operator scrolls — now they are reading.
        state.scroll_offset = 1;
        state.apply_read(vec![row(1, Some(1)), row(2, Some(2)), row(3, Some(3))], 20);
        assert_eq!(
            state.scroll_offset, 1,
            "a refresh must not yank the view out from under a reader"
        );
        assert_eq!(
            state.newer_entries, 1,
            "…and it must not silently withhold the row either"
        );

        state.acknowledge_rendered_boundary();
        assert_eq!(state.newer_entries, 0);
    }

    #[test]
    fn search_filters_rows_without_dropping_them_from_the_read() {
        let mut state = TransparencyPanelState::default();
        state.apply_read(vec![row(1, Some(1)), row(2, Some(2))], 10);
        state.report.as_mut().unwrap().rows[1].summary = "accepted and executing".to_owned();
        state.search = Some("accepted".to_owned());
        assert_eq!(state.visible_rows().len(), 1);
        assert_eq!(
            state.rows().len(),
            2,
            "the read is intact; only the view filters"
        );
    }

    #[test]
    fn the_window_never_indexes_past_the_end() {
        assert_eq!(visible_slice(0, 0, 10), (0, 0));
        assert_eq!(visible_slice(3, 99, 10), (0, 3));
        assert_eq!(visible_slice(50, 0, 5), (0, 5));
        assert_eq!(visible_slice(50, 48, 5), (45, 50));
    }
}
