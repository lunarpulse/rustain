//! TomlProfileResolver — adapter that loads profiles from
//! `~/.config/rustain/profiles/{name}.toml` (custom) or the embedded
//! built-in catalog (fallback).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::adapters::profile_resolver::embedded::EmbeddedProfileSource;
use crate::domain::errors::ProfileError;
use crate::domain::models::ResolvedProfile;
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::profile_loader::{ProfileLoader, ProfileSource};

pub struct TomlProfileResolver {
    resolved: ResolvedProfile,
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

impl ProfileSource for FileSystemProfileSource {
    fn get(&self, name: &str) -> Option<String> {
        if name.contains(|c: char| c == '/' || c == '\\') {
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
        let source = FileSystemProfileSource::new(config_dir);
        // We need to own the source - so we create the loader locally and resolve
        let catalog = AdapterCatalog;
        let loader = ProfileLoader::new(&catalog, &source);
        let resolved = loader.load(active_name)?;
        Ok(Self {
            resolved,
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
    // resolve_active_profile_defaults uses the trait default
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
