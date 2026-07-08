//! Shared status→glyph palette — the single source of truth (Story 14.3, AC6).
//!
//! Reconciles the two PRE-existing glyph-producing sites
//! (`agent_panel::subagent_icon_for` + `agent_panel::ownership_glyph`) with the
//! UX spec legend (`ux-design-specification.md :3323` ↔ `:2181`). ONE module,
//! owned by the result-contract layer, prevents the two-glyph-set drift the
//! orchestration UI would otherwise inherit.
//!
//! Migrated to the UX spec's glyph families (ux-design-specification.md :2995-2997):
//! ▶ Running, ○ Created, ‖ Waiting, z Suspended, ✓ Completed, ✗ Failed, ⊘ Cancelled.
//! Different metaphor families prevent the ◔/◐ monochrome collision.
//!
//! ## Glyph carries meaning, color decorates
//!
//! Per the UX invariant, every glyph is monochrome-safe (color is added by the
//! caller from the `Theme`; the glyph alone conveys status).

use crate::domain::models::node_state::NodeState;
use crate::domain::models::orchestration::SpokeResult;
use crate::domain::models::subagent_view::OwnershipKind;

/// Status glyph for a [`NodeState`]. Monochrome-safe; the caller adds color.
pub fn node_state_glyph(state: NodeState) -> &'static str {
    match state {
        NodeState::Running => "\u{25B6}",   // ▶
        NodeState::Created => "\u{25CB}",   // ○
        NodeState::Waiting => "\u{2016}",   // ‖
        NodeState::Suspended => "z",        // z (zzz — deliberately paused)
        NodeState::Completed => "\u{2713}", // ✓
        NodeState::Failed => "\u{2717}",    // ✗
        NodeState::Cancelled => "\u{2298}", // ⊘
    }
}

/// Ownership glyph. Peer is reserved (R1 does not render it).
pub fn ownership_glyph(kind: OwnershipKind) -> &'static str {
    match kind {
        OwnershipKind::Self_(_) => "\u{2605}", // ★ Self
        OwnershipKind::Owned => "\u{2666}",    // ♦ Owned
        OwnershipKind::Peer => "\u{25C7}",     // ◇ Peer (RESERVED — not rendered R1)
    }
}

/// Scratch-dir isolation indicator. Route all `⊙ iso` rendering through this
/// SSOT so 14.5 does not create a second orchestration glyph vocabulary.
pub fn isolation_glyph() -> &'static str {
    "\u{2299} iso"
}

/// Spoke-result glyph for the WaveStrip / SynthesisBlock coverage line.
pub fn spoke_result_glyph(result: &SpokeResult) -> &'static str {
    match result {
        SpokeResult::Completed { .. } => "\u{2713}", // ✓
        SpokeResult::Failed { .. } => "\u{2717}",    // ✗
        SpokeResult::Cancelled => "\u{2298}",        // ⊘
        SpokeResult::Empty => "\u{2205}",            // ∅
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_state_has_a_distinct_glyph() {
        let mut seen = std::collections::HashSet::new();
        for s in NodeState::ALL {
            let g = node_state_glyph(*s);
            assert!(!g.is_empty(), "{s:?} has no glyph");
            seen.insert(g);
        }
        assert_eq!(seen.len(), NodeState::ALL.len());
    }

    #[test]
    fn ownership_kinds_are_distinct() {
        assert_ne!(
            ownership_glyph(OwnershipKind::Owned),
            ownership_glyph(OwnershipKind::Peer)
        );
        assert_ne!(
            ownership_glyph(OwnershipKind::self_root()),
            ownership_glyph(OwnershipKind::Owned)
        );
    }

    #[test]
    fn isolation_indicator_uses_ssot_copy() {
        assert_eq!(isolation_glyph(), "\u{2299} iso");
    }

    #[test]
    fn spoke_result_glyphs_partition_the_outcomes() {
        assert_eq!(
            spoke_result_glyph(&SpokeResult::Completed { summary: "".into() }),
            "\u{2713}"
        );
        assert_eq!(
            spoke_result_glyph(&SpokeResult::Failed { reason: "".into() }),
            "\u{2717}"
        );
        assert_eq!(spoke_result_glyph(&SpokeResult::Cancelled), "\u{2298}");
        assert_eq!(spoke_result_glyph(&SpokeResult::Empty), "\u{2205}");
    }
}
