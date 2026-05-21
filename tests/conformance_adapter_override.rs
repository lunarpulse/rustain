//! Conformance ratchets for adapter override functionality — Story 8.5 AC-12.
//!
//! Mirrors Story 8.4's `conformance_profile_switch.rs` pattern:
//! domain isolation include_str! checks + round-trip totality.

use rustain::domain::models::{HealthLevel, HealthSummary, PortDimension};
use rustain::domain::services::adapter_overlay;

#[test]
fn test_port_dimension_from_command_name_total() {
    // Every PortDimension variant maps back to itself via round-trip
    for (port, expected_label) in &[
        (PortDimension::Persona, "persona"),
        (PortDimension::Memory, "memory"),
        (PortDimension::Session, "session"),
        (PortDimension::Tools, "tools"),
        (PortDimension::Channels, "channels"),
        (PortDimension::Scheduler, "scheduler"),
        (PortDimension::Context, "context"),
    ] {
        let label = adapter_overlay::port_label(*port);
        assert_eq!(label, *expected_label, "port_label mismatch for {:?}", port);
        let parsed = adapter_overlay::port_dimension_from_command_name(label);
        assert_eq!(parsed, Some(*port), "round-trip failed for {:?}", port);
    }
    // Unknown name returns None
    assert_eq!(
        adapter_overlay::port_dimension_from_command_name("bogus"),
        None
    );
    assert_eq!(adapter_overlay::port_dimension_from_command_name(""), None);
}

#[test]
fn test_session_overrides_default_empty() {
    use rustain::adapters::tui::state::TuiState;
    let ts = TuiState::new(120, 40);
    assert!(ts.session_overrides.is_empty());
}

#[test]
fn test_health_summary_constructors() {
    let h = HealthSummary::healthy("entries: 42");
    assert_eq!(h.level, HealthLevel::Healthy);
    assert!(h.suggested_action.is_none());

    let d = HealthSummary::degraded("write failed", "check permissions");
    assert_eq!(d.level, HealthLevel::Degraded);
    assert_eq!(d.suggested_action, Some("check permissions"));

    let e = HealthSummary::error("connection lost", "verify network");
    assert_eq!(e.level, HealthLevel::Error);
    assert_eq!(e.suggested_action, Some("verify network"));

    let u = HealthSummary::unknown();
    assert_eq!(u.level, HealthLevel::Unknown);
    assert_eq!(u.metric, "n/a");
    assert!(u.suggested_action.is_none());
}

#[test]
fn test_active_adapter_for_returns_override_when_present() {
    use rustain::domain::models::AdapterRef;
    use std::collections::BTreeMap;

    let mut overrides = BTreeMap::new();
    overrides.insert(
        PortDimension::Memory,
        AdapterRef {
            adapter: "daily-log".to_string(),
            _config: None,
        },
    );
    let got = adapter_overlay::active_adapter_for(PortDimension::Memory, "noop", &overrides);
    assert_eq!(got, "daily-log");

    // Without override, returns the core name
    let got = adapter_overlay::active_adapter_for(PortDimension::Memory, "noop", &BTreeMap::new());
    assert_eq!(got, "noop");
}

#[test]
fn test_adapter_overlay_domain_isolation() {
    // The adapter_overlay services module MUST NOT import from adapters/ or infrastructure/
    let overlay_src = include_str!("../src/domain/services/adapter_overlay.rs");
    assert!(
        !overlay_src.contains("use crate::adapters"),
        "adapter_overlay.rs must not import from adapters/"
    );
    assert!(
        !overlay_src.contains("use crate::infrastructure"),
        "adapter_overlay.rs must not import from infrastructure/"
    );

    let health_src = include_str!("../src/domain/models/adapter_health.rs");
    assert!(
        !health_src.contains("use crate::adapters"),
        "adapter_health.rs must not import from adapters/"
    );
    assert!(
        !health_src.contains("use crate::infrastructure"),
        "adapter_health.rs must not import from infrastructure/"
    );
}
