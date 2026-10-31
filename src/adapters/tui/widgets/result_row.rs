//! `ResultRow` — one row in the WaveOverlay spoke list (Story 14.3a, AC5).
//!
//! Pure render of a single spoke's terminal outcome + ownership + dispatch
//! slot + rerun state. Each row is **exactly one line** for ALL N — the
//! overlay holds a `Vec<ResultRowSnapshot>` and renders each via
//! [`render_result_row`]; row height never grows with wave size.
//!
//! Glyphs come entirely from the shared `orchestration_glyph` palette
//! (AC6/AC7) — this module defines NO new glyph constants. Salience is
//! **enum-derived** (DD3): exceptional variants render a stable
//! `⚠ <status-token>` (never raw child text); successful rows render the
//! spoke `summary` lede clipped to [`ROW_SALIENCE_MAX_BYTES`].

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::adapters::tui::text_geometry as geom;
use crate::domain::models::node_state::NodeState;
use crate::domain::models::orchestration::SpokeResult;
use crate::domain::models::subagent_view::OwnershipKind;

use super::orchestration_glyph::{node_state_glyph, ownership_glyph, spoke_result_glyph};

/// Byte cap for the success-row salience lede. Tighter than the prompt-window
/// cap (`SPOKE_SUMMARY_MAX_BYTES = 240`): a row is a one-line affordance, not a
/// reading surface, so its summary is re-clipped here.
pub const ROW_SALIENCE_MAX_BYTES: usize = 120;

/// Max display cells for the agent label before truncation.
const AGENT_LABEL_MAX_CELLS: usize = 20;

/// Compact, pure snapshot of one spoke's render-relevant state.
///
/// The WaveOverlay owns a `Vec<ResultRowSnapshot>` and calls
/// [`render_result_row`] per row. `Clone`/`Debug` so the overlay can
/// snapshot-copy rows; `Default` defaults `result` to [`SpokeResult::Empty`]
/// (the neutral no-signal outcome — `SpokeResult` itself has no `Default`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultRowSnapshot {
    /// The spoke's display label (truncated to ~20 chars at render time).
    pub agent_label: String,
    /// Terminal outcome of the spoke (the last result if currently rerunning).
    pub result: SpokeResult,
    /// Dispatch-order index (0-based).
    pub slot: usize,
    /// `true` if this spoke is the interactive root (`Self_`); else `Owned`.
    pub is_self: bool,
    /// How many times this slot has been rerun so far.
    pub rerun_count: u8,
    /// `true` while this slot is currently being rerun (the in-progress lamp).
    pub rerunning: bool,
}

impl Default for ResultRowSnapshot {
    fn default() -> Self {
        Self {
            agent_label: String::new(),
            result: SpokeResult::Empty,
            slot: 0,
            is_self: false,
            rerun_count: 0,
            rerunning: false,
        }
    }
}

/// Render a single [`ResultRowSnapshot`] as ONE line.
///
/// Format: `<own><state> name · <salience> · ↵`
///
/// - `<own>` — [`ownership_glyph`] (`★` Self_ / `♦` Owned).
/// - `<state>` — [`spoke_result_glyph`] for the terminal outcome, OR the
///   Running glyph `▶` ([`node_state_glyph`] of [`NodeState::Running`]) while
///   `rerunning` is set (the in-progress lamp overrides the stale glyph).
/// - `name` — `agent_label` clipped to 20 display cells at a whole grapheme.
/// - `<salience>` — enum-derived (DD3): `⚠ failed` / `⚠ cancelled` /
///   `⚠ empty` for exceptional variants; the `summary` lede for `Completed`,
///   capped to [`ROW_SALIENCE_MAX_BYTES`] bytes (at a grapheme boundary) and
///   then to the row's remaining display cells.
/// - `↵` — drill affordance, always present (muted).
///
/// `width` is honored as a display-column budget: after the byte cap the
/// salience is clipped, at whole graphemes, to the cells remaining on the row
/// so a narrow overlay never overflows. While `rerunning` the whole line is
/// dimmed (the row is in-flight).
pub fn render_result_row(row: &ResultRowSnapshot, width: u16) -> Line<'static> {
    let muted = Style::default().fg(Color::DarkGray);
    // Compose DIM into every segment when the row is in-flight (rerunning),
    // so the whole line dims uniformly while preserving per-segment color.
    let dim_if = |style: Style| {
        if row.rerunning {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };

    let own = ownership_glyph(if row.is_self {
        OwnershipKind::self_root()
    } else {
        OwnershipKind::Owned
    });
    // While rerunning the state glyph is the in-progress lamp (●), regardless
    // of the (stale) terminal result beneath it.
    let state = if row.rerunning {
        node_state_glyph(NodeState::Running)
    } else {
        spoke_result_glyph(&row.result)
    };

    let name = geom::clip(&row.agent_label, AGENT_LABEL_MAX_CELLS);

    // Salience — enum-derived (DD3). Two independent bounds: the 120-BYTE
    // source cap (grapheme-safe) and the row's remaining display cells.
    let salience_str = salience_full(&row.result);
    let salience_style = salience_style(&row.result);
    let sep = " \u{00B7} "; // ·
    let drill = "\u{21B5}"; // ↵
    let overhead_width = own.width()
        + state.width()
        + " ".width()
        + name.width()
        + sep.width()
        + sep.width()
        + drill.width();
    let remaining = (width as usize).saturating_sub(overhead_width);
    let salience = geom::clip(
        clip_grapheme_bytes(&salience_str, ROW_SALIENCE_MAX_BYTES),
        remaining,
    )
    .to_string();

    let spans: Vec<Span<'static>> = vec![
        Span::styled(own.to_string(), dim_if(muted)),
        Span::styled(state.to_string(), dim_if(muted)),
        Span::styled(" ".to_string(), dim_if(muted)),
        Span::styled(name.to_string(), dim_if(Style::default())),
        Span::styled(sep.to_string(), dim_if(muted)),
        Span::styled(salience, dim_if(salience_style)),
        Span::styled(sep.to_string(), dim_if(muted)),
        Span::styled(drill.to_string(), dim_if(muted)),
    ];

    Line::from(spans)
}

/// Build the full (un-clipped) salience string for a spoke outcome.
///
/// Exceptional variants carry a stable `⚠ <status-token>` (never raw child
/// text); `Completed` carries the raw `summary` lede (the `✓` state glyph
/// already precedes it, so no glyph prefix here).
fn salience_full(result: &SpokeResult) -> String {
    match result {
        SpokeResult::Failed { .. } => "\u{26A0} failed".to_string(),
        SpokeResult::Cancelled => "\u{26A0} cancelled".to_string(),
        SpokeResult::Empty => "\u{26A0} empty".to_string(),
        SpokeResult::Completed { summary } => summary.clone(),
    }
}

/// Per-variant salience color (color decorates the glyph-carries-meaning row).
fn salience_style(result: &SpokeResult) -> Style {
    match result {
        SpokeResult::Failed { .. } | SpokeResult::Cancelled => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM),
        SpokeResult::Empty => Style::default().add_modifier(Modifier::DIM),
        SpokeResult::Completed { .. } => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::DIM),
    }
}

/// Byte-capped prefix that ends on a whole grapheme: the `max_bytes` ceiling
/// is a source limit (not columns), so a cluster straddling it is dropped
/// whole rather than kept as a fragment.
fn clip_grapheme_bytes(s: &str, max_bytes: usize) -> &str {
    let cut = clip_bytes(s, max_bytes);
    if cut.len() == s.len() {
        return cut;
    }
    let scalars = cut.chars().count();
    &s[..geom::byte_of_scalar(s, geom::snap_boundary(s, scalars))]
}

/// Compatibility helper kept for existing UTF-8 boundary tests.
fn clip_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, result: SpokeResult) -> ResultRowSnapshot {
        ResultRowSnapshot {
            agent_label: label.to_string(),
            result,
            slot: 0,
            is_self: false,
            rerun_count: 0,
            rerunning: false,
        }
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn is_dim(style: &Style) -> bool {
        style.add_modifier.contains(Modifier::DIM)
    }

    #[test]
    fn completed_renders_clipped_summary_lede() {
        let r = row(
            "alpha",
            SpokeResult::Completed {
                summary: "found 3 races".into(),
            },
        );
        let line = render_result_row(&r, 80);
        let t = text(&line);
        assert!(t.contains("alpha"), "label present: {t}");
        assert!(t.contains("found 3 races"), "summary lede present: {t}");
        assert!(t.contains("\u{2713}"), "✓ state glyph present: {t}");
        // salience (span[5]) is dim green.
        let sal = &line.spans[5];
        assert_eq!(sal.style.fg, Some(Color::Green));
        assert!(is_dim(&sal.style));
    }

    #[test]
    fn completed_summary_clipped_to_row_salience_cap() {
        let long = "x".repeat(300);
        let r = row("alpha", SpokeResult::Completed { summary: long });
        let line = render_result_row(&r, 1000);
        let t = text(&line);
        let xs: String = t.chars().filter(|c| *c == 'x').collect();
        assert!(
            xs.len() <= ROW_SALIENCE_MAX_BYTES,
            "summary clipped <= {ROW_SALIENCE_MAX_BYTES}: got {}",
            xs.len()
        );
        assert!(!xs.is_empty(), "non-empty summary");
    }

    #[test]
    fn failed_renders_warning_token_without_child_reason() {
        let r = row(
            "beta",
            SpokeResult::Failed {
                reason: "model timeout\nsecret payload".into(),
            },
        );
        let line = render_result_row(&r, 80);
        let t = text(&line);
        assert!(t.contains("\u{26A0} failed"), "warning token: {t}");
        assert!(
            !t.contains("model timeout"),
            "child reason must not leak: {t}"
        );
        assert!(
            !t.contains("secret payload"),
            "child payload must not leak: {t}"
        );
        assert!(t.contains("\u{2717}"), "✗ state glyph present: {t}");
        assert!(t.contains("beta"));
        let sal = &line.spans[5];
        assert_eq!(sal.style.fg, Some(Color::Yellow));
        assert!(is_dim(&sal.style));
    }

    #[test]
    fn cancelled_renders_warning_token() {
        let r = row("gamma", SpokeResult::Cancelled);
        let line = render_result_row(&r, 80);
        let t = text(&line);
        assert!(t.contains("\u{26A0} cancelled"), "{t}");
        assert!(t.contains("\u{2298}"), "⊘ state glyph present: {t}");
    }

    #[test]
    fn empty_renders_warning_token() {
        let r = row("delta", SpokeResult::Empty);
        let line = render_result_row(&r, 80);
        let t = text(&line);
        assert!(t.contains("\u{26A0} empty"), "{t}");
        assert!(t.contains("\u{2205}"), "∅ state glyph present: {t}");
    }

    #[test]
    fn rerunning_overrides_state_glyph_to_running_and_dims() {
        let mut r = row(
            "epsilon",
            SpokeResult::Completed {
                summary: "ok".into(),
            },
        );
        r.rerunning = true;
        let line = render_result_row(&r, 80);
        let t = text(&line);
        assert!(
            t.contains("\u{25B6}"),
            "▶ Running glyph present (override): {t}"
        );
        assert!(
            !t.contains("\u{2713}"),
            "stale ✓ state glyph overridden: {t}"
        );
        // whole line dimmed: every span carries DIM
        assert!(
            line.spans.iter().all(|s| is_dim(&s.style)),
            "rerunning line is fully dimmed"
        );
    }

    #[test]
    fn is_self_renders_self_ownership_glyph() {
        let mut r = row(
            "root",
            SpokeResult::Completed {
                summary: "s".into(),
            },
        );
        r.is_self = true;
        let t = text(&render_result_row(&r, 80));
        assert!(t.contains("\u{2605}"), "★ Self_ glyph: {t}");
    }

    #[test]
    fn owned_renders_owned_ownership_glyph() {
        let r = row(
            "child",
            SpokeResult::Completed {
                summary: "s".into(),
            },
        );
        let t = text(&render_result_row(&r, 80));
        assert!(t.contains("\u{2666}"), "♦ Owned glyph: {t}");
    }

    #[test]
    fn each_variant_renders_distinctly() {
        let completed = text(&render_result_row(
            &row(
                "a",
                SpokeResult::Completed {
                    summary: "s".into(),
                },
            ),
            80,
        ));
        let failed = text(&render_result_row(
            &row("a", SpokeResult::Failed { reason: "r".into() }),
            80,
        ));
        let cancelled = text(&render_result_row(&row("a", SpokeResult::Cancelled), 80));
        let empty = text(&render_result_row(&row("a", SpokeResult::Empty), 80));
        let all = [&completed, &failed, &cancelled, &empty];
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert_ne!(all[i], all[j], "variants {i} and {j} render identically");
            }
        }
    }

    #[test]
    fn clip_bytes_never_splits_a_codepoint() {
        let s = "🦀🦀🦀"; // 12 bytes, each emoji 4 bytes
        assert_eq!(clip_bytes(s, 6), "🦀"); // floors to one emoji (4 bytes)
        assert_eq!(clip_bytes(s, 4), "🦀");
        assert_eq!(clip_bytes(s, 3), ""); // cannot fit even one emoji
        assert_eq!(clip_bytes("abc", 10), "abc"); // under cap → unchanged
    }

    #[test]
    fn agent_label_truncates_at_cell_budget() {
        // Was `clip_chars_truncates_at_char_count` (scalar budget); the label
        // budget is now 20 display cells, so 🦀 (2 cells) counts double.
        let label_of = |label: &str| {
            let line = render_result_row(
                &row(
                    label,
                    SpokeResult::Completed {
                        summary: "s".into(),
                    },
                ),
                120,
            );
            line.spans[3].content.to_string()
        };
        assert_eq!(label_of("hello world"), "hello world");
        assert_eq!(label_of(&"a".repeat(25)), "a".repeat(20)); // over cap → clipped
        assert_eq!(label_of(&"🦀".repeat(3)), "🦀🦀🦀"); // 6 cells, under cap
        assert_eq!(label_of(&"🦀".repeat(12)), "🦀".repeat(10)); // 20 cells, not 20 scalars
    }

    #[test]
    fn drill_affordance_always_present() {
        for result in [
            SpokeResult::Completed {
                summary: "s".into(),
            },
            SpokeResult::Failed { reason: "r".into() },
            SpokeResult::Cancelled,
            SpokeResult::Empty,
        ] {
            let t = text(&render_result_row(&row("a", result), 80));
            assert!(t.contains("\u{21B5}"), "↵ drill affordance present: {t}");
        }
    }
}
