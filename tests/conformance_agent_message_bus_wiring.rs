#[test]
fn ac3_subagent_event_variant_is_consumed_by_tui_handler() {
    let events = include_str!("../src/domain/events.rs");
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let state = include_str!("../src/adapters/tui/state.rs");

    assert!(
        events.contains("Subagent(crate::domain::models::SubagentEnvelope)"),
        "AppEvent::Subagent variant must carry SubagentEnvelope"
    );
    assert!(
        state.contains("pub fn handle_subagent_envelope"),
        "TuiState must expose a sync handler for SubagentEnvelope"
    );
    assert!(
        event_loop.contains("AppEvent::Subagent(envelope)"),
        "event_loop must have an explicit AppEvent::Subagent arm"
    );
    assert!(
        event_loop.contains("state.handle_subagent_envelope(envelope)"),
        "AppEvent::Subagent arm must delegate to TuiState handler"
    );
}

#[test]
fn ac3_subagent_envelope_not_projected_as_ad_hoc_app_event_fields() {
    let events = include_str!("../src/domain/events.rs");
    assert_eq!(
        events.matches("SubagentEnvelope").count(),
        1,
        "SubagentEnvelope should enter AppEvent only through AppEvent::Subagent"
    );
    assert!(
        !events.contains("SubagentRegistered"),
        "R1 must not add broad subagent schema variants"
    );
    assert!(
        !events.contains("OwnershipChanged"),
        "R1 must not add ownership-change AppEvent variants"
    );
}
