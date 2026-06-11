//! AdapterCatalog — compile-time registry of known adapter names per port dimension.
//! Story 8.2 AC-8, AC-10.
//!
//! Populated with the adapter names referenced by the 3 built-in profiles AND
//! `noop` for each port. Feature-gated adapters carry `feature_gate` and
//! `fallback` fields for graceful degradation of preview profiles (AC-10).

use crate::domain::models::PortDimension;

#[derive(Debug, Clone)]
pub struct AdapterDescriptor {
    pub name: &'static str,
    pub feature_gate: Option<&'static str>,
    pub fallback: Option<&'static str>,
}

impl AdapterDescriptor {
    pub const fn new(
        name: &'static str,
        feature_gate: Option<&'static str>,
        fallback: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            feature_gate,
            fallback,
        }
    }
}

/// Compile-time catalog of known adapters.
pub struct AdapterCatalog;

impl AdapterCatalog {
    /// Returns the list of known adapter names for a given port dimension.
    pub fn known_for(port: PortDimension) -> Vec<&'static str> {
        catalog_for(port).iter().map(|d| d.name).collect()
    }

    /// Look up an adapter descriptor by port and name.
    pub fn lookup(port: PortDimension, name: &str) -> Option<&'static AdapterDescriptor> {
        catalog_for(port).iter().find(|d| d.name == name).copied()
    }

    /// Returns the fallback adapter name for a feature-gated adapter, or None.
    pub fn fallback_for(port: PortDimension, name: &str) -> Option<&'static str> {
        let desc = Self::lookup(port, name)?;
        if let Some(feature) = desc.feature_gate {
            if !Self::is_feature_compiled(feature) {
                return desc.fallback;
            }
        }
        None
    }

    /// Check if a cargo feature is compiled into the current binary.
    #[allow(unexpected_cfgs)]
    pub fn is_feature_compiled(feature: &str) -> bool {
        match feature {
            "telegram" => cfg!(feature = "telegram"),
            "cron" => cfg!(feature = "cron"),
            "gmail" => cfg!(feature = "gmail"),
            "mcp" => cfg!(feature = "mcp"),
            _ => false,
        }
    }
}

/// Returns the static constant slice of adapter descriptors for a port.
fn catalog_for(port: PortDimension) -> &'static [&'static AdapterDescriptor] {
    use PortDimension::*;
    match port {
        Persona => PERSONA_ADAPTERS,
        Memory => MEMORY_ADAPTERS,
        Session => SESSION_ADAPTERS,
        Tools => TOOLS_ADAPTERS,
        Channels => CHANNELS_ADAPTERS,
        Scheduler => SCHEDULER_ADAPTERS,
        Context => CONTEXT_ADAPTERS,
        Skills => &[],
    }
}

// ── Per-port adapter registry ──

static PERSONA_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "minimal",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "coding",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "personal-assistant",
        feature_gate: None,
        fallback: None,
    },
];

static MEMORY_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "noop",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "project-scoped",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "daily-log",
        feature_gate: None,
        fallback: None,
    },
];

static SESSION_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "basic",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "workspace",
        feature_gate: None,
        fallback: None,
    },
];

static TOOLS_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "builtin-only",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "builtin-full",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "composite",
        feature_gate: Some("mcp"),
        fallback: Some("builtin-full"),
    },
];

static CHANNELS_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "terminal",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "telegram",
        feature_gate: Some("telegram"),
        fallback: Some("terminal"),
    },
];

static SCHEDULER_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "none",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "cron",
        feature_gate: Some("cron"),
        fallback: Some("none"),
    },
];

static CONTEXT_ADAPTERS: &[&AdapterDescriptor] = &[
    &AdapterDescriptor {
        name: "default",
        feature_gate: None,
        fallback: None,
    },
    &AdapterDescriptor {
        name: "daily",
        feature_gate: None,
        fallback: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_for_memory_returns_expected() {
        let names = AdapterCatalog::known_for(PortDimension::Memory);
        assert_eq!(names, vec!["noop", "project-scoped", "daily-log"]);
    }

    #[test]
    fn lookup_telegram_has_feature_gate() {
        let desc = AdapterCatalog::lookup(PortDimension::Channels, "telegram").unwrap();
        assert_eq!(desc.feature_gate, Some("telegram"));
    }

    #[test]
    fn lookup_terminal_has_no_feature_gate() {
        let desc = AdapterCatalog::lookup(PortDimension::Channels, "terminal").unwrap();
        assert!(desc.feature_gate.is_none());
    }

    #[test]
    fn fallback_for_telegram_returns_terminal_when_feature_off() {
        // Default build has no telegram feature
        if !cfg!(feature = "telegram") {
            let fallback = AdapterCatalog::fallback_for(PortDimension::Channels, "telegram");
            assert_eq!(fallback, Some("terminal"));
        }
    }

    #[test]
    fn lookup_unknown_adapter_returns_none() {
        let desc = AdapterCatalog::lookup(PortDimension::Persona, "nonexistent");
        assert!(desc.is_none());
    }

    #[test]
    fn known_for_channels_returns_terminal_and_telegram() {
        let names = AdapterCatalog::known_for(PortDimension::Channels);
        assert!(names.contains(&"terminal"));
        assert!(names.contains(&"telegram"));
    }

    #[test]
    fn known_for_scheduler_returns_none_and_cron() {
        let names = AdapterCatalog::known_for(PortDimension::Scheduler);
        assert!(names.contains(&"none"));
        assert!(names.contains(&"cron"));
    }
}
