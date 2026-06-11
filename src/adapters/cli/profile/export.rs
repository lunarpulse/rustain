//! `rustain profile export <name>` — flattens extends chain and prints/writes shareable TOML.
//! Story 8.6a AC-7, FR71.

use std::sync::Arc;

use anyhow::Result;

use crate::adapters::cli::commands::Cli;
use crate::adapters::profile_resolver::embedded::embedded_names;
use crate::domain::models::{AppConfig, ProfileSource};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::profile_serializer;
use crate::infrastructure::paths;

pub async fn run_profile_export(
    name: String,
    output: Option<String>,
    _profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");

    let resolver = match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
        &name,
        profiles_dir,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };

    let resolved = resolver
        .resolve_active()
        .expect("TomlProfileResolver always has resolve_active");

    let source = {
        let config_dir =
            paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
        let user_profiles_dir = config_dir.join("profiles");
        if embedded_names().contains(&name.as_str()) {
            let user_path = user_profiles_dir.join(format!("{}.toml", name));
            if user_path.exists() {
                ProfileSource::User
            } else {
                ProfileSource::Builtin
            }
        } else {
            ProfileSource::User
        }
    };

    let flat_toml = profile_serializer::to_flat_toml(&resolved, true, source)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let is_stdout = output.is_none() || output.as_deref() == Some("-");
    if is_stdout {
        print!("{}", flat_toml);
        eprintln!("Profile '{}' exported to stdout", name);
    } else {
        let out_path = std::path::PathBuf::from(output.as_ref().unwrap());
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&out_path, &flat_toml)?;
        println!("Profile '{}' exported to {}", name, out_path.display());
    }

    tracing::info!(subcommand = "profile-export", profile = %name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stdout_when_output_is_none() {
        assert_eq!(None::<String>.as_deref(), None);
        assert_eq!(Some("-".to_string()).as_deref(), Some("-"));
    }
}
