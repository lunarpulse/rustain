//! `rustain profile show <name>` — displays the fully resolved profile.
//! Story 8.6a AC-2, FR71.

use std::sync::Arc;

use anyhow::Result;

use super::prompt::source_label;
use crate::adapters::cli::commands::Cli;
use crate::adapters::profile_resolver::embedded::embedded_names;
use crate::domain::models::{AppConfig, PortDimension, ProfileSource};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::profile_serializer;
use crate::infrastructure::paths;

pub async fn run_profile_show(
    name: String,
    json: bool,
    toml_out: bool,
    _profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");

    // Load the profile
    let resolver = match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
        &name,
        profiles_dir.clone(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            // Exit 2 for profile not found / invalid
            std::process::exit(2);
        }
    };

    let resolved = resolver
        .resolve_active()
        .expect("TomlProfileResolver always has resolve_active");

    let source = determine_source(&name, &profiles_dir);
    let descriptor = {
        let list_resolver = crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            &name, profiles_dir.clone(),
        ).ok();
        list_resolver
            .and_then(|r| r.list_profiles().into_iter().find(|p| p.name == name))
    };

    // Check output format
    if toml_out {
        let flat = profile_serializer::to_flat_toml(&resolved, false, source)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        println!("{}", flat);
    } else if json {
        let json = resolved_to_json(&resolved, source, descriptor.as_ref());
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        // Human-readable output per AC-2
        println!("Profile: {}", resolved.name);
        println!("Source: {}", source_label(source));
        if let Some(ref desc) = descriptor.as_ref().and_then(|d| d.description.as_ref()) {
            println!("Description: {}", desc);
        }
        println!(
            "Identity color: {} (ANSI {})",
            color_name(&resolved.name),
            identity_color_value(&resolved.name)
        );
        println!("Preview: {}", resolved.preview);
        println!();

        println!("Adapter selection:");
        let port_order = [
            (PortDimension::Persona, "persona"),
            (PortDimension::Memory, "memory"),
            (PortDimension::Session, "session"),
            (PortDimension::Tools, "tools"),
            (PortDimension::Channels, "channels"),
            (PortDimension::Scheduler, "scheduler"),
            (PortDimension::Context, "context"),
        ];
        for (dim, label) in &port_order {
            if let Some(ar) = resolved.selection.dimensions.get(dim) {
                println!("  {:<12} {}", format!("{}:", label), ar.adapter);
            }
        }

        if let Some(ref overrides) = resolved.overrides {
            println!();
            println!("Overrides (figment):");
            // Pretty-print the overrides map
            print_overrides(overrides);
        }
    }

    tracing::info!(subcommand = "profile-show", profile = %name);
    Ok(())
}

fn determine_source(name: &str, profiles_dir: &std::path::Path) -> ProfileSource {
    if embedded_names().contains(&name) {
        let user_path = profiles_dir.join(format!("{}.toml", name));
        if user_path.exists() {
            ProfileSource::User // user shadowing built-in
        } else {
            ProfileSource::Builtin
        }
    } else {
        ProfileSource::User
    }
}

fn resolved_to_json(resolved: &crate::domain::models::ResolvedProfile, source: ProfileSource, descriptor: Option<&crate::domain::models::ProfileDescriptor>) -> serde_json::Value {
    use crate::domain::services::identity_color;

    let mut selection_map = serde_json::Map::new();
    let port_order = [
        ("persona", PortDimension::Persona),
        ("memory", PortDimension::Memory),
        ("session", PortDimension::Session),
        ("tools", PortDimension::Tools),
        ("channels", PortDimension::Channels),
        ("scheduler", PortDimension::Scheduler),
        ("context", PortDimension::Context),
    ];
    for (label, dim) in &port_order {
        if let Some(ar) = resolved.selection.dimensions.get(dim) {
            selection_map.insert(label.to_string(), serde_json::Value::String(ar.adapter.clone()));
        }
    }

    let overrides_json = resolved
        .overrides
        .as_ref()
        .and_then(|v| serde_json::to_value(v).ok());

    let color = identity_color::derive_identity_color(&resolved.name, None);
    serde_json::json!({
        "name": resolved.name,
        "source": source_label(source),
        "description": descriptor.and_then(|d| d.description.clone()).unwrap_or_default(),
        "identity_color": color.0,
        "preview": resolved.preview,
        "selection": selection_map,
        "overrides": overrides_json,
    })
}

fn identity_color_value(name: &str) -> u8 {
    use crate::domain::services::identity_color;
    identity_color::derive_identity_color(name, None).0
}

fn color_name(_name: &str) -> &'static str {
    // Match on the identity color value; for now return descriptive string
    // The color->ANSI name mapping (0-15):
    const NAMES: &[&str] = &[
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        "bright black", "bright red", "bright green", "bright yellow",
        "bright blue", "bright magenta", "bright cyan", "bright white",
    ];
    let idx = identity_color_value(_name) as usize;
    if idx < NAMES.len() {
        NAMES[idx]
    } else {
        "unknown"
    }
}

fn print_overrides(overrides: &figment::value::Value) {
    match overrides {
        figment::value::Value::Dict(_, map) => {
            for (key, value) in map {
                let val_str = figment_value_display(value);
                println!("  {} = {}", key, val_str);
            }
        }
        _ => {
            println!("  {:?}", overrides);
        }
    }
}

fn figment_value_display(v: &figment::value::Value) -> String {
    // For strings, add explicit quotes for clarity in human-readable output
    if let Some(s) = v.as_str() {
        return format!("\"{}\"", s);
    }
    // For all other types, use Debug which produces readable output
    format!("{:?}", v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::profile_resolver::embedded::embedded_names;

    #[test]
    fn test_embedded_names_has_three() {
        let names = embedded_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"base"));
        assert!(names.contains(&"coding"));
        assert!(names.contains(&"personal-assistant"));
    }

    #[test]
    fn test_determine_source_builtin() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        // No user file → builtin
        assert_eq!(determine_source("coding", &profiles_dir), ProfileSource::Builtin);
    }

    #[test]
    fn test_determine_source_user_shadow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        std::fs::write(
            profiles_dir.join("coding.toml"),
            "name = \"coding\"\n",
        )
        .unwrap();
        assert_eq!(determine_source("coding", &profiles_dir), ProfileSource::User);
    }
}
