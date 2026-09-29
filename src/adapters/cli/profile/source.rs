//! Shared in-memory ProfileSource for validation of imported/installed profiles,
//! and the built-in/user name-collision guard both write paths consult.
//!
//! Resolves the target name from the in-memory content; falls back to EmbeddedProfileSource
//! for extends = "base" resolution. Reused by import.rs (local path / stdin) and install.rs
//! (community profile fetched from gh:user/repo).
//!
//! ⚠ `check_name_collision` LIVES HERE, not in `install.rs`, and the reason is
//! structural rather than tidiness (Story 19.12, ruling A21): `install.rs` is
//! `#[cfg]`-gated on `any(anthropic, openai, ollama)` (`mod.rs:5-6`) while
//! `import.rs` is not gated at all. A private `fn` in the gated module cannot be
//! called from the ungated one — it would not resolve, and it would make an
//! ungated module depend on a gated one. This module already exists for exactly
//! this sharing, so the guard moved here instead of being duplicated.

use std::sync::LazyLock;

use regex::Regex;

use super::prompt::fix_profile_error;
use crate::domain::errors::ProfileError;
use crate::domain::models::{PortDimension, ProfileDefinition};
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::profile_loader::ProfileLoader;

use crate::adapters::profile_resolver::embedded::{EmbeddedProfileSource, embedded_names};
use crate::domain::services::profile_loader::ProfileSource as LoaderProfileSource;
use crate::infrastructure::paths;

static PREVIEW_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^preview\s*=\s*(true|false)\s*$").expect("PREVIEW_LINE_RE compile")
});

pub(super) struct SinglePathSource {
    pub(super) name: String,
    pub(super) content: String,
    pub(super) fallback: EmbeddedProfileSource,
}

impl LoaderProfileSource for SinglePathSource {
    fn get(&self, name: &str) -> Option<String> {
        if name == self.name {
            Some(self.content.clone())
        } else {
            self.fallback.get(name)
        }
    }
}

/// Refuse a destination name that would shadow a built-in profile or clobber an
/// existing user profile.
///
/// The escape hatch needs BOTH `--force` AND `--name <override>`: `--force`
/// alone still refuses, which is why the message names only `--name`.
pub(super) fn check_name_collision(
    target_name: &str,
    force: bool,
    name_override_provided: bool,
) -> Result<(), String> {
    // Check embedded names
    if embedded_names().contains(&target_name) && !(force && name_override_provided) {
        return Err(format!(
            "Error: profile name '{}' collides with a built-in profile. Pass --name <override> to install under a different name.",
            target_name
        ));
    }

    // Check user profiles
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let user_dest = config_dir
        .join("profiles")
        .join(format!("{}.toml", target_name));
    if user_dest.exists() && !(force && name_override_provided) {
        return Err(format!(
            "Error: profile name '{}' collides with a user profile. Pass --name <override> to install under a different name.",
            target_name
        ));
    }

    Ok(())
}

pub(super) struct FeatureGateInfo {
    pub(super) feature: String,
    pub(super) adapter: String,
    pub(super) port: String,
}

/// Validate a profile, optionally rewriting feature-gated profiles into preview mode.
///
/// Direct imports pass `strict_features = true`; installs pass the command's flag,
/// so changing only the source never changes the meaning of `--strict-features`.
pub(super) fn validate_or_flip(
    content: &str,
    def: &ProfileDefinition,
    strict_features: bool,
) -> (String, Vec<FeatureGateInfo>) {
    let validate = |content: &str| -> Result<(), ProfileError> {
        let def: ProfileDefinition =
            toml::from_str(content).map_err(|_| ProfileError::ProfileNotFound {
                name: "in-memory".into(),
                search_paths: vec![],
            })?;
        let source = SinglePathSource {
            name: def.name.clone(),
            content: content.to_string(),
            fallback: EmbeddedProfileSource,
        };
        ProfileLoader::new(&AdapterCatalog, &source)
            .load(&def.name)
            .map(|_| ())
    };

    match validate(content) {
        Ok(()) => (content.to_string(), Vec::new()),
        Err(ProfileError::AdapterFeatureGated { .. }) if !strict_features => {
            let already_preview = def.preview
                || PREVIEW_LINE_RE
                    .captures(content)
                    .and_then(|captures| captures.get(1).map(|value| value.as_str() == "true"))
                    .unwrap_or(false);

            let warnings = scan_features(def);
            if already_preview {
                (content.to_string(), warnings)
            } else {
                let rewritten = apply_preview_flip(content);
                match validate(&rewritten) {
                    Ok(()) => (rewritten, warnings),
                    Err(error) => {
                        eprintln!("Validation still failed after preview flip: {}", error);
                        eprintln!("{}", fix_profile_error(&error));
                        std::process::exit(2);
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("Profile validation failed: {}", error);
            eprintln!("{}", fix_profile_error(&error));
            std::process::exit(2);
        }
    }
}

pub(super) fn emit_feature_warnings(profile: &str, warnings: &[FeatureGateInfo]) {
    if warnings.is_empty() {
        return;
    }

    let features = warnings
        .iter()
        .map(|warning| warning.feature.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "Warning: profile '{}' references adapters not compiled into this binary:",
        profile
    );
    for warning in warnings {
        eprintln!(
            "  - {} (port: {}; requires --features {})",
            warning.adapter, warning.port, warning.feature
        );
    }
    eprintln!(
        "Set preview = true so the profile falls back to no-op adapters for missing dimensions.\n\
         Profile installed with preview = true. To use the full profile, rebuild with: cargo install rustain --features {}",
        features
    );
}

fn scan_features(def: &ProfileDefinition) -> Vec<FeatureGateInfo> {
    let dimensions = [
        (def.persona.as_ref(), PortDimension::Persona),
        (def.memory.as_ref(), PortDimension::Memory),
        (def.session.as_ref(), PortDimension::Session),
        (def.tools.as_ref(), PortDimension::Tools),
        (def.channels.as_ref(), PortDimension::Channels),
        (def.scheduler.as_ref(), PortDimension::Scheduler),
        (def.context.as_ref(), PortDimension::Context),
    ];
    let mut features = Vec::new();
    for (adapter_ref, port) in dimensions {
        if let Some(adapter_ref) = adapter_ref {
            if let Some(descriptor) = AdapterCatalog::lookup(port, &adapter_ref.adapter) {
                if let Some(feature) = descriptor.feature_gate {
                    if !AdapterCatalog::is_feature_compiled(feature) {
                        features.push(FeatureGateInfo {
                            feature: feature.to_string(),
                            adapter: adapter_ref.adapter.clone(),
                            port: format!("{:?}", port),
                        });
                    }
                }
            }
        }
    }
    features
}

pub(super) fn apply_preview_flip(content: &str) -> String {
    if PREVIEW_LINE_RE.is_match(content) {
        PREVIEW_LINE_RE
            .replace(content, "preview = true")
            .to_string()
    } else if let Some(table_start) = content.find("\n[") {
        let insert_at = table_start + 1;
        let (root, tables) = content.split_at(insert_at);
        format!("{root}preview = true\n{tables}")
    } else {
        let separator = if content.ends_with('\n') { "" } else { "\n" };
        format!("{content}{separator}preview = true\n")
    }
}
