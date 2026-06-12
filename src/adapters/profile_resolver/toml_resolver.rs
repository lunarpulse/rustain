//! TomlProfileResolver — adapter that loads profiles from
//! `~/.config/rustain/profiles/{name}.toml` (custom) or the embedded
//! built-in catalog (fallback).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::adapters::profile_resolver::embedded::{EmbeddedProfileSource, embedded_names};
use crate::domain::errors::ProfileError;
use crate::domain::models::{
    McpServerSpec, ProfileDescriptor, ProfileIdentityColor, ProfileSelection, ProfileSource,
    ResolvedProfile,
};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::identity_color;
use crate::domain::services::profile_loader::{
    ProfileLoader, ProfileSource as LoaderProfileSource,
};

pub struct TomlProfileResolver {
    resolved: ResolvedProfile,
    config_dir: PathBuf,
    preview_warning_emitted: OnceLock<()>,
}

pub struct FileSystemProfileSource {
    config_dir: PathBuf,
    embedded: EmbeddedProfileSource,
}

impl FileSystemProfileSource {
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            embedded: EmbeddedProfileSource,
        }
    }
}

impl LoaderProfileSource for FileSystemProfileSource {
    fn get(&self, name: &str) -> Option<String> {
        if name.contains(['/', '\\']) {
            return None;
        }

        const MAX_PROFILE_SIZE: usize = 1024 * 1024;

        let user_path = self.config_dir.join(format!("{name}.toml"));
        if user_path.exists() {
            match std::fs::read_to_string(&user_path) {
                Ok(content) => {
                    if content.len() > MAX_PROFILE_SIZE {
                        tracing::warn!(
                            "Profile at {:?} exceeds 1 MB limit ({} bytes), falling back to embedded",
                            user_path,
                            content.len()
                        );
                    } else {
                        return Some(content);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to read profile at {:?}: {}. Falling back to embedded catalog.",
                        user_path,
                        e
                    );
                }
            }
        }

        // 2. Community dir fallback (Story 8.6b)
        let community_path = self
            .config_dir
            .join("community")
            .join(format!("{name}.toml"));
        if community_path.exists() {
            match std::fs::read_to_string(&community_path) {
                Ok(content) => {
                    if content.len() > MAX_PROFILE_SIZE {
                        tracing::warn!(
                            "Community profile at {:?} exceeds 1 MB limit ({} bytes), falling back to embedded",
                            community_path,
                            content.len()
                        );
                    } else {
                        return Some(content);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to read community profile at {:?}: {}. Falling back to embedded catalog.",
                        community_path,
                        e
                    );
                }
            }
        }

        self.embedded.get(name)
    }
}

impl TomlProfileResolver {
    pub fn new(active_name: &str, config_dir: PathBuf) -> Result<Self, ProfileError> {
        let source = FileSystemProfileSource::new(config_dir.clone());
        let catalog = AdapterCatalog;
        let loader = ProfileLoader::new(&catalog, &source);
        let mut resolved = loader.load(active_name)?;

        // Story 9.1: Parse MCP server configs from workspace + profile
        #[cfg(feature = "mcp")]
        {
            let workspace_specs =
                crate::adapters::mcp::workspace_config::parse_workspace_mcp_config(
                    &std::env::current_dir()
                        .unwrap_or_default()
                        .join(".claude")
                        .join("mcp.json"),
                )
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to parse workspace MCP config: {e}");
                    Vec::new()
                });

            let profile_specs = crate::adapters::mcp::profile_config::extract_profile_mcp_servers(
                resolved
                    .selection
                    .dimensions
                    .get(&crate::domain::models::PortDimension::Tools)
                    .and_then(|r| r._config.as_ref()),
                active_name,
            );

            resolved.mcp_servers =
                crate::adapters::mcp::merge_mcp_specs(workspace_specs, profile_specs);

            crate::adapters::mcp::emit_transport_warnings(&resolved.mcp_servers);

            resolved.include_builtin_tools = resolved
                .selection
                .dimensions
                .get(&crate::domain::models::PortDimension::Tools)
                .and_then(|r| r._config.as_ref())
                .and_then(|c| c.get("include_builtin"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            // Auto-rewrite "composite" to "builtin-full" if no MCP servers configured
            if let Some(tools_ref) = resolved
                .selection
                .dimensions
                .get_mut(&crate::domain::models::PortDimension::Tools)
            {
                if tools_ref.adapter == "composite" && resolved.mcp_servers.is_empty() {
                    tracing::warn!(
                        "Profile '{}' selects composite tools adapter but defines no MCP servers and workspace has no .claude/mcp.json. Falling back to 'builtin-full'.",
                        active_name
                    );
                    tools_ref.adapter = "builtin-full".to_string();
                }
            }
        }

        Ok(Self {
            resolved,
            config_dir,
            preview_warning_emitted: OnceLock::new(),
        })
    }

    /// Returns true if the preview-warning notice should be emitted (once).
    pub fn take_preview_warning(&self) -> Option<&str> {
        if self.resolved.preview && self.preview_warning_emitted.set(()).is_ok() {
            Some(self.resolved.name.as_str())
        } else {
            None
        }
    }
}

impl ProfileResolver for TomlProfileResolver {
    fn resolve_active(&self) -> Option<ResolvedProfile> {
        Some(self.resolved.clone())
    }

    fn list_profiles(&self) -> Vec<ProfileDescriptor> {
        let mut profiles: Vec<ProfileDescriptor> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. Scan user profiles directory
        if let Ok(entries) = std::fs::read_dir(&self.config_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "toml") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        if name.contains(['/', '\\']) {
                            continue;
                        }
                        let content = match std::fs::read_to_string(&path) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        if let Ok(def) =
                            toml::from_str::<crate::domain::models::ProfileDefinition>(&content)
                        {
                            if seen.insert(def.name.clone()) {
                                profiles.push(build_descriptor(&def, ProfileSource::User));
                            }
                        }
                    }
                }
            }
        }

        // 2. Scan community profiles directory (Story 8.6b)
        let community_dir = self.config_dir.join("community");
        if let Ok(entries) = std::fs::read_dir(&community_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "toml") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        if name.contains(['/', '\\']) {
                            continue;
                        }
                        let content = match std::fs::read_to_string(&path) {
                            Ok(c) => {
                                if c.len() > 1024 * 1024 {
                                    tracing::warn!(
                                        "Community profile at {:?} exceeds 1 MB limit ({} bytes), skipping",
                                        path,
                                        c.len()
                                    );
                                    continue;
                                }
                                c
                            }
                            Err(_) => continue,
                        };
                        if let Ok(def) =
                            toml::from_str::<crate::domain::models::ProfileDefinition>(&content)
                        {
                            if seen.insert(def.name.clone()) {
                                let mut desc = build_descriptor(&def, ProfileSource::Community);
                                desc.source_origin =
                                    crate::infrastructure::profile_install::read_source_sidecar(
                                        &path,
                                    );
                                profiles.push(desc);
                            }
                        }
                    }
                }
            }
        }

        // 3. Enumerate embedded profiles (only if not already seen)
        for &name in embedded_names() {
            if !seen.contains(name) {
                let source = EmbeddedProfileSource;
                if let Some(content) = source.get(name) {
                    if let Ok(def) =
                        toml::from_str::<crate::domain::models::ProfileDefinition>(&content)
                    {
                        seen.insert(def.name.clone());
                        profiles.push(build_descriptor(&def, ProfileSource::Builtin));
                    }
                }
            }
        }

        // Sort: Builtin first, then User, then Community; alphabetical within source
        profiles.sort_by(|a, b| {
            fn source_ord(s: ProfileSource) -> u8 {
                match s {
                    ProfileSource::Builtin => 0,
                    ProfileSource::User => 1,
                    ProfileSource::Community => 2,
                }
            }
            source_ord(a.source)
                .cmp(&source_ord(b.source))
                .then_with(|| a.name.cmp(&b.name))
        });

        profiles
    }
    // resolve_active_profile_defaults uses the trait default
}

fn build_descriptor(
    def: &crate::domain::models::ProfileDefinition,
    source: ProfileSource,
) -> ProfileDescriptor {
    use crate::domain::models::{AdapterRef, PortDimension};
    let identity_color = identity_color::derive_identity_color(&def.name, def.identity_color);
    let mut dimensions = std::collections::BTreeMap::new();
    if let Some(ref r) = def.persona {
        dimensions.insert(
            PortDimension::Persona,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    if let Some(ref r) = def.memory {
        dimensions.insert(
            PortDimension::Memory,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    if let Some(ref r) = def.session {
        dimensions.insert(
            PortDimension::Session,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    if let Some(ref r) = def.tools {
        dimensions.insert(
            PortDimension::Tools,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    if let Some(ref r) = def.channels {
        dimensions.insert(
            PortDimension::Channels,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    if let Some(ref r) = def.scheduler {
        dimensions.insert(
            PortDimension::Scheduler,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    if let Some(ref r) = def.context {
        dimensions.insert(
            PortDimension::Context,
            AdapterRef {
                adapter: r.adapter.clone(),
                _config: r._config.clone(),
            },
        );
    }
    let selection = ProfileSelection { dimensions };
    ProfileDescriptor {
        name: def.name.clone(),
        description: def.description.clone(),
        preview: def.preview,
        identity_color,
        source,
        source_origin: None,
        selection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::PortDimension;
    use std::collections::BTreeMap;

    fn make_tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    #[test]
    fn new_coding_loads_from_embedded() {
        let tmpdir = make_tempdir();
        let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
        let profile = resolver.resolve_active().unwrap();
        assert_eq!(profile.name, "coding");
        let dims = &profile.selection.dimensions;
        assert_eq!(dims[&PortDimension::Persona].adapter, "coding");
        assert_eq!(dims[&PortDimension::Memory].adapter, "project-scoped");
    }

    #[test]
    fn personal_assistant_preview_warning_once() {
        let tmpdir = make_tempdir();
        let resolver =
            TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();
        assert!(resolver.take_preview_warning().is_some());
        assert!(resolver.take_preview_warning().is_none());
    }

    #[test]
    fn nonexistent_profile_returns_error() {
        let tmpdir = make_tempdir();
        let result = TomlProfileResolver::new("nonexistent", tmpdir.path().to_path_buf());
        assert!(result.is_err());
    }

    #[test]
    fn custom_profile_overrides_embedded() {
        let tmpdir = make_tempdir();
        let custom_coding = r#"
name = "custom-coding"
extends = "base"
[persona]
adapter = "personal-assistant"
[memory]
adapter = "daily-log"
[session]
adapter = "workspace"
[tools]
adapter = "builtin-full"
"#;
        std::fs::write(tmpdir.path().join("custom-coding.toml"), custom_coding).unwrap();
        let resolver =
            TomlProfileResolver::new("custom-coding", tmpdir.path().to_path_buf()).unwrap();
        let profile = resolver.resolve_active().unwrap();
        assert_eq!(
            profile.selection.dimensions[&PortDimension::Persona].adapter,
            "personal-assistant"
        );
        assert_eq!(
            profile.selection.dimensions[&PortDimension::Memory].adapter,
            "daily-log"
        );
    }

    #[test]
    fn path_traversal_rejected() {
        let tmpdir = make_tempdir();
        let result = TomlProfileResolver::new("../../etc/passwd", tmpdir.path().to_path_buf());
        assert!(result.is_err());
    }

    #[test]
    fn empty_name_rejected() {
        let tmpdir = make_tempdir();
        let result = TomlProfileResolver::new("", tmpdir.path().to_path_buf());
        assert!(result.is_err());
    }

    #[test]
    fn community_dir_lookup_works() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        let toml_content = r#"
name = "community-foo"
extends = "base"
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
        std::fs::write(community_dir.join("foo.toml"), toml_content).unwrap();
        let source = FileSystemProfileSource::new(tmpdir.path().to_path_buf());
        assert!(source.get("foo").is_some());
    }

    #[test]
    fn user_dir_shadows_community() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        let user_toml = r#"
name = "shadow"
extends = "base"
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
        std::fs::write(tmpdir.path().join("shadow.toml"), user_toml).unwrap();
        let community_toml = r#"name = "shadow-community"
[persona]
adapter = "minimal"
"#;
        std::fs::write(community_dir.join("shadow.toml"), community_toml).unwrap();
        let source = FileSystemProfileSource::new(tmpdir.path().to_path_buf());
        let got = source.get("shadow").unwrap();
        assert!(got.contains("extends = \"base\""));
    }

    #[test]
    fn community_shadows_embedded_base() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        let base_toml = r#"
name = "community-base"
extends = "base"
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
        std::fs::write(community_dir.join("mybase.toml"), base_toml).unwrap();
        let source = FileSystemProfileSource::new(tmpdir.path().to_path_buf());
        // community "mybase" should resolve (embedded has "base" but not "mybase")
        let got = source.get("mybase");
        assert!(got.is_some());
        assert!(got.unwrap().contains("community-base"));
    }

    #[test]
    fn community_profile_appears_in_list_with_source_community() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        let toml_content = r#"
name = "list-test"
[persona]
adapter = "minimal"
"#;
        std::fs::write(community_dir.join("list-test.toml"), toml_content).unwrap();
        // Must load a valid profile to construct TomlProfileResolver
        // Use the embedded "coding" as the active profile
        let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
        let profiles = resolver.list_profiles();
        let community = profiles.iter().find(|p| p.name == "list-test");
        assert!(
            community.is_some(),
            "community profile should appear in list"
        );
        assert_eq!(community.unwrap().source, ProfileSource::Community);
    }

    #[test]
    fn community_source_origin_populated_from_sidecar() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        let toml_content = r#"
name = "sidecar-test"
[persona]
adapter = "minimal"
"#;
        std::fs::write(community_dir.join("sidecar-test.toml"), toml_content).unwrap();
        // Write sidecar with origin
        let sidecar_path = community_dir
            .join("sidecar-test.toml")
            .with_extension("toml.source");
        std::fs::write(&sidecar_path, "gh:owner/repo").unwrap();
        let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
        let profiles = resolver.list_profiles();
        let community = profiles.iter().find(|p| p.name == "sidecar-test").unwrap();
        assert_eq!(community.source_origin.as_deref(), Some("gh:owner/repo"));
    }

    #[test]
    fn community_source_origin_none_when_sidecar_missing() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        let toml_content = r#"
name = "no-sidecar"
[persona]
adapter = "minimal"
"#;
        std::fs::write(community_dir.join("no-sidecar.toml"), toml_content).unwrap();
        let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
        let profiles = resolver.list_profiles();
        let community = profiles.iter().find(|p| p.name == "no-sidecar").unwrap();
        assert_eq!(community.source_origin, None);
    }

    #[test]
    fn user_shadows_community_shadows_builtin() {
        let tmpdir = make_tempdir();
        let community_dir = tmpdir.path().join("community");
        std::fs::create_dir_all(&community_dir).unwrap();
        // Write community profile named "coding"
        let community_toml = r#"
name = "coding"
[persona]
adapter = "minimal"
"#;
        std::fs::write(community_dir.join("coding.toml"), community_toml).unwrap();
        // Write user profile named "coding"
        let user_toml = r#"
name = "coding"
extends = "base"
[persona]
adapter = "coding"
"#;
        std::fs::write(tmpdir.path().join("coding.toml"), user_toml).unwrap();
        let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
        let profiles = resolver.list_profiles();
        // There should be exactly one "coding" entry
        let coding_entries: Vec<_> = profiles.iter().filter(|p| p.name == "coding").collect();
        assert_eq!(coding_entries.len(), 1, "only one coding entry expected");
        assert_eq!(coding_entries[0].source, ProfileSource::User);
    }
}
