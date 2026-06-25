//! `ExceptionalSpawnGate` — the adaptive, learned-threshold spawn gate
//! (Story 14.3a, **AC6 / DD5**).
//!
//! A fan-out **above** the config-backed remembered threshold surfaces a gate
//! framed as *review-burden* — a [`PermissionCard`](super::permission_prompt)
//! variant — never a per-spawn modal (which trains `y`-mashing; UX `:3354`).
//! **Below** the threshold the gate is silent: the Story 14.3 static
//! `FORK_JOIN_SPAWN_CAP` governs and auto fan-out proceeds with no prompt
//! (UX J-O1 "auto fan-out · NO prompt").
//!
//! The decision itself is a **pure fold** ([`gate_decision`]) with an
//! **injectable** threshold (DD5) so AC6's boundary test can pin it; the widget
//! is a pure render function over a [`SpawnGateSnapshot`] (zero async, zero
//! mutations). All glyphs are reused — the warning mark comes from the existing
//! [`symbols::WARNING`](crate::domain::models::visual::symbols::WARNING); the
//! affordance punctuation (`·`, `←→`, `↵`) is inline like `wave_strip`, so this
//! module introduces **no new glyph constants** (AC7).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::domain::models::visual::symbols;

/// Pure decision returned by the adaptive spawn gate.
///
/// `Allow` ⇒ the requested breadth is at-or-below the learned threshold:
/// proceed silently. `Refuse` ⇒ above the threshold: surface the
/// review-burden card for human confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// `requested <= threshold` — no prompt; the static cap governs.
    Allow,
    /// `requested > threshold` — surface the gate for confirmation.
    Refuse,
}

/// Pure fold: `requested <= threshold → Allow`, else `Refuse`.
///
/// Zero I/O — the threshold is **injected** (DD5) so the boundary test can pin
/// it without touching config. Below ⇒ silent auto fan-out; above ⇒ the
/// review-burden card.
pub fn gate_decision(requested: usize, threshold: usize) -> GateDecision {
    if requested <= threshold {
        GateDecision::Allow
    } else {
        GateDecision::Refuse
    }
}

/// Snapshot the gate renders from. The event loop builds this from the
/// requested fan-out breadth plus the config-backed remembered threshold
/// (default `FORK_JOIN_SPAWN_CAP=8`); the widget touches no async state.
#[derive(Clone, Debug)]
pub struct SpawnGateSnapshot {
    /// The fan-out breadth the user/model requested.
    pub requested: usize,
    /// The learned (config-backed, remembered) threshold. At-or-below ⇒ silent;
    /// above ⇒ the gate surfaces.
    pub threshold: usize,
    /// User-adjusted N (the `←→` inline edit). When `Some`, the gate renders
    /// and decides on the adjusted breadth instead of the raw request.
    pub adjusted: Option<usize>,
}

impl SpawnGateSnapshot {
    /// The effective breadth the gate displays and decides on: the
    /// user-adjusted N when present, else the raw request.
    pub fn effective_n(&self) -> usize {
        self.adjusted.unwrap_or(self.requested)
    }
}

/// Render the `ExceptionalSpawnGate` as a single-line PermissionCard variant.
///
/// Format:
/// `spawn {N}? ⚠ {N} rows · ←→ adjust N · ↵ confirm · c cap at {threshold}`
/// where `N` is the effective breadth (adjusted when the user has nudged it,
/// else the raw request). Padded to `width` so the `┃ … ┃` border closes flush
/// (mirrors [`permission_prompt`](super::permission_prompt)'s card grammar).
pub fn render_spawn_gate(snap: &SpawnGateSnapshot, width: u16) -> Vec<Line<'static>> {
    let n = snap.effective_n();

    let border = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let warn = Style::default().fg(Color::Yellow);
    let muted = Style::default().fg(Color::DarkGray);
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::Gray);
    let sep = " \u{00B7} "; // · affordance separator

    // Content spans (no borders) — multi-span so the review-burden `⚠ N rows`
    // reads in warning yellow and affordances read muted/gray.
    let mut content: Vec<Span> = vec![
        Span::styled(format!("spawn {n}?"), accent),
        Span::raw(" "),
        Span::styled(format!("{} {n} rows", symbols::WARNING), warn),
        Span::styled(sep, muted),
        Span::styled("\u{2190}\u{2192} adjust N", body), // ←→ adjust N
        Span::styled(sep, muted),
        Span::styled("\u{21B5} confirm", body), // ↵ confirm
        Span::styled(sep, muted),
        Span::styled(format!("c cap at {}", snap.threshold), body),
    ];

    // Pad to `width` (minus the 4 chars of `┃ ` + ` ┃` border overhead) so the
    // closing border sits flush. The gate content is short by design; we pad
    // rather than truncate the keybinding hints.
    let content_str: String = content.iter().map(|s| s.content.as_ref()).collect();
    let content_w = content_str.width();
    let target_content = (width as usize).saturating_sub(4);
    if content_w < target_content {
        content.push(Span::raw(" ".repeat(target_content - content_w)));
    }

    let mut spans = vec![Span::styled("\u{2503} ", border)]; // ┃
    spans.append(&mut content);
    spans.push(Span::styled(" \u{2503}", border)); // ┃

    vec![Line::from(spans)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Join a line's spans back into a flat string for assertion.
    fn rendered(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn gate_decision_boundary_at_threshold_two() {
        // AC6 boundary test: threshold=2, exercise EXACTLY {1,2,3}.
        // 1 → Allow (below), 2 → Allow (at, `<=`), 3 → Refuse (above).
        assert_eq!(gate_decision(1, 2), GateDecision::Allow);
        assert_eq!(gate_decision(2, 2), GateDecision::Allow);
        assert_eq!(gate_decision(3, 2), GateDecision::Refuse);
    }

    #[test]
    fn below_threshold_is_allowed_silent() {
        // Below the threshold ⇒ Allow (no prompt; static cap governs silently).
        assert_eq!(gate_decision(0, 8), GateDecision::Allow);
        assert_eq!(gate_decision(7, 8), GateDecision::Allow);
        assert_eq!(gate_decision(8, 8), GateDecision::Allow);
    }

    #[test]
    fn above_threshold_is_refused() {
        // Above the threshold ⇒ Refuse (surface the review-burden card).
        assert_eq!(gate_decision(9, 8), GateDecision::Refuse);
        assert_eq!(gate_decision(20, 8), GateDecision::Refuse);
        assert_eq!(gate_decision(500, 8), GateDecision::Refuse);
    }

    #[test]
    fn gate_decision_is_pure_with_injectable_threshold() {
        // DD5: the same request against different thresholds flips the verdict
        // — the fold depends ONLY on its inputs (zero I/O, injectable seam).
        assert_eq!(gate_decision(20, 20), GateDecision::Allow);
        assert_eq!(gate_decision(20, 8), GateDecision::Refuse);
        assert_eq!(gate_decision(20, 50), GateDecision::Allow);
    }

    #[test]
    fn render_gate_shows_review_burden_format() {
        let snap = SpawnGateSnapshot {
            requested: 20,
            threshold: 8,
            adjusted: None,
        };
        let lines = render_spawn_gate(&snap, 80);
        assert_eq!(lines.len(), 1, "gate renders as a single line");
        let out = rendered(&lines[0]);
        assert!(out.contains("spawn 20?"), "lede shows requested N: {out}");
        assert!(out.contains("20 rows"), "review-burden count: {out}");
        assert!(out.contains("←→ adjust N"), "adjust affordance: {out}");
        assert!(out.contains("↵ confirm"), "confirm affordance: {out}");
        assert!(out.contains("c cap at 8"), "cap-at-threshold: {out}");
        assert!(
            out.contains(symbols::WARNING.to_string().as_str()),
            "warning glyph reused (no new constant): {out}"
        );
    }

    #[test]
    fn render_gate_shows_adjusted_n_when_present() {
        // ←→ nudges N: the gate renders the adjusted breadth, not the raw request.
        let snap = SpawnGateSnapshot {
            requested: 20,
            threshold: 8,
            adjusted: Some(12),
        };
        let out = rendered(&render_spawn_gate(&snap, 80)[0]);
        assert!(out.contains("spawn 12?"), "adjusted N shown: {out}");
        assert!(out.contains("12 rows"), "adjusted count: {out}");
        assert!(
            !out.contains("20"),
            "raw request hidden when adjusted: {out}"
        );
    }

    #[test]
    fn effective_n_prefers_adjusted() {
        assert_eq!(
            SpawnGateSnapshot {
                requested: 20,
                threshold: 8,
                adjusted: None
            }
            .effective_n(),
            20
        );
        assert_eq!(
            SpawnGateSnapshot {
                requested: 20,
                threshold: 8,
                adjusted: Some(5)
            }
            .effective_n(),
            5
        );
    }
}
