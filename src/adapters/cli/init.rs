use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::domain::models::AppConfig;
use crate::infrastructure::paths;

/// Entry point for the `rustain init` wizard.
///
/// Runs in standard terminal mode (NOT ratatui). Uses println!/print! for output
/// and std::io::stdin() for input. Must NOT trigger provider construction or terminal setup.
pub async fn run_init() -> Result<()> {
    run_init_with_paths(None, None).await
}

/// Testable init implementation that accepts optional path overrides.
pub async fn run_init_with_paths(
    config_dir_override: Option<PathBuf>,
    workspace_override: Option<PathBuf>,
) -> Result<()> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        anyhow::bail!(
            "rustain init requires an interactive terminal. Use `rustain init --non-interactive` for CI environments (available in a future release)."
        );
    }
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    run_init_with_io(
        config_dir_override,
        workspace_override,
        true,
        find_api_key_var(),
        &mut input,
        &mut output,
    )
}

fn run_init_with_io<R: BufRead, W: Write>(
    config_dir_override: Option<PathBuf>,
    workspace_override: Option<PathBuf>,
    is_tty: bool,
    api_key_var: Option<&str>,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    if !is_tty {
        anyhow::bail!(
            "rustain init requires an interactive terminal. Use `rustain init --non-interactive` for CI environments (available in a future release)."
        );
    }

    let config_dir = match config_dir_override {
        Some(d) => d,
        None => paths::config_dir()?,
    };
    let workspace = match workspace_override {
        Some(w) => w,
        None => paths::workspace_dir()?,
    };
    let config_toml_path = config_dir.join("config.toml");
    let settings_json_path = workspace.join(".claude").join("settings.json");
    let interaction_policy_path =
        crate::adapters::policy::config::individual_policy_path(&workspace);
    let config_existed = config_toml_path.exists();
    let settings_existed = settings_json_path.exists();
    let policy_existed = interaction_policy_path.exists();
    let overwrite = !(config_existed || settings_existed || policy_existed)
        || prompt_yes_no("Configuration already exists. Overwrite?", input, output)?;

    let api_key_found = detect_api_key(api_key_var, input, output)?;
    create_directories(&config_dir, &workspace)?;

    if config_existed && !overwrite {
        writeln!(output, "Existing global configuration preserved.")?;
    } else {
        write_config_toml(&config_toml_path)?;
    }
    if settings_existed {
        writeln!(output, "Existing workspace permissions preserved.")?;
    } else {
        write_settings_json(&settings_json_path)?;
    }

    let policy_status = if policy_existed && !overwrite {
        FileWriteStatus::Preserved
    } else {
        let response_mode = prompt_response_mode(input, output)?;
        let urgency = prompt_notification_urgency(input, output)?;
        write_interaction_policy(&interaction_policy_path, response_mode, urgency)?;
        if policy_existed {
            FileWriteStatus::Replaced
        } else {
            FileWriteStatus::Created
        }
    };

    let sessions_dir = workspace.join(".claude").join("sessions");
    display_summary(
        &config_toml_path,
        &settings_json_path,
        &interaction_policy_path,
        policy_status,
        &sessions_dir,
        api_key_found,
        output,
    )?;
    Ok(())
}

/// Check which API key environment variable is set (if any).
/// Returns the variable name if found, None otherwise.
/// Checks ANTHROPIC_AUTH_TOKEN first (CC precedence), then ANTHROPIC_API_KEY.
/// Filters out empty and whitespace-only values. NEVER returns the key value (NFR11).
pub fn find_api_key_var() -> Option<&'static str> {
    use crate::infrastructure::utils::env_var_is_set;

    if env_var_is_set("ANTHROPIC_AUTH_TOKEN") {
        return Some("ANTHROPIC_AUTH_TOKEN");
    }
    if env_var_is_set("ANTHROPIC_API_KEY") {
        return Some("ANTHROPIC_API_KEY");
    }
    None
}

// AC4 guard: `run_init` does no network I/O today (detect_api_key reads env only).
// If a live key-validation step is ever added, it MUST skip on transport error with:
// "⚠ Skipped API key validation (offline). Verify later with 'rustain doctor'."
/// Detect API key presence from environment variables.
/// Returns true if a key was found and user confirmed, false otherwise.
/// NEVER displays the key value (NFR11).
fn detect_api_key<R: BufRead, W: Write>(
    api_key_var: Option<&str>,
    input: &mut R,
    output: &mut W,
) -> Result<bool> {
    if let Some(var_name) = api_key_var
        && prompt_yes_no(&format!("Found {var_name}. Use this?"), input, output)?
    {
        return Ok(true);
    }

    writeln!(output)?;
    writeln!(output, "Set ANTHROPIC_API_KEY in your shell profile:")?;
    writeln!(output)?;
    writeln!(output, "  export ANTHROPIC_API_KEY=sk-ant-...")?;
    writeln!(output)?;
    writeln!(
        output,
        "Get your API key at: https://console.anthropic.com/"
    )?;
    writeln!(output)?;
    Ok(false)
}

/// Create required directories.
pub fn create_directories(config_dir: &std::path::Path, workspace: &std::path::Path) -> Result<()> {
    // config_dir is already created by paths::config_dir() or the override path
    std::fs::create_dir_all(config_dir).with_context(|| {
        format!(
            "Failed to create config directory: {}",
            config_dir.display()
        )
    })?;

    let claude_dir = workspace.join(".claude");
    std::fs::create_dir_all(&claude_dir).with_context(|| {
        format!(
            "Failed to create workspace config dir: {}",
            claude_dir.display()
        )
    })?;

    let sessions_dir = workspace.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("Failed to create sessions dir: {}", sessions_dir.display()))?;

    Ok(())
}

/// Write default config.toml from AppConfig::default().
pub fn write_config_toml(path: &std::path::Path) -> Result<()> {
    let config = AppConfig::default();
    let toml_content =
        toml::to_string_pretty(&config).context("Failed to serialize default config to TOML")?;

    let content = format!(
        "# Rustain Configuration\n\
         # Generated by rustain init\n\
         \n\
         {}\n\
         # profile = \"coding\"  # Available in a future release\n",
        toml_content
    );

    std::fs::write(path, content)
        .with_context(|| format!("Failed to write config file: {}", path.display()))?;

    Ok(())
}

/// Write default settings.json (CC-compatible format).
pub fn write_settings_json(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create settings directory: {}", parent.display())
        })?;
    }

    let settings = serde_json::json!({
        "permissions": {
            "allow": []
        }
    });

    let content =
        serde_json::to_string_pretty(&settings).context("Failed to serialize settings to JSON")?;

    std::fs::write(path, content)
        .with_context(|| format!("Failed to write settings file: {}", path.display()))?;

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileWriteStatus {
    Created,
    Replaced,
    Preserved,
}

impl FileWriteStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Replaced => "replaced",
            Self::Preserved => "preserved",
        }
    }
}

fn write_interaction_policy(
    path: &std::path::Path,
    response_mode: crate::domain::models::ResponseMode,
    notification: crate::domain::models::NotificationUrgency,
) -> Result<()> {
    let policy = crate::domain::models::IndividualPolicy {
        defaults: crate::domain::models::IndividualDefaults {
            response_mode: Some(response_mode),
            notification: Some(notification),
            ..Default::default()
        },
        ..Default::default()
    };
    let content = crate::adapters::policy::config::render_individual_policy(&policy)
        .map_err(anyhow::Error::new)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create policy dir: {}", parent.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write interaction policy: {}", path.display()))
}

fn prompt_response_mode<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<crate::domain::models::ResponseMode> {
    writeln!(output)?;
    writeln!(
        output,
        "── Team interaction (A2A) ─────────────────────────────"
    )?;
    writeln!(output, "When another team member's agent messages yours:")?;
    writeln!(output)?;
    writeln!(
        output,
        "  1. notify-and-wait   — show me, wait for my response   (recommended)"
    )?;
    writeln!(
        output,
        "  2. notify-and-draft  — draft a reply for my approval"
    )?;
    writeln!(
        output,
        "  3. notify-and-auto   — reply for me; I always see what was sent"
    )?;
    Ok(match prompt_choice(input, output)? {
        2 => crate::domain::models::ResponseMode::NotifyAndDraft,
        3 => crate::domain::models::ResponseMode::NotifyAndAuto,
        _ => crate::domain::models::ResponseMode::NotifyAndWait,
    })
}

fn prompt_notification_urgency<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<crate::domain::models::NotificationUrgency> {
    writeln!(output)?;
    writeln!(output, "Interruption level for incoming A2A messages:")?;
    writeln!(
        output,
        "  1. queue     — surface at the next idle moment          (recommended)"
    )?;
    writeln!(output, "  2. immediate — interrupt current work")?;
    writeln!(
        output,
        "  3. digest    — batch into a summary every 15 minutes"
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "There is no silent mode. Your log sees everything immediately;"
    )?;
    writeln!(output, "this dial only sets how loudly you're interrupted.")?;
    writeln!(output, "Change anytime: .rustain/a2a-interaction.toml")?;
    Ok(match prompt_choice(input, output)? {
        2 => crate::domain::models::NotificationUrgency::Immediate,
        3 => crate::domain::models::NotificationUrgency::Digest,
        _ => crate::domain::models::NotificationUrgency::Queue,
    })
}

fn prompt_choice<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> Result<u8> {
    loop {
        write!(output, "Selection [1]: ")?;
        output.flush()?;
        let mut line = String::new();
        input.read_line(&mut line)?;
        match line.trim() {
            "" | "1" => return Ok(1),
            "2" => return Ok(2),
            "3" => return Ok(3),
            _ => writeln!(output, "Choose 1, 2, or 3.")?,
        }
    }
}

/// Display completion summary with absolute paths.
fn display_summary<W: Write>(
    config_path: &std::path::Path,
    settings_path: &std::path::Path,
    interaction_policy_path: &std::path::Path,
    interaction_policy_status: FileWriteStatus,
    sessions_path: &std::path::Path,
    api_key_found: bool,
    output: &mut W,
) -> Result<()> {
    let api_status = if api_key_found {
        "\u{2713} Found"
    } else {
        "\u{2717} Not set"
    };

    writeln!(output)?;
    writeln!(output, "rustain init completed!")?;
    writeln!(output)?;
    writeln!(output, "Configuration:")?;
    writeln!(output, "  Global config:    {}", config_path.display())?;
    writeln!(output, "  Workspace config: {}", settings_path.display())?;
    writeln!(
        output,
        "  a2a-interaction.toml: {} ({})",
        interaction_policy_status.as_str(),
        interaction_policy_path.display()
    )?;
    writeln!(output, "  Sessions dir:     {}", sessions_path.display())?;
    writeln!(output)?;
    writeln!(output, "Settings:")?;
    writeln!(output, "  API key:       {api_status}")?;
    writeln!(output, "  Default model: {}", AppConfig::default().model)?;
    writeln!(output)?;
    writeln!(output, "Next steps:")?;
    writeln!(output, "  Run `rustain` to start the TUI application")?;
    writeln!(
        output,
        "  Run `rustain doctor` to verify setup health (coming soon)"
    )?;
    Ok(())
}

/// Prompt the user with a yes/no question.
/// Returns true for 'y'/'Y'/any string starting with y, false otherwise.
fn prompt_yes_no<R: BufRead, W: Write>(
    question: &str,
    input: &mut R,
    output: &mut W,
) -> std::io::Result<bool> {
    write!(output, "{question} [y/n] ")?;
    output.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim().starts_with(['y', 'Y']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_write_config_toml_creates_valid_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        write_config_toml(&config_path).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("# Rustain Configuration"));
        assert!(content.contains("# Generated by rustain init"));
        assert!(content.contains("claude-sonnet-4-6"));
        assert!(content.contains("log_level"));
        assert!(content.contains("log_max_size_mb"));
        assert!(content.contains("log_retain_count"));
        assert!(content.contains("# profile = \"coding\""));

        // Round-trip: parse back to AppConfig
        // Strip comment lines for TOML parsing
        let toml_lines: String = content
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: AppConfig = toml::from_str(&toml_lines).unwrap();
        let default = AppConfig::default();
        assert_eq!(parsed.model, default.model);
        assert_eq!(parsed.log_level, default.log_level);
        assert_eq!(parsed.log_max_size_mb, default.log_max_size_mb);
        assert_eq!(parsed.log_retain_count, default.log_retain_count);
    }

    #[test]
    fn test_write_settings_json_creates_valid_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let settings_path = tmp.path().join(".claude").join("settings.json");

        write_settings_json(&settings_path).unwrap();

        let content = std::fs::read_to_string(&settings_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["permissions"]["allow"], serde_json::json!([]));
    }

    #[test]
    fn test_create_directories() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("config").join("rustain");
        let workspace = tmp.path().join("workspace");

        create_directories(&config_dir, &workspace).unwrap();

        assert!(config_dir.exists());
        assert!(workspace.join(".claude").exists());
        assert!(workspace.join(".claude").join("sessions").exists());
    }

    #[test]
    #[serial]
    fn test_find_api_key_var_prefers_auth_token() {
        // Save originals
        let orig_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok(); // CONFORMANCE_EXCEPTION: test backup/restore
        let orig_key = std::env::var("ANTHROPIC_API_KEY").ok(); // CONFORMANCE_EXCEPTION: test backup/restore

        // SAFETY: Test-only env manipulation. Tests using env vars must run
        // with --test-threads=1 or accept potential flakiness.
        unsafe {
            // Both set — AUTH_TOKEN takes precedence
            std::env::set_var("ANTHROPIC_AUTH_TOKEN", "tok");
            std::env::set_var("ANTHROPIC_API_KEY", "key");
            assert_eq!(find_api_key_var(), Some("ANTHROPIC_AUTH_TOKEN"));

            // Only API_KEY set
            std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
            assert_eq!(find_api_key_var(), Some("ANTHROPIC_API_KEY"));

            // Neither set
            std::env::remove_var("ANTHROPIC_API_KEY");
            assert_eq!(find_api_key_var(), None);

            // Whitespace-only should be treated as absent
            std::env::set_var("ANTHROPIC_API_KEY", "   ");
            assert_eq!(find_api_key_var(), None);

            // Empty string should be treated as absent
            std::env::set_var("ANTHROPIC_API_KEY", "");
            assert_eq!(find_api_key_var(), None);

            // Restore originals
            match orig_token {
                Some(v) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", v),
                None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
            }
            match orig_key {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
        }
    }

    #[test]
    fn test_config_toml_roundtrip() {
        let default = AppConfig::default();
        let toml_str = toml::to_string_pretty(&default).unwrap();
        let parsed: AppConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.model, default.model);
        assert_eq!(parsed.log_level, default.log_level);
        assert_eq!(parsed.log_max_size_mb, default.log_max_size_mb);
        assert_eq!(parsed.log_retain_count, default.log_retain_count);
    }

    /// AC4 guard: `find_api_key_var` reads only environment variables — no network I/O.
    /// `detect_api_key` delegates to `find_api_key_var` + `prompt_yes_no` (stdin only).
    /// `run_init_with_paths` calls `detect_api_key`, `create_directories`, `write_config_toml`,
    /// `write_settings_json`, and `display_summary` — all pure file/env/stdio operations.
    /// Therefore `run_init` completes with no network by construction.
    /// This test exercises `find_api_key_var` with no API keys set, confirming it returns
    /// `None` without any network call, satisfying AC4's offline guard.
    #[test]
    // Shares the `ANTHROPIC_*` process-global env with
    // `test_find_api_key_var_prefers_auth_token`, which was already
    // `#[serial]`. Without the pair being serialized, whichever ran first
    // cleared the other's fixture — a pre-existing race the test itself
    // documented as "accept potential flakiness". `#[serial]` removes it
    // instead of accepting it.
    #[serial]
    fn test_init_completes_offline_guard() {
        // Save originals
        let orig_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok(); // CONFORMANCE_EXCEPTION: test backup/restore
        let orig_key = std::env::var("ANTHROPIC_API_KEY").ok(); // CONFORMANCE_EXCEPTION: test backup/restore

        // SAFETY: Test-only env manipulation, serialized against its twin above.
        unsafe {
            std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        // find_api_key_var is the network-relevant function in the init path.
        // It only reads env vars — no reqwest, no HTTP, no DNS.
        assert_eq!(
            find_api_key_var(),
            None,
            "AC4: find_api_key_var must not require network"
        );

        // Also verify the pure file-writing helpers complete offline.
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("workspace");
        create_directories(&config_dir, &workspace).unwrap();
        write_config_toml(&config_dir.join("config.toml")).unwrap();
        write_settings_json(&workspace.join(".claude").join("settings.json")).unwrap();

        // Restore originals
        unsafe {
            match orig_token {
                Some(v) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", v),
                None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
            }
            match orig_key {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
        }
    }
    #[test]
    fn interaction_policy_defaults_round_trip_and_existing_file_is_preserved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("workspace");
        let mut input = std::io::Cursor::new(b"\n\n".to_vec());
        let mut output = Vec::new();

        run_init_with_io(
            Some(config_dir.clone()),
            Some(workspace.clone()),
            true,
            None,
            &mut input,
            &mut output,
        )
        .unwrap();

        let policy_path = workspace.join(".rustain").join("a2a-interaction.toml");
        let original = std::fs::read(&policy_path).unwrap();
        let loaded = crate::adapters::policy::load_workspace_policies(&workspace).unwrap();
        assert_eq!(
            loaded.individual.defaults.response_mode,
            Some(crate::domain::models::ResponseMode::NotifyAndWait)
        );
        assert_eq!(
            loaded.individual.defaults.notification,
            Some(crate::domain::models::NotificationUrgency::Queue)
        );
        let first_output = String::from_utf8(output).unwrap();
        assert!(first_output.contains("There is no silent mode."));
        assert!(first_output.contains("a2a-interaction.toml: created"));

        let mut input = std::io::Cursor::new(b"n\n".to_vec());
        let mut output = Vec::new();
        run_init_with_io(
            Some(config_dir),
            Some(workspace),
            true,
            None,
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(std::fs::read(policy_path).unwrap(), original);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("a2a-interaction.toml: preserved")
        );
    }

    #[test]
    fn confirmed_policy_overwrite_replaces_with_selected_values() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("workspace");
        let mut input = std::io::Cursor::new(b"\n\n".to_vec());
        let mut output = Vec::new();
        run_init_with_io(
            Some(config_dir.clone()),
            Some(workspace.clone()),
            true,
            None,
            &mut input,
            &mut output,
        )
        .unwrap();

        let mut input = std::io::Cursor::new(b"y\n3\n2\n".to_vec());
        let mut output = Vec::new();
        run_init_with_io(
            Some(config_dir),
            Some(workspace.clone()),
            true,
            None,
            &mut input,
            &mut output,
        )
        .unwrap();

        let loaded = crate::adapters::policy::load_workspace_policies(&workspace).unwrap();
        assert_eq!(
            loaded.individual.defaults.response_mode,
            Some(crate::domain::models::ResponseMode::NotifyAndAuto)
        );
        assert_eq!(
            loaded.individual.defaults.notification,
            Some(crate::domain::models::NotificationUrgency::Immediate)
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("a2a-interaction.toml: replaced")
        );
    }

    #[test]
    fn non_tty_init_still_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("workspace");
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let error = run_init_with_io(
            Some(config_dir.clone()),
            Some(workspace.clone()),
            false,
            None,
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(error.to_string().contains("interactive terminal"));
        assert!(!config_dir.exists());
        assert!(!workspace.exists());
        assert!(output.is_empty());
    }
}
