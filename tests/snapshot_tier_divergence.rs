//! Locks the Tier-1 vs Tier-2 visual divergence — the durable differentiator
//! vs codex/gemini-cli/opencode (UX-DR-COLLAPSED-TIER, ADR-16-01 §Consequences).
//! Re-record only when ADR-16-01 §Q3 LLM-polish lands.

#[path = "common/render_helpers.rs"]
mod common;

use common::*;
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    InvocationStatus, MessageRole, StopReason, SummaryTier, ViewState,
};

#[test]
fn tier1_and_tier2_render_different_collapsed_lines() {
    let clock = MockClock::at_wall_ms(1_700_000_000_000);

    // Build a turn: 1 prose + 3 Read invocations with shared path prefix
    let turn = make_turn(
        "dt",
        vec![
            prose("I need to read the auth files."),
            tool_with_path("Read", "src/auth/login.rs", InvocationStatus::Success),
            tool_with_path("Read", "src/auth/jwt.rs", InvocationStatus::Success),
            tool_with_path("Read", "src/auth/session.rs", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );

    let msg = make_msg("dt", MessageRole::Assistant);
    let conversation = make_conversation(vec![msg], vec![turn.clone()]);

    // Tier-1 render
    let mut vs_tier1 = ViewState::default();
    vs_tier1.collapsed.insert(turn.id.clone(), true);
    vs_tier1.summary_tier = SummaryTier::Tier1;
    let tier1_str = render_to_string(&conversation, None, &vs_tier1, &clock, 80, 20, None);
    insta::assert_snapshot!("tier1", tier1_str);

    // Tier-2 render
    let mut vs_tier2 = ViewState::default();
    vs_tier2.collapsed.insert(turn.id.clone(), true);
    vs_tier2.summary_tier = SummaryTier::Tier2;
    let tier2_str = render_to_string(&conversation, None, &vs_tier2, &clock, 80, 20, None);
    insta::assert_snapshot!("tier2", tier2_str);

    // Durable differentiator lock: the two must not be equal
    assert_ne!(
        tier1_str, tier2_str,
        "Tier-1 and Tier-2 collapsed lines must differ — durable differentiator lock"
    );

    // Sanity check directionality
    assert!(
        tier1_str.contains("3 tools"),
        "Tier-1 must contain '3 tools': {tier1_str}"
    );
    assert!(
        tier2_str.contains("3 reads"),
        "Tier-2 must contain '3 reads': {tier2_str}"
    );
}
