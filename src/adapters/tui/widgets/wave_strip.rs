//! `WaveStrip` — the always-one-line wave status strip (Story 14.3, AC6/AC7).
//!
//! Height is **never proportional to N** (responsive invariant: test N∈{1,8,50,
//! 500} all render exactly one line). Reads executor state + the cost ledger +
//! the `EventBus` (it does NOT need 14.4's status bus — it is fed a compact
//! render snapshot). Cancel-all is WIRED (not a label): the caller passes a
//! `cancel_all: bool` rendered as an action affordance, and the strip surfaces
//! the wave's paused/cancelled state.
//!
//! Grammar mirrors `status_bar`: a free function returning a single `Line`.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::domain::models::orchestration::SpokeResult;

/// Compact snapshot the WaveStrip renders from. The executor / event loop
/// builds this from executor state + the cost ledger; the widget itself touches
/// no async state (pure render).
#[derive(Clone, Debug, Default)]
pub struct WaveStripSnapshot {
    /// Total dispatched spoke handles.
    pub handle_count: usize,
    /// Completed spokes contributing signal.
    pub completed: usize,
    /// High-salience spokes (a ranked subset the user should attend to).
    pub high: usize,
    /// Spokes that failed / were cancelled / came back empty.
    pub degraded: usize,
    /// Cumulative consumed cost micros under the coordinator's budget
    /// (`AuthorityLedger::conservation().consumed.cost_micros`).
    pub burn_micros: u64,
    /// `true` when the wave auto-paused at the budget ceiling (AC10).
    pub paused: bool,
    /// `true` when cancel-all fired (the affordance is now spent).
    pub cancelled: bool,
}

impl WaveStripSnapshot {
    /// Build a snapshot from a slice of spoke results + a cost reading + the
    /// high-salience count. `high` is a SEPARATE input (P20): it is NOT derived
    /// from `completed` (the prior `high = completed` made the high-salience
    /// indicator a duplicate of the completed count). R1 callers pass the
    /// ranked-subset size (0 until 14.3a's adaptive gate computes one);
    /// `paused` / `cancelled` come from executor state via [`Self::paused`] /
    /// [`Self::cancelled`] (the event loop maps `BudgetPaused` / `WaveCancelled`).
    pub fn from_results(results: &[SpokeResult], burn_micros: u64, high: usize) -> Self {
        let mut completed = 0;
        let mut degraded = 0;
        for r in results {
            if r.is_signal() {
                completed += 1;
            } else {
                degraded += 1;
            }
        }
        Self {
            handle_count: results.len(),
            completed,
            high,
            degraded,
            burn_micros,
            paused: false,
            cancelled: false,
        }
    }

    /// Surface the budget-ceiling auto-pause state (AC10). The event loop sets
    /// this from `OrchestrationError::BudgetPaused`; the widget itself touches
    /// no executor state (pure render).
    pub fn paused(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }

    /// Surface the cancel-all state (AC10). The event loop sets this from
    /// `AppEvent::WaveCancelled`.
    pub fn cancelled(mut self, cancelled: bool) -> Self {
        self.cancelled = cancelled;
        self
    }
}

/// Render the WaveStrip as a SINGLE line, regardless of `handle_count`.
///
/// Format: `▸ N handles · ⚠H high · $burn ↑ · cancel all` (or `· paused` /
/// `· cancelled ⊘` when the wave is paused/cancelled). Monochrome-safe (color
/// decorates the glyph, which carries meaning).
pub fn render_wave_strip_line(snap: &WaveStripSnapshot, cancel_all_wired: bool) -> Line<'static> {
    let glyph = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let warn = Style::default().fg(Color::Yellow);
    let muted = Style::default().fg(Color::DarkGray);
    let burn = Style::default().fg(Color::Magenta);

    let mut spans: Vec<Span> = vec![
        Span::styled("\u{25B8} ", glyph), // ▸ wave handle
        Span::raw(format!("{} handles", snap.handle_count)),
        Span::styled(" \u{00B7} ", muted), // ·
        Span::styled(format!("\u{26A0}{} high", snap.high), warn),
        Span::styled(" \u{00B7} ", muted),
        Span::styled(
            format!("{} \u{2191}", burn_micros_to_usd(snap.burn_micros)),
            burn,
        ),
    ];

    if snap.paused {
        spans.push(Span::styled(" \u{00B7} ", muted));
        spans.push(Span::styled("paused", warn));
    }
    if snap.cancelled {
        spans.push(Span::styled(" \u{00B7} ", muted));
        spans.push(Span::styled("cancelled \u{2298}", warn));
    }

    // Cancel-all is WIRED (an action affordance), not a decorative label.
    if cancel_all_wired && !snap.cancelled {
        spans.push(Span::styled(" \u{00B7} ", muted));
        spans.push(Span::styled(
            "cancel all",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }

    Line::from(spans)
}

/// Render micro-dollars (1_000_000 micros = $1.00) as a fixed-point USD
/// string (P21: never `f64` for money — integer cents, formatted directly).
/// Returns e.g. `$0.40`, `$1.00`, `$1234.56`.
fn burn_micros_to_usd(micros: u64) -> String {
    let dollars = micros / 1_000_000;
    let cents = (micros % 1_000_000) / 10_000;
    format!("${dollars}.{cents:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wave_strip_is_always_one_line_regardless_of_n() {
        // The responsive invariant: N∈{1,8,50,500} all render exactly ONE line.
        for &n in &[1usize, 8, 50, 500] {
            let results: Vec<SpokeResult> = (0..n)
                .map(|i| SpokeResult::Completed {
                    summary: format!("s{i}"),
                })
                .collect();
            // P20: high is a separate input (here 0 — no ranked subset in R1).
            let snap = WaveStripSnapshot::from_results(&results, 1_500_000, 0);
            let line = render_wave_strip_line(&snap, true);
            // A single Line is height 1 by construction.
            assert_eq!(
                line.spans
                    .iter()
                    .map(|s| s.content.chars().filter(|c| *c == '\n').count())
                    .sum::<usize>(),
                0,
                "N={n}: wave strip must contain no newlines (one line)"
            );
            let rendered: String = line.spans.iter().map(|s| s.content.clone()).collect();
            assert!(
                rendered.contains(&format!("{n} handles")),
                "N={n}: handle count rendered"
            );
        }
    }

    #[test]
    fn high_is_distinct_from_completed_not_a_duplicate() {
        // P20: the high-salience indicator must NOT duplicate the completed
        // count. With 3 completed + high=1 the strip renders both numbers.
        let results: Vec<SpokeResult> = (0..3)
            .map(|_| SpokeResult::Completed {
                summary: "s".into(),
            })
            .collect();
        let snap = WaveStripSnapshot::from_results(&results, 0, 1);
        assert_eq!(snap.completed, 3);
        assert_eq!(snap.high, 1);
        let rendered: String = render_wave_strip_line(&snap, true)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(rendered.contains("1 high"), "high renders its own value");
    }

    #[test]
    fn burn_micros_renders_fixed_point_usd_not_float() {
        // P21: integer-cent money. $0.40, $1.00, $0.00 — no float artifacts.
        for (micros, expect) in &[
            (0u64, "$0.00"),
            (400_000, "$0.40"),
            (1_000_000, "$1.00"),
            (1_400_000, "$1.40"),
        ] {
            assert_eq!(burn_micros_to_usd(*micros), *expect);
        }
    }

    #[test]
    fn wave_strip_surfaces_paused_and_cancelled_state() {
        let snap = WaveStripSnapshot::from_results(
            &[SpokeResult::Completed {
                summary: "x".into(),
            }],
            0,
            0,
        )
        .paused(true);
        let rendered: String = render_wave_strip_line(&snap, true)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(rendered.contains("paused"));

        let snap2 = WaveStripSnapshot::from_results(
            &[SpokeResult::Completed {
                summary: "x".into(),
            }],
            0,
            0,
        )
        .cancelled(true);
        let rendered2: String = render_wave_strip_line(&snap2, true)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(rendered2.contains("cancelled"));
    }

    #[test]
    fn cancel_all_affordance_only_when_wired_and_not_cancelled() {
        let snap = WaveStripSnapshot::from_results(
            &[SpokeResult::Completed {
                summary: "x".into(),
            }],
            0,
            0,
        );
        let wired = render_wave_strip_line(&snap, true);
        let not_wired = render_wave_strip_line(&snap, false);
        let wired_s: String = wired.spans.iter().map(|s| s.content.clone()).collect();
        let not_wired_s: String = not_wired.spans.iter().map(|s| s.content.clone()).collect();
        assert!(wired_s.contains("cancel all"));
        assert!(!not_wired_s.contains("cancel all"));
    }
}
