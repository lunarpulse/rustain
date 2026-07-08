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

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

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
    Line::styled(
        "[j/k scroll \u{00B7} \u{21B5} drill \u{00B7} r re-run \u{00B7} d diverge \u{00B7} Esc close]",
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
}
