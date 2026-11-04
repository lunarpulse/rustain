//! `WaveOverlay` — the virtual-scrolled fan-out result overlay (Story 14.3a, AC5).
//!
//! Collapsed, the overlay is ONE line regardless of the spoke count N — the
//! responsive invariant (test N∈{1, 8, 50, 500} all collapse to exactly one
//! line: width/height are never proportional to N). Expanded, it virtual-scrolls
//! a window of `viewport_height - 2` rows centered on `selected`, framing each
//! spoke via [`super::result_row::render_result_row`], with a `▾ k of N` scroll
//! indicator header and a `[j/k scroll · ↵ drill · r re-run · d diverge · Esc
//! close]` hint footer.
//!
//! Pure render: no async, no mutation. Composes with the chat-pane render
//! pipeline like `synthesis_block` / `wave_strip`. Glyphs are sourced from
//! `orchestration_glyph` via the per-row renderer — no new glyph constants.
//!
//! Expanded list, drill and divergence are painted only by `render_frame` in
//! the owning Stage rectangle. The line builder remains a pure widget contract.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::keymap::{
    render::{key_text, key_text_at},
    table_panels::PanelCmd,
};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::domain::ports::wave_handle::WaveSnapshot;

use super::diverge_view::{DivergeSnapshot, render_diverge_view};
use super::modal::{ModalPart, fit_modal};

use super::result_row::{ResultRowSnapshot, render_result_row};

/// Compact snapshot the WaveOverlay renders from. The event loop builds this
/// from the [`WaveHandle`](crate::domain::ports::wave_handle::WaveHandle)
/// snapshot (one [`ResultRowSnapshot`] per spoke); the widget itself touches no
/// async state (pure render).
#[derive(Clone, Debug, Default)]
pub struct WaveOverlaySnapshot {
    /// One row per spoke, in dispatch order.
    pub rows: Vec<ResultRowSnapshot>,
    /// Currently selected row index (j/k scroll). Clamped to `rows.len()-1` at
    /// render time, so an out-of-range value never panics.
    pub selected: usize,
    /// `true` = render only the single-line indicator (collapsed); `false` =
    /// render the virtual-scrolled body + header/footer (expanded).
    pub collapsed: bool,
    /// Available lines for the expanded overlay (header + body + footer). The
    /// body window is `viewport_height - 2` rows.
    pub viewport_height: u16,
}

/// Render the WaveOverlay.
///
/// **Collapsed** (`snap.collapsed == true`): returns EXACTLY one line,
/// `▾ {selected+1} of {total}` — height is independent of N (the N≈8 overflow
/// guard generalised to all N).
///
/// **Expanded**: returns `2 + min(viewport_height - 2, total)` lines — a
/// `▾ k of N` header, a virtual-scrolled window of body rows centered on the
/// selected row (each via `render_result_row`), and a keybinding-hint footer.
/// The selected body row is highlighted with reverse video.
pub fn render_wave_overlay(snap: &WaveOverlaySnapshot, width: u16) -> Vec<Line<'static>> {
    let total = snap.rows.len();
    let selected = if total == 0 {
        0
    } else {
        snap.selected.min(total - 1)
    };

    if snap.collapsed {
        // Collapsed: EXACTLY one line, regardless of N (responsive invariant).
        return vec![position_line(total, selected)];
    }

    let mut lines = Vec::new();
    // Header: scroll indicator.
    lines.push(position_line(total, selected));

    // Virtual-scroll window of (viewport_height - 2) body rows, centered on the
    // selected row. The window never grows with N — only with viewport_height.
    let visible_rows = usize::from(snap.viewport_height).saturating_sub(2);
    let (start, count) = visible_window(total, selected, visible_rows);
    for (offset, row) in snap.rows[start..start + count].iter().enumerate() {
        let mut line = render_result_row(row, width);
        if start + offset == selected {
            line = highlight_selected(line);
        }
        lines.push(line);
    }

    // Footer: keybinding hint.
    lines.push(hint_footer());
    lines
}

/// Paint the frame's complete view, without consulting or changing base focus.
/// The caller supplies one borrowed snapshot; only visible list rows are
/// projected into the existing ResultRow builder.
pub fn render_frame(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &TuiState,
    snap: &WaveSnapshot,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(" Wave ");
    let inner = block.inner(area);
    let width = inner.width;
    let height = usize::from(inner.height);
    let total = snap.outcome.spokes.len();
    let selected = state.wave_overlay_selected.min(total.saturating_sub(1));
    let (header, rows, selected_row) = if state.wave_diverge_open {
        let mut lines = render_diverge_view(&DivergeSnapshot::new(
            snap.outcome
                .spokes
                .iter()
                .enumerate()
                .map(|(slot, (_, result))| (format!("SPOKE-{slot}"), result.clone()))
                .collect(),
            width,
        ));
        let header = lines.remove(0);
        let row = if width >= 120 { selected / 2 } else { selected };
        (header, lines, row)
    } else if let Some((slot, text)) = &state.wave_drill_body {
        (
            Line::from(format!("Drill: spoke {slot}")),
            text.lines()
                .take(height)
                .map(|line| Line::raw(line.to_owned()))
                .collect(),
            0,
        )
    } else {
        // Limit work by the actual frame height, not by the number of spokes.
        let (start, count) = visible_window(total, selected, height.saturating_sub(2));
        let rows = snap.outcome.spokes[start..start + count]
            .iter()
            .enumerate()
            .map(|(offset, (agent, result))| {
                let slot = start + offset;
                let row = ResultRowSnapshot {
                    agent_label: agent.as_str().to_owned(),
                    result: result.clone(),
                    slot,
                    is_self: false,
                    rerun_count: snap.rerun_counts.get(slot).copied().unwrap_or(0),
                    rerunning: state.rerunning_slot == Some(slot),
                };
                let line = render_result_row(&row, width);
                if slot == selected {
                    highlight_selected(line)
                } else {
                    line
                }
            })
            .collect();
        (
            position_line(total, selected),
            rows,
            selected.saturating_sub(start),
        )
    };
    let footer = if state.wave_diverge_open {
        format!(
            "{} back · {} back · {} back",
            key_text(PanelCmd::WaveDivergeClose),
            key_text(PanelCmd::WaveBack),
            key_text_at(PanelCmd::WaveClose, 1)
        )
    } else if state.wave_chat_keys {
        format!(
            "{}/{} overlay · {} diverge · {} dismiss · {} close",
            key_text(PanelCmd::WaveOpenOverlay),
            key_text(PanelCmd::WaveOpenFromChat),
            key_text(PanelCmd::WaveOpenDiverge),
            key_text(PanelCmd::WaveBack),
            key_text_at(PanelCmd::WaveClose, 1)
        )
    } else {
        format!(
            "{} drill · {} rerun · {} diverge · {}/{} close",
            key_text(PanelCmd::WaveDrill),
            key_text(PanelCmd::WaveRerun),
            key_text(PanelCmd::WaveDiverge),
            key_text(PanelCmd::WaveClose),
            key_text_at(PanelCmd::WaveClose, 1)
        )
    };
    let detail = if state.wave_chat_keys {
        format!(
            "{} overlay · {} select · {} diverge",
            key_text(PanelCmd::WaveOpenOverlay),
            key_text(PanelCmd::WaveOpenFromChat),
            key_text(PanelCmd::WaveOpenDiverge)
        )
    } else {
        format!(
            "{}/{} select · {} opens the selected spoke",
            key_text(PanelCmd::WaveNext),
            key_text(PanelCmd::WavePrev),
            key_text(PanelCmd::WaveDrill)
        )
    };
    let lines = fit_modal(
        vec![
            ModalPart::Header(vec![header]),
            ModalPart::Notice(vec![Line::from(detail)]),
            ModalPart::Rows {
                lines: rows,
                selected: (selected_row, selected_row + 1),
            },
            ModalPart::Footer(vec![Line::styled(
                footer,
                Style::default().fg(theme.colors.fg_muted),
            )]),
        ],
        height,
        usize::from(width),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
}

/// The single shared `▾ k of N` indicator line. `pos` is the 1-indexed selected
/// position (`selected + 1`), or `0` when there are no rows.
fn position_line(total: usize, selected: usize) -> Line<'static> {
    let pos = if total == 0 { 0 } else { selected + 1 };
    let glyph = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        Span::styled("\u{25BE} ", glyph), // ▾ collapsed/scroll indicator
        Span::raw(format!("{pos}")),
        Span::styled(" of ", muted),
        Span::raw(format!("{total}")),
    ])
}

/// The keybinding-hint footer line (muted, monochrome-safe).
fn hint_footer() -> Line<'static> {
    use crate::adapters::tui::keymap::render::key_text;
    use crate::adapters::tui::keymap::table_panels::PanelCmd as P;
    Line::styled(
        format!(
            "[{}/{} scroll \u{00B7} \u{21B5} drill \u{00B7} {} re-run \u{00B7} {} diverge \u{00B7} {} close]",
            key_text(P::WaveNext),
            key_text(P::WavePrev),
            key_text(P::WaveRerun),
            key_text(P::WaveDiverge),
            key_text(P::WaveClose)
        ),
        Style::default().fg(Color::DarkGray),
    )
}

/// Compute the visible body window `(start, count)` for a virtual scroll that
/// keeps `selected` centered when there is room and clamps at the ends.
///
/// `count = min(visible_rows, total)`; `start` centers `selected` within the
/// window and is clamped to `[0, total - count]`. Returns `(0, 0)` when there
/// are no rows or no room.
fn visible_window(total: usize, selected: usize, visible_rows: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let count = visible_rows.min(total);
    if count == 0 {
        return (0, 0);
    }
    let half = count / 2;
    let max_start = total - count;
    let start = selected.saturating_sub(half).min(max_start);
    (start, count)
}

/// Apply reverse video to every span of a line. Each span already carries its
/// own style (the row's glyph/label colours); augmenting each span's style —
/// rather than the line's base style, which `Line::patch_style` does not
/// propagate to spans — guarantees the whole row reads as selected regardless
/// of how `render_result_row` styled it.
fn highlight_selected(mut line: Line<'static>) -> Line<'static> {
    for span in &mut line.spans {
        span.style = span.style.add_modifier(Modifier::REVERSED);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::orchestration::SpokeResult;

    /// Build `n` synthetic spoke rows, mixing Completed/Failed so every variant
    /// path through `render_result_row` is exercised by the overlay tests.
    fn make_rows(n: usize) -> Vec<ResultRowSnapshot> {
        (0..n)
            .map(|i| ResultRowSnapshot {
                agent_label: format!("agent-{i}"),
                result: if i % 4 == 0 {
                    SpokeResult::Failed {
                        reason: format!("boom-{i}"),
                    }
                } else {
                    SpokeResult::Completed {
                        summary: format!("done-{i}"),
                    }
                },
                slot: i,
                is_self: i == 0,
                rerun_count: 0,
                rerunning: false,
            })
            .collect()
    }

    /// Flatten a line's spans into a plain string for substring assertions.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// True when any span of the line carries the REVERSED modifier (the
    /// selected-row highlight).
    fn has_reverse(line: &Line) -> bool {
        line.spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
    }

    #[test]
    fn collapsed_renders_exactly_one_line_regardless_of_n() {
        for &n in &[1_usize, 8, 50, 500] {
            let snap = WaveOverlaySnapshot {
                rows: make_rows(n),
                selected: 0,
                collapsed: true,
                viewport_height: 20,
            };
            let lines = render_wave_overlay(&snap, 80);
            assert_eq!(lines.len(), 1, "collapsed must be exactly 1 line for N={n}");
        }
    }

    #[test]
    fn collapsed_line_shows_selected_position_and_total() {
        let snap = WaveOverlaySnapshot {
            rows: make_rows(8),
            selected: 2,
            collapsed: true,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        assert_eq!(lines.len(), 1);
        let txt = line_text(&lines[0]);
        assert!(txt.contains('\u{25BE}'), "indicator glyph present: {txt}");
        // selected is 0-indexed 2 → displayed position is 3.
        assert!(
            txt.contains("3 of 8"),
            "position is selected+1 of total: {txt}"
        );
    }

    #[test]
    fn expanded_renders_header_footer_and_all_rows_when_room() {
        let snap = WaveOverlaySnapshot {
            rows: make_rows(3),
            selected: 1,
            collapsed: false,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        // header + 3 body rows + footer.
        assert_eq!(lines.len(), 5);
        assert!(
            line_text(&lines[0]).contains('\u{25BE}'),
            "header is the indicator"
        );
        let footer = line_text(lines.last().unwrap());
        assert!(
            footer.contains("scroll"),
            "footer has scroll hint: {footer}"
        );
        assert!(footer.contains("drill"), "footer has drill hint: {footer}");
        assert!(
            footer.contains("re-run"),
            "footer has re-run hint: {footer}"
        );
        assert!(
            footer.contains("diverge"),
            "footer has diverge hint: {footer}"
        );
        assert!(
            footer.contains("Esc close"),
            "footer has close hint: {footer}"
        );
    }

    #[test]
    fn expanded_virtual_scrolls_to_viewport_height() {
        // viewport_height 6 → 4 body rows + header + footer = 6, even though N=50.
        let snap = WaveOverlaySnapshot {
            rows: make_rows(50),
            selected: 25,
            collapsed: false,
            viewport_height: 6,
        };
        let lines = render_wave_overlay(&snap, 80);
        assert_eq!(
            lines.len(),
            6,
            "expanded height tracks viewport_height, not N"
        );
    }

    #[test]
    fn expanded_highlights_exactly_the_selected_row_with_reverse() {
        let snap = WaveOverlaySnapshot {
            rows: make_rows(5),
            selected: 2,
            collapsed: false,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        let body = &lines[1..lines.len() - 1];
        let highlighted: Vec<_> = body.iter().filter(|l| has_reverse(l)).collect();
        assert_eq!(highlighted.len(), 1, "exactly one body row is highlighted");
        // selected (index 2) is the 3rd body row when the window starts at 0.
        let highlighted_idx = body.iter().position(|l| has_reverse(l)).unwrap();
        assert_eq!(
            highlighted_idx, 2,
            "the highlighted row is the selected one"
        );
    }

    #[test]
    fn expanded_window_keeps_selected_visible_near_end() {
        let snap = WaveOverlaySnapshot {
            rows: make_rows(50),
            selected: 49,
            collapsed: false,
            viewport_height: 6,
        };
        let lines = render_wave_overlay(&snap, 80);
        let body = &lines[1..lines.len() - 1];
        assert!(
            body.iter().any(|l| has_reverse(l)),
            "selected near the end is still rendered + highlighted"
        );
    }

    #[test]
    fn empty_rows_collapsed_does_not_panic() {
        let snap = WaveOverlaySnapshot {
            rows: vec![],
            selected: 0,
            collapsed: true,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn empty_rows_expanded_shows_only_header_and_footer() {
        let snap = WaveOverlaySnapshot {
            rows: vec![],
            selected: 0,
            collapsed: false,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        // header + footer, zero body rows.
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn selected_out_of_range_is_clamped_to_last_row() {
        let snap = WaveOverlaySnapshot {
            rows: make_rows(5),
            selected: 999,
            collapsed: false,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        let body = &lines[1..lines.len() - 1];
        let highlighted_idx = body.iter().position(|l| has_reverse(l)).unwrap();
        assert_eq!(
            highlighted_idx, 4,
            "out-of-range selected clamps to last index"
        );
    }

    #[test]
    fn collapsed_line_format_includes_total_for_large_n() {
        let snap = WaveOverlaySnapshot {
            rows: make_rows(500),
            selected: 137,
            collapsed: true,
            viewport_height: 20,
        };
        let lines = render_wave_overlay(&snap, 80);
        assert_eq!(lines.len(), 1);
        let txt = line_text(&lines[0]);
        // selected 137 → position 138 of 500.
        assert!(txt.contains("138 of 500"), "large-N collapsed line: {txt}");
    }

    fn retained_snapshot(n: usize) -> WaveSnapshot {
        use crate::domain::models::AgentId;
        use crate::domain::models::orchestration::{ForkJoinOutcome, SynthesisView};
        WaveSnapshot {
            spoke_count: n,
            completed: n,
            honest_empty: Some(false),
            cancelled: false,
            outcome: ForkJoinOutcome {
                spokes: make_rows(n)
                    .into_iter()
                    .map(|row| (AgentId::root(), row.result))
                    .collect(),
                synthesis: SynthesisView::default(),
            },
            resolve_count: 0,
            slots: vec![AgentId::root(); n],
            rerun_counts: vec![0; n],
        }
    }

    #[test]
    fn story21_4_wave_frame_all_views_keep_primary_keys_and_real_selected_data_in_five_rows() {
        use ratatui::{Terminal, backend::TestBackend};
        for view in 0..3 {
            let mut state = TuiState::new(60, 16);
            state.wave_overlay_selected = 49;
            state.wave_diverge_open = view == 2;
            state.wave_chat_keys = view == 2;
            if view == 1 {
                state.wave_drill_body = Some((49, "actual drill body\nsecond body line".into()));
            }
            let snap = retained_snapshot(50);
            let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("full composer payload"),
                        Rect::new(0, 12, 60, 4),
                    );
                    render_frame(frame, Rect::new(0, 1, 60, 5), &state, &snap, &state.theme);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("full composer payload"), "{text}");
            if view == 2 {
                assert!(
                    text.contains("s back") && text.contains("Ctrl+C back"),
                    "{text}"
                );
                assert!(text.contains("SPOKE-49: done-49"), "{text}");
            } else {
                for key in ["Enter drill", "r rerun", "d diverge", "Esc/Ctrl+C close"] {
                    assert!(text.contains(key), "missing {key}: {text}");
                }
                assert!(
                    text.contains(if view == 1 {
                        "actual drill body"
                    } else {
                        "done-49"
                    }),
                    "{text}"
                );
            }
            assert_eq!(state.wave_overlay_selected, 49, "pure render");
        }
    }

    #[test]
    fn story21_4_wave_frame_chat_drill_exposes_dismiss_instead_of_overlay_close() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut state = TuiState::new(60, 16);
        state.wave_chat_keys = true;
        state.wave_drill_body = Some((1, "retained drill after divergence".into()));
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    Rect::new(0, 0, 60, 5),
                    &state,
                    &retained_snapshot(2),
                    &state.theme,
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("retained drill after divergence"), "{text}");
        assert!(
            text.contains("Enter/r overlay")
                && text.contains("Esc dismiss")
                && text.contains("Ctrl+C close"),
            "{text}"
        );
        assert!(!text.contains("Esc/Ctrl+C close"), "{text}");
    }
}
