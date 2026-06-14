//! Shared editor resolution and launch helpers.
//!
//! Extracted from `adapters/cli/profile/edit.rs` (Story 13.2a AC3) so both
//! `profile edit` and `config edit` use the same `$VISUAL → $EDITOR → platform`
//! resolution logic without duplication.

use anyhow::Result;

/// Resolve the editor command.
///
/// Precedence: `$VISUAL` → `$EDITOR` → platform default (`vi` on Unix,
/// `notepad.exe` on Windows). Delegates to `env_var_trimmed` for env reads.
pub fn resolve_editor() -> Result<String> {
    let visual = super::env_var_trimmed("VISUAL");
    if let Some(editor) = visual {
        return Ok(editor);
    }

    let editor_env = super::env_var_trimmed("EDITOR");
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
        anyhow::bail!("No editor configured. Set $EDITOR or $VISUAL (e.g., export EDITOR=nano)");
    }
}

/// Launch the editor on a file path and wait for it to exit.
///
/// Handles multi-word editor commands (e.g. `"code --wait"`) and editor paths
/// containing spaces (e.g. `"/Applications/My Editor.app/Contents/MacOS/editor"`).
pub fn run_editor(editor: &str, path: &std::path::Path) -> Result<std::process::ExitStatus> {
    let parts = shell_words::split(editor)?;
    let (cmd, args) = match parts.split_first() {
        Some((c, a)) => (c.as_str(), a),
        None => anyhow::bail!("Editor command is empty"),
    };
    let mut command = std::process::Command::new(cmd);
    for arg in args {
        command.arg(arg);
    }
    command.arg(path);
    command
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to launch editor '{}': {}", editor, e))
}
