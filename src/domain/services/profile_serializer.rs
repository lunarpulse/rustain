//! Profile serializer — converts a fully-resolved profile into a self-contained
//! TOML string. Shared by `rustain profile show --toml` (no header) and
//! `rustain profile export` (with 4-line header comment).
//!
//! Story 8.6a (Decision Gate 1.7).

use crate::domain::models::{PortDimension, ProfileSource, ResolvedProfile};

/// Error type for profile serialization failures.
#[derive(Debug)]
pub enum ProfileSerializeError {
    /// Failed to encode overrides (figment → TOML conversion).
    OverrideEncode(String),
    /// Failed to emit TOML (toml::ser error).
    TomlEmit(toml::ser::Error),
}

impl std::fmt::Display for ProfileSerializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverrideEncode(msg) => write!(f, "Failed to encode overrides: {}", msg),
            Self::TomlEmit(e) => write!(f, "Failed to emit TOML: {}", e),
        }
    }
}

impl std::error::Error for ProfileSerializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TomlEmit(e) => Some(e),
            _ => None,
        }
    }
}

/// Canonical order of port dimensions for serialization.
const PORT_ORDER: &[(PortDimension, &str)] = &[
    (PortDimension::Persona, "persona"),
    (PortDimension::Memory, "memory"),
    (PortDimension::Session, "session"),
    (PortDimension::Tools, "tools"),
    (PortDimension::Channels, "channels"),
    (PortDimension::Scheduler, "scheduler"),
    (PortDimension::Context, "context"),
];

/// Serialize a resolved profile into a self-contained TOML string.
///
/// The output has NO `extends` line — the extends chain is already flattened
/// by `ProfileLoader::load`. All 7 port sections are emitted. The `[overrides]`
/// section is only emitted when non-empty.
///
/// When `with_header` is true, a 4-line comment header is prepended with
/// export metadata (source label + UTC timestamp).
pub fn to_flat_toml(
    resolved: &ResolvedProfile,
    with_header: bool,
    source: ProfileSource,
) -> Result<String, ProfileSerializeError> {
    let mut out = String::new();

    if with_header {
        let source_label = match source {
            ProfileSource::Builtin => "built-in",
            ProfileSource::User => "user",
            ProfileSource::Community => "community",
        };
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        out.push_str("# Exported by rustain profile export\n");
        out.push_str(&format!("# Source: {} ({})\n", resolved.name, source_label));
        out.push_str(&format!("# Exported at: {}\n", ts));
        out.push_str("# Note: extends chain flattened; this file is self-contained.\n");
    }

    // Build a toml::Table root
    let mut root = toml::Table::new();
    root.insert(
        "name".to_string(),
        toml::Value::String(resolved.name.clone()),
    );

    if resolved.preview {
        root.insert("preview".to_string(), toml::Value::Boolean(true));
    }

    // Serialize 7 port sections in canonical order
    for (dim, label) in PORT_ORDER {
        if let Some(adapter_ref) = resolved.selection.dimensions.get(dim) {
            let mut port_table = toml::Table::new();
            port_table.insert(
                "adapter".to_string(),
                toml::Value::String(adapter_ref.adapter.clone()),
            );
            root.insert(label.to_string(), toml::Value::Table(port_table));
        }
    }

    // Serialize [overrides] block ONLY if non-empty
    if let Some(ref overrides) = resolved.overrides {
        let override_toml = figment_to_toml(overrides)?;
        match override_toml {
            toml::Value::Table(table) if !table.is_empty() => {
                root.insert("overrides".to_string(), toml::Value::Table(table));
            }
            _ => { /* empty or non-table overrides — skip */ }
        }
    }

    let toml_str = toml::to_string(&root).map_err(ProfileSerializeError::TomlEmit)?;
    out.push_str(&toml_str);

    Ok(out)
}

/// Convert a `figment::value::Value` to a `toml::Value` via serde round-trip.
fn figment_to_toml(fv: &figment::value::Value) -> Result<toml::Value, ProfileSerializeError> {
    let json_val = serde_json::to_value(fv)
        .map_err(|e| ProfileSerializeError::OverrideEncode(format!("figment->json: {}", e)))?;
    let toml_val: toml::Value = toml::Value::try_from(json_val)
        .map_err(|e| ProfileSerializeError::OverrideEncode(format!("json->toml: {}", e)))?;
    Ok(toml_val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{AdapterRef, ProfileSelection};
    use std::collections::BTreeMap;

    fn make_resolved(name: &str, overrides: Option<figment::value::Value>) -> ResolvedProfile {
        let mut dimensions = BTreeMap::new();
        for (dim, _label) in PORT_ORDER {
            dimensions.insert(
                *dim,
                AdapterRef {
                    adapter: "test-adapter".to_string(),
                    _config: None,
                },
            );
        }
        ResolvedProfile {
            name: name.to_string(),
            selection: ProfileSelection { dimensions },
            overrides,
            preview: false,
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
        }
    }

    #[test]
    fn test_coding_round_trips_through_loader() {
        use crate::domain::services::adapter_catalog::AdapterCatalog;
        use crate::domain::services::profile_loader::ProfileLoader;
        use std::cell::RefCell;
        use std::collections::HashMap;

        struct TestSource {
            profiles: RefCell<HashMap<String, String>>,
        }
        impl crate::domain::services::profile_loader::ProfileSource for TestSource {
            fn get(&self, name: &str) -> Option<String> {
                self.profiles.borrow().get(name).cloned()
            }
        }

        let coding_toml = r#"name = "coding"
description = "Coding profile"
preview = false
[persona]
adapter = "coding"
[memory]
adapter = "project-scoped"
[session]
adapter = "workspace"
[tools]
adapter = "builtin-full"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;

        let mut m = HashMap::new();
        m.insert("coding".to_string(), coding_toml.to_string());
        let source = TestSource {
            profiles: RefCell::new(m),
        };
        let loader = ProfileLoader::new(&AdapterCatalog, &source);
        let resolved = loader.load("coding").unwrap();

        let flat = to_flat_toml(&resolved, false, ProfileSource::Builtin).unwrap();
        assert!(flat.contains("[persona]"));
        assert!(flat.contains("adapter = \"coding\""));
        // Round-trip: parse back to ProfileDefinition
        let reparse: crate::domain::models::ProfileDefinition = toml::from_str(&flat).unwrap();
        assert_eq!(reparse.name, "coding");
        assert!(reparse.persona.is_some());
        assert_eq!(reparse.persona.unwrap().adapter, "coding");
    }

    #[test]
    fn test_no_overrides_section_emitted_when_empty() {
        let resolved = make_resolved("test", None);
        let flat = to_flat_toml(&resolved, false, ProfileSource::User).unwrap();
        assert!(!flat.contains("[overrides]"));
    }

    #[test]
    fn test_overrides_section_emitted_with_default_plan_mode() {
        // Create a simple override using figment's serialize mechanism
        let mut overrides_toml = toml::Table::new();
        overrides_toml.insert("default_plan_mode".to_string(), toml::Value::Boolean(false));
        let override_val =
            figment::value::Value::serialize(toml::Value::Table(overrides_toml)).ok();

        let resolved = make_resolved("test", override_val);
        let flat = to_flat_toml(&resolved, false, ProfileSource::Builtin).unwrap();
        assert!(flat.contains("[overrides]"));
        // The TOML emitter serializes booleans as lowercase
        assert!(flat.contains("default_plan_mode"));
    }

    #[test]
    fn test_header_format_matches_spec() {
        let resolved = make_resolved("coding", None);
        let flat = to_flat_toml(&resolved, true, ProfileSource::Builtin).unwrap();
        assert!(flat.starts_with("# Exported by rustain profile export"));
        assert!(flat.contains("# Source: coding (built-in)"));
        assert!(flat.contains("# Exported at: "));
        assert!(flat.contains("# Note: extends chain flattened; this file is self-contained."));
    }

    #[test]
    fn test_preview_flag_preserved() {
        let mut resolved = make_resolved("test-prev", None);
        resolved.preview = true;
        let flat = to_flat_toml(&resolved, false, ProfileSource::User).unwrap();
        assert!(flat.contains("preview = true"));
    }

    #[test]
    fn test_to_flat_toml_contains_all_7_ports() {
        let resolved = make_resolved("full", None);
        let flat = to_flat_toml(&resolved, false, ProfileSource::Builtin).unwrap();
        for (_dim, label) in PORT_ORDER {
            assert!(
                flat.contains(&format!("[{}]", label)),
                "missing port section: [{}]",
                label
            );
        }
    }
}
