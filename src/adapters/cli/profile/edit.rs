//! `rustain profile edit <name>` — opens the user TOML in `$EDITOR`.
//! Story 8.6a AC-4, FR71.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::Result;

use super::prompt::{prompt_yes_no, validate_profile_name};
use crate::adapters::cli::commands::Cli;
use crate::adapters::profile_resolver::embedded::{embedded_names, EmbeddedProfileSource};
use crate::domain::models::AppConfig;
use crate::domain::ports::ProfileResolver;
use crate::domain::services::profile_loader::ProfileSource;
use crate::infrastructure::paths;

pub async fn run_profile_edit(
    name: String,
    no_validate: bool,
    _profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    // TTY guard FIRST
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("rustain profile edit requires an interactive terminal.");
    }

    validate_profile_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");
    std::fs::create_dir_all(&profiles_dir)?;

    let dest = profiles_dir.join(format!("{}.toml", name));

    // If file doesn't exist but name matches a built-in, offer to copy
    if !dest.exists() && embedded_names().contains(&name.as_str()) {
        if !prompt_yes_no(&format!(
            "Built-in profile '{}' has no user override yet. Create a copy in {} from the built-in template before editing?",
            name,
            dest.display()
        ))? {
            println!(
                "Use a different name (e.g. `rustain profile create`) to start fresh."
            );
            return Ok(());
        }
        // Write embedded content to user path
        let embedded = EmbeddedProfileSource;
        if let Some(content) = embedded.get(&name) {
            let header = format!("# Edited copy of built-in profile '{}'\n", name);
            std::fs::write(&dest, format!("{}{}", header, content))?;
        }
    }

    // Resolve editor: $VISUAL > $EDITOR > platform default
    let editor = resolve_editor()?;

    // Editor loop
    loop {
        let status = run_editor(&editor, &dest)?;

        if !status.success() {
            let code = status.code().unwrap_or(1);
            eprintln!(
                "Editor exited with status {}. Profile not modified.",
                code
            );
            tracing::info!(subcommand = "profile-edit", profile = %name, editor_exit = code);
            std::process::exit(code);
        }

        if no_validate {
            println!("Profile '{}' saved (validation skipped).", name);
            tracing::info!(subcommand = "profile-edit", profile = %name, validated = false);
            return Ok(());
        }

        match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            &name,
            profiles_dir.clone(),
        ) {
            Ok(_) => {
                println!("Profile '{}' saved and validated.", name);
                tracing::info!(subcommand = "profile-edit", profile = %name, validated = true);
                return Ok(());
            }
            Err(e) => {
                eprintln!("Profile validation failed: {}", e);
                if !prompt_yes_no("Re-open editor to fix?")? {
                    eprintln!(
                        "Saved file is invalid. Run 'rustain profile validate {}' for details, or 'rustain profile edit {}' to fix.",
                        name, name
                    );
                    return Err(crate::infrastructure::startup::SubcommandExit.into());
                }
            }
        }
    }
}

/// Resolve the editor command per Decision Gate 1.3.
/// Precedence: $VISUAL → $EDITOR → platform default (vi on unix, notepad on windows).
fn resolve_editor() -> Result<String> {
    let visual = crate::infrastructure::utils::env_var_trimmed("VISUAL");
    if let Some(editor) = visual {
        return Ok(editor);
    }

    let editor_env = crate::infrastructure::utils::env_var_trimmed("EDITOR");
    if let Some(editor) = editor_env {
        return Ok(editor);
    }

    // Platform default
    #[cfg(target_family = "unix")]
    {
        Ok("vi".to_string())
    }
    #[cfg(target_family = "windows")]
    {
        Ok("notepad.exe".to_string())
    }
    #[cfg(not(any(target_family = "unix", target_family = "windows")))]
    {
        anyhow::bail!(
            "No editor configured. Set $EDITOR or $VISUAL (e.g., export EDITOR=nano)"
        );
    }
}

fn run_editor(editor: &str, path: &std::path::Path) -> Result<std::process::ExitStatus> {
    let parts: Vec<&str> = editor.split_whitespace().collect();
    let (cmd, args) = match parts.split_first() {
        Some((c, a)) => (*c, a),
        None => anyhow::bail!("Editor command is empty"),
    };
    let mut command = std::process::Command::new(cmd);
    for arg in args {
        command.arg(arg);
    }
    command.arg(path);
    command.status().map_err(|e| anyhow::anyhow!("Failed to launch editor '{}': {}", editor, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test editor resolution precedence. Must run with --test-threads=1
    /// due to env-var manipulation (standard for env-dependent tests in this repo).
    #[test]
    fn test_resolve_editor_precedence() {
        let orig_visual = std::env::var("VISUAL").ok();
        let orig_editor = std::env::var("EDITOR").ok();

        // CONFORMANCE_EXCEPTION: test env manipulation
        // VISUAL wins when set
        unsafe {
            std::env::set_var("VISUAL", "myvisual");
            std::env::remove_var("EDITOR");
        }
        assert_eq!(resolve_editor().unwrap(), "myvisual");

        // EDITOR used when VISUAL absent
        unsafe {
            std::env::remove_var("VISUAL");
            std::env::set_var("EDITOR", "myeditor");
        }
        assert_eq!(resolve_editor().unwrap(), "myeditor");

        // Platform default when neither set
        unsafe {
            std::env::remove_var("VISUAL");
            std::env::remove_var("EDITOR");
        }
        let result = resolve_editor().unwrap();
        // On unix this will be "vi", on windows "notepad.exe"
        assert!(!result.is_empty());

        // Restore
        match orig_visual {
            Some(v) => unsafe { std::env::set_var("VISUAL", v) },
            None => unsafe { std::env::remove_var("VISUAL") },
        }
        match orig_editor {
            Some(v) => unsafe { std::env::set_var("EDITOR", v) },
            None => unsafe { std::env::remove_var("EDITOR") },
        }
    }
}
