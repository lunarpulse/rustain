//! `rustain profile validate <name>` — runs validation (5 passes).
//! Story 8.6a AC-6, FR72.

use std::sync::Arc;

use anyhow::Result;

use super::prompt::fix_profile_error;
use crate::adapters::cli::commands::Cli;
use crate::domain::errors::ProfileError;
use crate::domain::models::AppConfig;
use crate::domain::ports::ProfileResolver;
use crate::domain::services::identity_color;
use crate::infrastructure::paths;

pub async fn run_profile_validate(
    name: Option<String>,
    all: bool,
    json: bool,
    profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    // Resolve targets: --all, explicit name, or no-arg (= --all per Decision Gate 1.8)
    let do_all = all || name.is_none();
    let profiles = profile_resolver.list_profiles();

    let targets: Vec<String> = if do_all {
        profiles.iter().map(|p| p.name.clone()).collect()
    } else {
        vec![name.unwrap()]
    };

    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut any_failed = false;

    for profile_name in &targets {
        match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            profile_name,
            profiles_dir.clone(),
        ) {
            Ok(resolver) => {
                if let Some(resolved) = resolver.resolve_active() {
                    let color = identity_color::derive_identity_color(profile_name, None);
                    if json {
                        results.push(serde_json::json!({
                            "valid": true,
                            "name": profile_name,
                            "summary": {
                                "ports": 7,
                                "identity_color": color.0,
                                "preview": resolved.preview,
                            }
                        }));
                    } else {
                        println!(
                            "Profile '{}' is valid. 7 ports resolved, identity color {}, preview: {}",
                            profile_name, color.0, resolved.preview
                        );
                    }
                } else {
                    any_failed = true;
                    if json {
                        results.push(serde_json::json!({
                            "valid": false,
                            "name": profile_name,
                            "error": {
                                "kind": "ResolveFailed",
                                "detail": format!("Profile '{}' could not be resolved.", profile_name),
                                "fix": "Fix: check that the profile TOML is well-formed and all extends references exist.".to_string(),
                            },
                        }));
                    } else {
                        eprintln!("Profile '{}' could not be resolved.", profile_name);
                        eprintln!(
                            "Fix: check that the profile TOML is well-formed and all extends references exist."
                        );
                        eprintln!();
                    }
                }
            }
            Err(e) => {
                any_failed = true;
                if json {
                    let error_json = format_profile_error_json(&e);
                    results.push(serde_json::json!({
                        "valid": false,
                        "name": profile_name,
                        "error": error_json,
                    }));
                } else {
                    eprintln!("{}", e);
                    eprintln!("{}", fix_profile_error(&e));
                    eprintln!();
                }
            }
        }
    }

    if json {
        let output = serde_json::json!({
            "valid": !any_failed,
            "profiles": results,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if do_all && !any_failed {
        println!("\nAll {} profile(s) validated successfully.", targets.len());
    }

    tracing::info!(
        subcommand = "profile-validate",
        profile_count = targets.len(),
        any_failed
    );

    if any_failed {
        let exit_code = if do_all { 1 } else { 2 };
        std::process::exit(exit_code);
    }

    Ok(())
}

fn format_profile_error_json(e: &ProfileError) -> serde_json::Value {
    serde_json::json!({
        "kind": error_kind(e),
        "detail": e.to_string(),
        "fix": fix_profile_error(e),
    })
}

fn error_kind(e: &ProfileError) -> &'static str {
    match e {
        ProfileError::ProfileNotFound { .. } => "ProfileNotFound",
        ProfileError::ParentNotFound { .. } => "ParentNotFound",
        ProfileError::CircularExtends { .. } => "CircularExtends",
        ProfileError::ExtendsTooDeep { .. } => "ExtendsTooDeep",
        ProfileError::DimensionMissing { .. } => "DimensionMissing",
        ProfileError::AdapterUnknown { .. } => "AdapterUnknown",
        ProfileError::AdapterFeatureGated { .. } => "AdapterFeatureGated",
        ProfileError::Parse { .. } => "Parse",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::PortDimension;

    #[test]
    fn test_fix_text_dimension_missing() {
        let err = ProfileError::DimensionMissing {
            profile: "test".into(),
            dimensions: vec![PortDimension::Memory, PortDimension::Tools],
        };
        let fix = fix_profile_error(&err);
        assert!(fix.contains("memory"));
        assert!(fix.contains("tools"));
        assert!(fix.contains("port section"));
    }

    #[test]
    fn test_fix_text_adapter_unknown_with_suggestion() {
        let err = ProfileError::AdapterUnknown {
            profile: "test".into(),
            port: PortDimension::Memory,
            adapter: "projct-scoped".into(),
            available: vec![],
            suggestion: Some("project-scoped".into()),
        };
        let fix = fix_profile_error(&err);
        assert!(fix.contains("project-scoped"));
        assert!(fix.contains("Levenshtein-1"));
    }

    #[test]
    fn test_fix_text_circular_extends() {
        let err = ProfileError::CircularExtends {
            chain: vec!["a".into(), "b".into(), "a".into()],
        };
        let fix = fix_profile_error(&err);
        assert!(fix.contains("a → b → a"));
    }

    #[test]
    fn test_fix_text_extends_too_deep() {
        let err = ProfileError::ExtendsTooDeep {
            chain: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
        };
        let fix = fix_profile_error(&err);
        assert!(fix.contains("5"));
        assert!(fix.contains("≤ 4"));
    }

    #[test]
    fn test_fix_text_adapter_feature_gated() {
        let err = ProfileError::AdapterFeatureGated {
            profile: "test".into(),
            port: PortDimension::Channels,
            adapter: "telegram".into(),
            feature: "telegram".into(),
        };
        let fix = fix_profile_error(&err);
        assert!(fix.contains("--features telegram"));
        assert!(fix.contains("preview = true"));
    }
}
