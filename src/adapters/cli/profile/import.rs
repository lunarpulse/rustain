//! `rustain profile import <path>` — validates + installs a profile TOML.
//! Also the single implementation behind `rustain profile install <local path>`
//! (Story 19.12, ruling A6): one read, one parse, one write path.
//! Story 8.6a AC-8, FR71; Story 19.12 AC1 (A20's traversal guard, A21's parameter).

use std::io::Read;
use std::sync::Arc;

use anyhow::Result;

use super::prompt::validate_profile_name;
use super::source::{check_name_collision, emit_feature_warnings, validate_or_flip};
use crate::adapters::cli::commands::Cli;
use crate::domain::models::{AppConfig, ProfileDefinition};
use crate::domain::ports::ProfileResolver;
use crate::infrastructure::paths;

/// Maximum profile size in bytes.
const MAX_PROFILE_SIZE: usize = 1024 * 1024; // 1 MB

/// Import a profile TOML from a local path or stdin.
///
/// `refuse_builtin_collision` — refuse a destination name that shadows a built-in
/// profile or clobbers an existing user profile (Story 19.12, A21).
/// `ProfileAction::Import` passes `false`: `import` is "load MY file" and its
/// shipped behaviour is unchanged, because making a scripted verb start refusing
/// would be a breaking change. The `install` local-path arm passes `true`:
/// `install` is "take SOMEONE ELSE'S file", and silently shadowing the default
/// profile with a stranger's TOML is what the guard exists for.
pub async fn run_profile_import(
    path: String,
    name_override: Option<String>,
    force: bool,
    refuse_builtin_collision: bool,
    strict_features: bool,
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

    // ── A20: the name INSIDE the file is attacker-controlled ────────────────
    //
    // 🔴 SHIPPED ARBITRARY-PATH WRITE, fixed here. `dest` below is
    // `profiles_dir.join(format!("{}.toml", def.name))`, so a file carrying
    // `name = "../../ESCAPED"` wrote OUTSIDE `RUSTAIN_CONFIG_DIR` — exit 0, with
    // a cheerful success line. Reproduced at Task 0. `install.rs` validated its
    // downloaded `def.name`; this path validated only the `--name` override.
    //
    // ⛔ The validation runs ALWAYS and BEFORE any destination path is built, and
    // it is deliberately before the `--name` override below: a traversal name in
    // the file is refused even when the caller renames it away, because the file
    // is the thing that is untrustworthy.
    //
    // The house pattern is `eprintln!` + `exit(2)` (`install.rs`, and this file's
    // own parse arm above). An `anyhow::Err` here would reach
    // `startup.rs`'s `tracing::error!` and refuse in silence — curing a write
    // primitive with an invisible message.
    if let Err(e) = validate_profile_name(&def.name) {
        eprintln!("Error: {}", e);
        std::process::exit(2);
    }

    // With stdin, --name is required
    if path == "-" && name_override.is_none() {
        anyhow::bail!("--name is required when importing from stdin (use `-`)");
    }

    // Apply name override with validation.
    //
    // ⚠ Named behaviour change (Story 19.12): this refusal used to be an
    // `anyhow::Err` — `startup.rs`'s `tracing::error!`, exit 1, ZERO bytes on
    // both streams. It now uses the file's own house pattern so a bad `--name`
    // says so. Nothing previously accepted is now refused; only the exit code
    // (1 -> 2) and the visibility change. Same class as the `profile create` TTY
    // refusal this story also makes visible.
    if let Some(new_name) = &name_override {
        if let Err(e) = validate_profile_name(new_name) {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
        def.name = new_name.clone();
    }

    // ── A21: the built-in / user collision guard, for `install` only ────────
    //
    // Runs after the parse, where `def.name` exists, and after the override, so
    // the name checked is the name that will be written. `import`'s caller passes
    // `false`, so this is inert on the shipped verb.
    if refuse_builtin_collision {
        if let Err(msg) = check_name_collision(&def.name, force, name_override.is_some()) {
            eprintln!("{}", msg);
            std::process::exit(2);
        }
    }

    // Direct imports are strict. Local installs pass their `--strict-features`
    // flag so the Install option keeps one meaning across local and `gh:` sources.
    let (validated_content, feature_warnings) = validate_or_flip(&content, &def, strict_features);

    // Determine destination
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");
    std::fs::create_dir_all(&profiles_dir)?;

    let dest = profiles_dir.join(format!("{}.toml", def.name));

    // Overwrite check
    if dest.exists() && !force {
        use std::io::{BufRead, Write};
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
        if let Some(pos) = validated_content.find(needle) {
            let before = &validated_content[..pos];
            let after_name_line = validated_content[pos..]
                .find('\n')
                .map(|n| &validated_content[pos + n..])
                .unwrap_or("");
            format!("{}name = \"{}\"{}", before, def.name, after_name_line)
        } else {
            validated_content.clone()
        }
    } else {
        validated_content
    };

    std::fs::write(&dest, &write_content)
        .map_err(|e| anyhow::anyhow!("Failed to write profile to {}: {}", dest.display(), e))?;

    emit_feature_warnings(&def.name, &feature_warnings);

    println!(
        "Profile '{}' imported. Activate now? Run: rustain --profile {}",
        def.name, def.name
    );

    tracing::info!(subcommand = "profile-import", profile = %def.name, source = %source_desc);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::source::SinglePathSource;
    use super::*;
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
