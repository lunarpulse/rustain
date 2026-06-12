use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDimension {
    Persona,
    Memory,
    Session,
    Tools,
    Channels,
    Scheduler,
    Context,
    Skills,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileIdentityColor(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    Builtin,
    User,
    Community,
}

#[derive(Debug, Clone)]
pub struct ProfileDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub preview: bool,
    pub identity_color: ProfileIdentityColor,
    pub source: ProfileSource,
    pub source_origin: Option<String>,
    pub selection: ProfileSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileId(pub String);

impl ProfileId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct TransitionState {
    pub port_type: &'static str,
    pub adapter_id: String,
    pub version: u32,
    pub data: serde_json::Value,
}

impl TransitionState {
    pub fn empty(port_type: &'static str) -> Self {
        Self {
            port_type,
            adapter_id: String::new(),
            version: 0,
            data: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActiveProfileSnapshot {
    pub name: String,
    pub identity_color: ProfileIdentityColor,
    pub preview: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterRef {
    pub adapter: String,
    #[serde(default, rename = "config")]
    pub _config: Option<toml::Value>,
}

#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub name: String,
    pub selection: ProfileSelection,
    pub overrides: Option<figment::value::Value>,
    pub preview: bool,
    /// MCP servers parsed from workspace `.claude/mcp.json` + profile `[tools.config.mcp.*]`.
    /// Populated by `TomlProfileResolver` after the 7-layer figment merge (Story 9.1).
    pub mcp_servers: Vec<crate::domain::models::mcp_server_spec::McpServerSpec>,
    /// Whether composite tools adapter includes builtin tools (default true).
    /// Parsed from `[tools.config] include_builtin` in profile TOML (Story 9.1 AC-4).
    pub include_builtin_tools: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileSelection {
    pub dimensions: BTreeMap<PortDimension, AdapterRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub identity_color: Option<u8>,
    #[serde(default)]
    pub preview: bool,
    #[serde(default)]
    pub persona: Option<AdapterRef>,
    #[serde(default)]
    pub memory: Option<AdapterRef>,
    #[serde(default)]
    pub session: Option<AdapterRef>,
    #[serde(default)]
    pub tools: Option<AdapterRef>,
    #[serde(default)]
    pub channels: Option<AdapterRef>,
    #[serde(default)]
    pub scheduler: Option<AdapterRef>,
    #[serde(default)]
    pub context: Option<AdapterRef>,
    #[serde(default)]
    pub overrides: Option<toml::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_base_profile() {
        let toml_str = r#"
name = "base"
description = "Minimal agent"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;
        let def: ProfileDefinition = toml::from_str(toml_str).unwrap();
        assert_eq!(def.name, "base");
        assert_eq!(def.description.as_deref(), Some("Minimal agent"));
        assert!(def.extends.is_none());
        assert!(!def.preview);
        assert_eq!(def.persona.as_ref().unwrap().adapter, "minimal");
        assert_eq!(def.memory.as_ref().unwrap().adapter, "noop");
        assert_eq!(def.session.as_ref().unwrap().adapter, "basic");
        assert_eq!(def.tools.as_ref().unwrap().adapter, "builtin-only");
        assert_eq!(def.channels.as_ref().unwrap().adapter, "terminal");
        assert_eq!(def.scheduler.as_ref().unwrap().adapter, "none");
        assert_eq!(def.context.as_ref().unwrap().adapter, "default");
    }

    #[test]
    fn parse_profile_with_extends() {
        let toml_str = r#"
name = "coding"
extends = "base"
description = "Coding assistant"
[persona]
adapter = "coding"
[overrides]
default_plan_mode = false
"#;
        let def: ProfileDefinition = toml::from_str(toml_str).unwrap();
        assert_eq!(def.extends.as_deref(), Some("base"));
        assert!(def.overrides.is_some());
    }

    #[test]
    fn parse_preview_profile() {
        let toml_str = r#"
name = "test-preview"
preview = true
"#;
        let def: ProfileDefinition = toml::from_str(toml_str).unwrap();
        assert!(def.preview);
    }

    #[test]
    fn missing_dimensions_are_none() {
        let toml_str = r#"name = "minimal""#;
        let def: ProfileDefinition = toml::from_str(toml_str).unwrap();
        assert!(def.persona.is_none());
        assert!(def.memory.is_none());
        assert!(def.session.is_none());
        assert!(def.tools.is_none());
        assert!(def.channels.is_none());
        assert!(def.scheduler.is_none());
        assert!(def.context.is_none());
        assert!(def.overrides.is_none());
    }

    #[test]
    fn port_dimension_ordering() {
        let mut dims = vec![
            PortDimension::Scheduler,
            PortDimension::Persona,
            PortDimension::Context,
            PortDimension::Memory,
            PortDimension::Tools,
            PortDimension::Channels,
            PortDimension::Session,
            PortDimension::Skills,
        ];
        dims.sort();
        assert_eq!(
            dims,
            vec![
                PortDimension::Persona,
                PortDimension::Memory,
                PortDimension::Session,
                PortDimension::Tools,
                PortDimension::Channels,
                PortDimension::Scheduler,
                PortDimension::Context,
                PortDimension::Skills,
            ]
        );
    }

    #[test]
    fn adapter_ref_with_config() {
        let toml_str = r#"
name = "test"
[persona]
adapter = "custom"
[persona.config]
tone = "concise"
style = "prefer-existing"
"#;
        let def: ProfileDefinition = toml::from_str(toml_str).unwrap();
        let persona = def.persona.unwrap();
        assert_eq!(persona.adapter, "custom");
        assert!(persona._config.is_some());
    }

    #[test]
    fn unknown_fields_accepted() {
        let toml_str = r#"
name = "test"
future_field = "no problem"
[nested_unknown]
foo = "bar"
[persona]
adapter = "minimal"
"#;
        let def: ProfileDefinition = toml::from_str(toml_str).unwrap();
        assert_eq!(def.name, "test");
        assert_eq!(def.persona.as_ref().unwrap().adapter, "minimal");
    }
}
