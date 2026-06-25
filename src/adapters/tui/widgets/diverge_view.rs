//! `DivergeView` — the disagreement pivot over the SAME collected wave handles
//! (Story 14.3a, AC3).
//!
//! Where the WaveStrip / SynthesisBlock present the wave's *agreement* surface
//! (coverage, citations, synthesis), the DivergeView pivots the identical
//! retained spoke outcomes into a *disagreement* view: it surfaces where the
//! fan-out spokes diverge, never silently collapsing a difference.
//!
//! ## Zero re-spawn
//!
//! The view consumes a [`DivergeSnapshot`] built from the retained
//! `WaveHandle`'s `snapshot()` — the same collected spoke results the
//! agreement surface already renders. No spoke is re-run; this is a pure
//! projection of already-collected outcomes (AC3).
//!
//! ## Agreement vs. disagreement
//!
//! - **Agreement**: every spoke is `SpokeResult::Completed` with identical
//!   summary content. Collapses to a single line (`All {N} spokes agree`).
//! - **Disagreement**: any difference in outcomes or summaries. Becomes the
//!   headline: every spoke is listed with its outcome, and no disagreement is
//!   ever dropped (the view degrades chrome, never content).
//!
//! ## Responsive layout
//!
//! - ≥120 cols: side-by-side aligned columns (two columns max, wrapping for
//!   more than two spokes).
//! - <120 cols: stacked list (one spoke per line).
//!
//! Grammar mirrors `synthesis_block` / `wave_strip`: a free function returning
//! `Vec<Line<'static>>` for inline rendering. Pure render — no async, no
//! mutation. Status glyphs come solely from
//! `orchestration_glyph::spoke_result_glyph` (AC7: no new glyph constants).

use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::orchestration_glyph::spoke_result_glyph;
use super::sidebar::truncate_to_width;
use crate::domain::models::SpokeResult;

/// Minimum terminal width (columns) for the side-by-side layout. Below this the
/// view stacks one spoke per line so no disagreement is clipped out of view.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 120;

/// Compact render snapshot for the DivergeView. The caller builds this from the
/// retained `WaveHandle`'s `snapshot()` (the same collected outcomes the
/// agreement surface reads) — zero re-spawn. `spokes` is `(label, result)`
/// pairs; `width` is the available terminal width in columns.
#[derive(Clone, Debug, Default)]
pub struct DivergeSnapshot {
    /// `(label, result)` pairs, one per spoke, in dispatch order.
    pub spokes: Vec<(String, SpokeResult)>,
    /// Available terminal width in columns (drives side-by-side vs. stacked).
    pub width: u16,
}

impl DivergeSnapshot {
    /// Convenience constructor.
    pub fn new(spokes: Vec<(String, SpokeResult)>, width: u16) -> Self {
        Self { spokes, width }
    }
}

/// `true` when every spoke is `Completed` with identical summary content.
///
/// Empty snapshots are vacuously in agreement (nothing to disagree about). A
/// single non-`Completed` spoke, or any two `Completed` spokes whose summaries
/// differ, is a disagreement.
fn is_agreement(spokes: &[(String, SpokeResult)]) -> bool {
    let mut summaries = spokes.iter().map(|(_, r)| match r {
        SpokeResult::Completed { summary } => Some(summary.as_str()),
        _ => None,
    });
    let first = match summaries.next() {
        None => return true,
        Some(Some(s)) => s,
        Some(None) => return false,
    };
    summaries.all(|s| s == Some(first))
}

/// Compact, human-readable outcome text for a spoke (the salience lede for
/// `Completed`; the reason for `Failed`; a stable token otherwise). Used as the
/// per-spoke content in the disagreement list.
fn spoke_display(result: &SpokeResult) -> String {
    match result {
        SpokeResult::Completed { summary } => summary.clone(),
        SpokeResult::Failed { reason } => reason.clone(),
        SpokeResult::Cancelled => "cancelled".to_string(),
        SpokeResult::Empty => "empty".to_string(),
    }
}

/// Render one spoke as `<glyph> <label>: <content>`, clipped to `max_w`. The
/// glyph is the monochrome-safe status glyph from `spoke_result_glyph` (AC7).
fn spoke_line_text(label: &str, result: &SpokeResult, max_w: usize) -> String {
    let glyph = spoke_result_glyph(result);
    let body = format!("{glyph} {label}: {}", spoke_display(result));
    truncate_to_width(&body, max_w)
}

/// Right-pad `s` with spaces to a fixed display width so side-by-side columns
/// align. Display-width-aware (unicode-width) so wide (CJK) glyphs do not skew
/// the column boundary.
fn pad_to_width(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        s.to_string()
    } else {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(width - w));
        out
    }
}

/// Render a side-by-side spoke cell: clipped to `col_w` then padded to `col_w`
/// so the next column starts at a consistent boundary.
fn spoke_cell(label: &str, result: &SpokeResult, col_w: usize) -> String {
    let text = spoke_line_text(label, result, col_w);
    pad_to_width(&text, col_w)
}

/// Render the DivergeView.
///
/// - Agreement collapses to a single line: `All {N} spokes agree`.
/// - Disagreement is the headline: a one-line summary followed by every spoke
///   with its outcome. Layout is side-by-side (≥120 cols, two columns max,
///   wrapping) or stacked (<120 cols, one per line). A disagreement is never
///   dropped — only chrome degrades under width pressure.
pub fn render_diverge_view(snap: &DivergeSnapshot) -> Vec<Line<'static>> {
    let n = snap.spokes.len();
    let mut lines = Vec::new();

    if is_agreement(&snap.spokes) {
        // Agreement collapses to a single line — the whole view is one line.
        lines.push(Line::styled(
            format!("All {n} spokes agree"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        return lines;
    }

    // Disagreement headline.
    lines.push(Line::styled(
        format!("{n} spokes disagree"),
        Style::default().add_modifier(Modifier::BOLD),
    ));

    if snap.width >= SIDE_BY_SIDE_MIN_WIDTH {
        // Side-by-side: two columns max, wrapping for >2 spokes. Each cell is
        // padded to a fixed column width so columns align; the pair is emitted
        // as a single line. No spoke is dropped — odd tails render a lone cell.
        let col_w = usize::from(snap.width) / 2;
        for pair in snap.spokes.chunks(2) {
            let row: String = pair
                .iter()
                .map(|(label, result)| spoke_cell(label, result, col_w))
                .collect();
            lines.push(Line::raw(row));
        }
    } else {
        // Stacked: one spoke per line, clipped to the full width.
        let max_w = usize::from(snap.width).max(1);
        for (label, result) in &snap.spokes {
            lines.push(Line::raw(spoke_line_text(label, result, max_w)));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every line's text into a single owned string for substring checks.
    fn rendered_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>()
    }

    #[test]
    fn agreement_collapses_to_a_single_line() {
        let snap = DivergeSnapshot::new(
            vec![
                (
                    "alpha".to_string(),
                    SpokeResult::Completed {
                        summary: "same answer".into(),
                    },
                ),
                (
                    "beta".to_string(),
                    SpokeResult::Completed {
                        summary: "same answer".into(),
                    },
                ),
                (
                    "gamma".to_string(),
                    SpokeResult::Completed {
                        summary: "same answer".into(),
                    },
                ),
            ],
            120,
        );
        let lines = render_diverge_view(&snap);
        assert_eq!(lines.len(), 1, "agreement collapses to exactly one line");
        let text = rendered_text(&lines);
        assert!(
            text.contains("All 3 spokes agree"),
            "agreement line text: {text:?}"
        );
    }

    #[test]
    fn disagreement_shows_all_spokes() {
        let snap = DivergeSnapshot::new(
            vec![
                (
                    "alpha".to_string(),
                    SpokeResult::Completed {
                        summary: "answer A".into(),
                    },
                ),
                (
                    "beta".to_string(),
                    SpokeResult::Completed {
                        summary: "answer B".into(),
                    },
                ),
                (
                    "gamma".to_string(),
                    SpokeResult::Failed {
                        reason: "boom".into(),
                    },
                ),
            ],
            120,
        );
        let lines = render_diverge_view(&snap);
        let text = rendered_text(&lines);
        assert!(
            text.contains("disagree"),
            "disagreement is the headline: {text:?}"
        );
        // No disagreement dropped: every spoke label is present.
        assert!(
            text.contains("alpha") && text.contains("beta") && text.contains("gamma"),
            "all spoke labels present in disagreement: {text:?}"
        );
    }

    #[test]
    fn wide_width_uses_side_by_side_layout() {
        // 3 disagreeing spokes at width 130 → two columns, wrapping into two
        // data rows: row 0 holds spokes 0,1; row 1 holds spoke 2.
        let snap = DivergeSnapshot::new(
            vec![
                (
                    "alpha".to_string(),
                    SpokeResult::Completed {
                        summary: "answer A".into(),
                    },
                ),
                (
                    "beta".to_string(),
                    SpokeResult::Completed {
                        summary: "answer B".into(),
                    },
                ),
                (
                    "gamma".to_string(),
                    SpokeResult::Completed {
                        summary: "answer C".into(),
                    },
                ),
            ],
            130,
        );
        let lines = render_diverge_view(&snap);
        // header + ceil(3/2) data rows == 1 + 2.
        assert_eq!(
            lines.len(),
            3,
            "side-by-side wraps 3 spokes into 2 data rows: {lines:?}"
        );
        // First data row (index 1) holds TWO spokes side-by-side.
        let row1: String = lines[1].spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            row1.contains("alpha") && row1.contains("beta"),
            "side-by-side places two spokes on one row: {row1:?}"
        );
        // Second data row (index 2) holds the remaining spoke (wrapping).
        let row2: String = lines[2].spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            row2.contains("gamma") && !row2.contains("alpha"),
            "wrapping spills the third spoke to its own row: {row2:?}"
        );
        // No disagreement dropped.
        let text = rendered_text(&lines);
        assert!(text.contains("alpha") && text.contains("beta") && text.contains("gamma"));
    }

    #[test]
    fn narrow_width_stacks_and_drops_no_disagreement() {
        let snap = DivergeSnapshot::new(
            vec![
                (
                    "alpha".to_string(),
                    SpokeResult::Completed {
                        summary: "answer A".into(),
                    },
                ),
                (
                    "beta".to_string(),
                    SpokeResult::Completed {
                        summary: "answer B".into(),
                    },
                ),
                ("gamma".to_string(), SpokeResult::Cancelled),
            ],
            70,
        );
        let lines = render_diverge_view(&snap);
        // header + 3 stacked rows == 4.
        assert_eq!(
            lines.len(),
            4,
            "stacked layout is one spoke per line: {lines:?}"
        );
        // Each data row holds exactly one spoke.
        for (i, label) in ["alpha", "beta", "gamma"].iter().enumerate() {
            let row: String = lines[i + 1]
                .spans
                .iter()
                .map(|s| s.content.clone())
                .collect();
            assert!(row.contains(label), "row {i} holds {label}: {row:?}");
        }
        // Confirm no two spokes share a stacked row (truly one-per-line).
        let r1: String = lines[1].spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            !r1.contains("beta"),
            "stacked rows never combine spokes: {r1:?}"
        );
        // No disagreement dropped even at narrow width.
        let text = rendered_text(&lines);
        assert!(text.contains("alpha") && text.contains("beta") && text.contains("gamma"));
    }
}
