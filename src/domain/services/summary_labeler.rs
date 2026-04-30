//! Heuristic semantic labeler for collapsed-turn one-liners.
//!
//! Produces a [`SummaryLabel`] with two tiers:
//! - **Tier-1** (cheap, always available): "N tools" + optional elapsed time.
//! - **Tier-2** (one keystroke away via `zs` in S16.6): path-prefix clustered
//!   summary like "3 reads in src/auth/, 1 grep".
//!
//! # Stub status
//!
//! This story (S16.4) ships a degenerate **Tier-2 == Tier-1** implementation so
//! the collapsed-turn render and the `zs` keybinding can compile and land before
//! the full clusterer is ready. Story 16.4.5 replaces the body of
//! [`compute_summary_label`] with the full path-prefix clusterer; the API
//! (`SummaryLabel`, `compute_summary_label`) and struct shape are stable.
//!
//! The user-facing `zs` keybinding (registered in S16.6) is gated behind
//! `#[cfg(feature = "tier2")]` until S16.4.5 lands, so users cannot trigger
//! the visual no-op during the stub window. See the S16.4.5 spec at
//! `_bmad-output/planning-artifacts/epics.md:5116-5154`.
//!
//! # Carve-out
//!
//! The `compute_summary_label` API is **not** feature-gated — only the keybinding
//! is. This keeps `chat_pane`'s call path locked and makes S16.4.5's swap a
//! single-file diff.

use crate::domain::models::turn::{Turn, TurnPart};

/// Tier-1 and Tier-2 summary strings for a completed turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryLabel {
    /// Cheap-form summary (e.g. "3 tools, 12.5s").
    pub tier1: String,
    /// Semantic-form summary (e.g. "3 reads in src/auth/, 1 grep").
    /// Currently identical to `tier1` — S16.4.5 implements clustering.
    pub tier2: String,
}

/// Compute the collapsed-summary label for a completed turn.
///
/// `elapsed_ms`: wall-clock elapsed for the turn as a whole
/// (`Some(ms)` if a wall-anchor is available, `None` otherwise — sentinel 0
/// propagates through as `None` per P0-8 decision).
pub fn compute_summary_label(turn: &Turn, elapsed_ms: Option<i64>) -> SummaryLabel {
    let n = turn.parts.iter().filter(|p| matches!(p, TurnPart::ToolInvocation { .. })).count();
    let elapsed_suffix = match elapsed_ms {
        Some(ms) if ms > 0 => format!(", {:.1}s", ms as f64 / 1000.0),
        _ => String::new(),
    };
    let tier1 = format!("{} tool{}{}", n, if n == 1 { "" } else { "s" }, elapsed_suffix);
    let tier2 = tier1.clone(); // stub: S16.4.5 implements clustering
    SummaryLabel { tier1, tier2 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::turn::{PartId, TurnPart};

    fn make_turn(n_tools: usize) -> Turn {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for i in 0..(n_tools.max(1)) {
            turn.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: format!("tool_{}", i),
                args: serde_json::json!({}),
                status: crate::domain::models::turn::InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            });
        }
        turn
    }

    #[test]
    fn stub_tier1_format_with_no_tools() {
        let turn = Turn::new("claude".into(), 1_700_000_000_000);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier1, "0 tools");
        assert_eq!(label.tier2, "0 tools");
    }

    #[test]
    fn stub_tier1_pluralizes_correctly() {
        assert_eq!(compute_summary_label(&make_turn(1), None).tier1, "1 tool");
        assert_eq!(compute_summary_label(&make_turn(2), None).tier1, "2 tools");
        assert_eq!(compute_summary_label(&make_turn(5), None).tier1, "5 tools");
    }

    #[test]
    fn stub_tier2_equals_tier1() {
        let label = compute_summary_label(&make_turn(3), Some(12_500));
        assert_eq!(label.tier1, label.tier2);
    }

    #[test]
    fn stub_elapsed_suffix_appears_when_provided() {
        let label = compute_summary_label(&make_turn(2), Some(12_500));
        assert_eq!(label.tier1, "2 tools, 12.5s");
    }

    #[test]
    fn stub_elapsed_suffix_omitted_when_zero_or_none() {
        assert_eq!(
            compute_summary_label(&make_turn(1), Some(0)).tier1,
            "1 tool"
        );
        assert_eq!(
            compute_summary_label(&make_turn(1), None).tier1,
            "1 tool"
        );
    }
}
