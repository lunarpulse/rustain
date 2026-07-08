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
    ];

    // Live progress: completed / total. ALWAYS shown so a running wave visibly
    // ticks 0/3 → 3/3 — the smoke fix. Without this the strip gave no "what is
    // happening" signal during an in-flight `/fanout`.
    spans.push(Span::styled(" \u{00B7} ", muted));
    spans.push(Span::raw(format!(
        "{}/{} done",
        snap.completed, snap.handle_count
    )));

    // High-salience is an R2/R3 ranked-subset signal. Suppress the "⚠0 high"
    // placeholder when no subset has been computed (R1 is always 0) so the strip
    // never reads as broken/empty.
    if snap.high > 0 {
        spans.push(Span::styled(" \u{00B7} ", muted));
        spans.push(Span::styled(format!("\u{26A0}{} high", snap.high), warn));
    }

    // Cost burn comes from the AuthorityLedger (R2). Suppress the "$0.00 ↑"
    // placeholder until a ledger reading is wired (R1 is always 0).
    if snap.burn_micros > 0 {
        spans.push(Span::styled(" \u{00B7} ", muted));
        spans.push(Span::styled(
            format!("{} \u{2191}", burn_micros_to_usd(snap.burn_micros)),
            burn,
        ));
    }

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

    #[test]
    fn wave_strip_shows_live_completed_progress() {
        // Smoke fix (14.3c): a running wave must tick completed/total so the
        // user sees "what is happening". A mutant that drops the progress span
        // makes this go RED.
        let snap = WaveStripSnapshot {
            handle_count: 3,
            completed: 1,
            ..Default::default()
        };
        let rendered: String = render_wave_strip_line(&snap, true)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(
            rendered.contains("1/3 done"),
            "live completed/total progress must render, got: {rendered}"
        );
    }

    #[test]
    fn wave_strip_suppresses_zero_high_and_zero_burn_placeholders() {
        // R1 has no ranked subset and no ledger reading, so the strip must NOT
        // show the "⚠0 high" / "$0.00" placeholders that read as broken.
        let zero = WaveStripSnapshot {
            handle_count: 2,
            completed: 0,
            ..Default::default()
        };
        let z: String = render_wave_strip_line(&zero, true)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(
            !z.contains("high"),
            "zero high must be suppressed, got: {z}"
        );
        assert!(!z.contains('$'), "zero burn must be suppressed, got: {z}");

        // Positive control: non-zero values DO render — kills a mutant that
        // unconditionally suppresses them.
        let nonzero = WaveStripSnapshot {
            handle_count: 2,
            completed: 0,
            high: 1,
            burn_micros: 400_000,
            ..Default::default()
        };
        let nz: String = render_wave_strip_line(&nonzero, true)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(
            nz.contains("1 high"),
            "non-zero high must render, got: {nz}"
        );
        assert!(nz.contains("$0.40"), "non-zero burn must render, got: {nz}");
    }
}
