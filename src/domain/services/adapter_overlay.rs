//! Adapter overlay services — shared logic for the adapter status panel,
//! slash-command routing, and CLI flag processing (Story 8.5).
//!
//! Pure functions: no I/O, no async, no adapter/infrastructure imports.

use std::collections::BTreeMap;

use crate::domain::models::profile::{AdapterRef, PortDimension};

pub fn port_dimension_from_command_name(cmd_name: &str) -> Option<PortDimension> {
    match cmd_name {
        "persona" => Some(PortDimension::Persona),
        "memory" => Some(PortDimension::Memory),
        "session" => Some(PortDimension::Session),
        "tools" => Some(PortDimension::Tools),
        "channels" => Some(PortDimension::Channels),
        "scheduler" => Some(PortDimension::Scheduler),
        "context" => Some(PortDimension::Context),
        _ => None,
    }
}

pub fn port_label(port: PortDimension) -> &'static str {
    match port {
        PortDimension::Persona => "persona",
        PortDimension::Memory => "memory",
        PortDimension::Session => "session",
        PortDimension::Tools => "tools",
        PortDimension::Channels => "channels",
        PortDimension::Scheduler => "scheduler",
        PortDimension::Context => "context",
    }
}

pub fn active_adapter_for(
    port: PortDimension,
    adapter_name_from_core: &str,
    overrides: &BTreeMap<PortDimension, AdapterRef>,
) -> String {
    if let Some(r) = overrides.get(&port) {
        r.adapter.clone()
    } else {
        adapter_name_from_core.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_dimension_round_trip() {
        for port in &[
            PortDimension::Persona,
            PortDimension::Memory,
            PortDimension::Session,
            PortDimension::Tools,
            PortDimension::Channels,
            PortDimension::Scheduler,
            PortDimension::Context,
        ] {
            let label = port_label(*port);
            let parsed = port_dimension_from_command_name(label);
            assert_eq!(parsed, Some(*port), "round-trip failed for {:?}", port);
        }
    }

    #[test]
    fn test_unknown_command_name_returns_none() {
        assert_eq!(port_dimension_from_command_name("bogus"), None);
        assert_eq!(port_dimension_from_command_name(""), None);
    }

    #[test]
    fn test_active_adapter_for_prefers_override() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            PortDimension::Memory,
            AdapterRef {
                adapter: "daily-log".into(),
                _config: None,
            },
        );
        let got = active_adapter_for(PortDimension::Memory, "noop", &overrides);
        assert_eq!(got, "daily-log");
    }

    #[test]
    fn test_active_adapter_for_falls_back_to_core() {
        let overrides = BTreeMap::new();
        let got = active_adapter_for(PortDimension::Memory, "noop", &overrides);
        assert_eq!(got, "noop");
    }
}
