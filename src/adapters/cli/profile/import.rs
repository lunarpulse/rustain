//! `rustain profile import <path>` — validates + installs a profile TOML.
//! Story 8.6a AC-8, FR71.

use std::io::Read;
use std::sync::Arc;

use anyhow::Result;

use super::prompt::{fix_profile_error, validate_profile_name};
use super::source::SinglePathSource;
use crate::adapters::cli::commands::Cli;
use crate::adapters::profile_resolver::embedded::EmbeddedProfileSource;
use crate::domain::errors::ProfileError;
use crate::domain::models::{AppConfig, ProfileDefinition};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::profile_loader::ProfileLoader;
use crate::infrastructure::paths;

/// Maximum profile size in bytes.
const MAX_PROFILE_SIZE: usize = 1024 * 1024; // 1 MB

pub async fn run_profile_import(
    path: String,
    name_override: Option<String>,
    force: bool,
    _profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    // Read source content
    let (content, source_desc) = if path == "-" {
        // Read from stdin
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow::anyhow!("Failed to read from stdin: {}", e))?;
        if buf.len() > MAX_PROFILE_SIZE {
            anyhow::bail!("Input exceeds 1 MB limit ({} bytes)", buf.len());
        }
        (buf, "<stdin>".to_string())
    } else {
        let file_path = std::path::Path::new(&path);
        let metadata = std::fs::metadata(file_path)
            .map_err(|e| anyhow::anyhow!("Failed to read file at {}: {}", path, e))?;
        if metadata.len() as usize > MAX_PROFILE_SIZE {
            anyhow::bail!(
                "Profile at {} exceeds 1 MB limit ({} bytes)",
                path,
                metadata.len()
            );
        }
        let content = std::fs::read_to_string(file_path)
            .map_err(|e| anyhow::anyhow!("Failed to read file at {}: {}", path, e))?;
        (content, path.clone())
    };

    // Parse TOML
    let mut def: ProfileDefinition = match toml::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: Failed to parse TOML at {}: {}", source_desc, e);
            std::process::exit(2);
        }
    };

    // With stdin, --name is required
    if path == "-" && name_override.is_none() {
        anyhow::bail!("--name is required when importing from stdin (use `-`)");
    }

    // Apply name override with validation
    if let Some(ref new_name) = name_override {
        validate_profile_name(new_name).map_err(|e| anyhow::anyhow!(e))?;
        def.name = new_name.clone();
    }

    // Validate via in-memory ProfileSource (Decision Gate 1.11)
    let source = SinglePathSource {
        name: def.name.clone(),
        content: content.clone(),
        fallback: EmbeddedProfileSource,
    };
    let loader = ProfileLoader::new(&AdapterCatalog, &source);
    loader.load(&def.name).map_err(|e| {
        eprintln!("Profile validation failed: {}", e);
        eprintln!("{}", fix_profile_error(&e));
        std::process::exit(2);
    })?;

    // Determine destination
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");
    std::fs::create_dir_all(&profiles_dir)?;

    let dest = profiles_dir.join(format!("{}.toml", def.name));

    // Overwrite check
    if dest.exists() && !force {
        use std::io::{Write, BufRead};
        let mut input = String::new();
        print!(
            "Profile '{}' already exists at {}. Overwrite? [y/n] ",
            def.name,
            dest.display()
        );
        std::io::stdout().flush()?;
        std::io::stdin().lock().read_line(&mut input)?;
        if !input.trim().starts_with(['y', 'Y']) {
            println!("Import cancelled. Existing profile preserved.");
            return Ok(());
        }
    }

    // If --name override was used, rewrite the name field in the TOML content
    let write_content = if name_override.is_some() {
        let needle = "name =";
        if let Some(pos) = content.find(needle) {
            let before = &content[..pos];
            let after_name_line = content[pos..]
                .find('\n')
                .map(|n| &content[pos + n..])
                .unwrap_or("");
            format!("{}name = \"{}\"{}", before, def.name, after_name_line)
        } else {
            content.clone()
        }
    } else {
        content.clone()
    };

    std::fs::write(&dest, &write_content)
        .map_err(|e| anyhow::anyhow!("Failed to write profile to {}: {}", dest.display(), e))?;

    println!(
        "Profile '{}' imported. Activate now? Run: rustain --profile {}",
        def.name, def.name
    );

    tracing::info!(subcommand = "profile-import", profile = %def.name, source = %source_desc);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::source::SinglePathSource;
    use crate::adapters::profile_resolver::embedded::EmbeddedProfileSource;
    use crate::domain::services::profile_loader::ProfileSource as LoaderProfileSource;

    #[test]
    fn test_max_profile_size_constant() {
        assert_eq!(MAX_PROFILE_SIZE, 1024 * 1024);
    }

    #[test]
    fn test_single_path_source_resolves_own_name() {
        let source = SinglePathSource {
            name: "test-profile".into(),
            content: "name = \"test-profile\"\n".into(),
            fallback: EmbeddedProfileSource,
        };
        assert!(source.get("test-profile").is_some());
    }

    #[test]
    fn test_single_path_source_falls_back_to_embedded() {
        let source = SinglePathSource {
            name: "test-profile".into(),
            content: "name = \"test-profile\"\n".into(),
            fallback: EmbeddedProfileSource,
        };
        // Resolve a different name (e.g., for extends) → falls back to embedded
        let base = source.get("base");
        assert!(base.is_some());
        assert!(base.unwrap().contains("name = \"base\""));
    }
}
