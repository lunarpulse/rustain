//! TomlProfileResolver — adapter that loads profiles from
//! `~/.config/rustain/profiles/{name}.toml` (custom) or the embedded
//! built-in catalog (fallback).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::adapters::profile_resolver::embedded::{embedded_names, EmbeddedProfileSource};
use crate::domain::errors::ProfileError;
use crate::domain::models::{ProfileDescriptor, ProfileIdentityColor, ProfileSelection, ProfileSource, ResolvedProfile};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::identity_color;
use crate::domain::services::profile_loader::{ProfileLoader, ProfileSource as LoaderProfileSource};

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

        let user_path = self.config_dir.join(format!("{name}.toml"));
        if user_path.exists() {
            match std::fs::read_to_string(&user_path) {
                Ok(content) => {
                    const MAX_PROFILE_SIZE: usize = 1024 * 1024;
                    if content.len() > MAX_PROFILE_SIZE {
                        tracing::warn!(
                            "Profile at {:?} exceeds 1 MB limit ({} bytes), falling back to embedded",
                            user_path,
                            content.len()
                        );
                        return self.embedded.get(name);
                    }
                    return Some(content);
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

        self.embedded.get(name)
    }
}

impl TomlProfileResolver {
    pub fn new(active_name: &str, config_dir: PathBuf) -> Result<Self, ProfileError> {
        let source = FileSystemProfileSource::new(config_dir.clone());
        let catalog = AdapterCatalog;
        let loader = ProfileLoader::new(&catalog, &source);
        let resolved = loader.load(active_name)?;
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
                if path.extension().map_or(false, |e| e == "toml") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        if name.contains(['/', '\\']) {
                            continue;
                        }
                        let content = match std::fs::read_to_string(&path) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        if let Ok(def) = toml::from_str::<crate::domain::models::ProfileDefinition>(&content) {
                            if seen.insert(def.name.clone()) {
                                profiles.push(build_descriptor(&def, ProfileSource::User));
                            }
                        }
                    }
                }
            }
        }

        // 2. Enumerate embedded profiles (only if not already seen from user override)
        for &name in embedded_names() {
            if !seen.contains(name) {
                let source = EmbeddedProfileSource;
                if let Some(content) = source.get(name) {
                    if let Ok(def) = toml::from_str::<crate::domain::models::ProfileDefinition>(&content) {
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

fn build_descriptor(def: &crate::domain::models::ProfileDefinition, source: ProfileSource) -> ProfileDescriptor {
    use crate::domain::models::{AdapterRef, PortDimension};
    let identity_color = identity_color::derive_identity_color(&def.name, def.identity_color);
    let mut dimensions = std::collections::BTreeMap::new();
    if let Some(ref r) = def.persona {
        dimensions.insert(PortDimension::Persona, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    if let Some(ref r) = def.memory {
        dimensions.insert(PortDimension::Memory, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    if let Some(ref r) = def.session {
        dimensions.insert(PortDimension::Session, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    if let Some(ref r) = def.tools {
        dimensions.insert(PortDimension::Tools, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    if let Some(ref r) = def.channels {
        dimensions.insert(PortDimension::Channels, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    if let Some(ref r) = def.scheduler {
        dimensions.insert(PortDimension::Scheduler, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    if let Some(ref r) = def.context {
        dimensions.insert(PortDimension::Context, AdapterRef { adapter: r.adapter.clone(), _config: r._config.clone() });
    }
    let selection = ProfileSelection { dimensions };
    ProfileDescriptor {
        name: def.name.clone(),
        description: def.description.clone(),
        preview: def.preview,
        identity_color,
        source,
        selection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use crate::domain::models::PortDimension;

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
        let resolver = TomlProfileResolver::new("custom-coding", tmpdir.path().to_path_buf()).unwrap();
        let profile = resolver.resolve_active().unwrap();
        assert_eq!(profile.selection.dimensions[&PortDimension::Persona].adapter, "personal-assistant");
        assert_eq!(profile.selection.dimensions[&PortDimension::Memory].adapter, "daily-log");
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
}
