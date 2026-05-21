//! `rustain profile create` interactive wizard.
//! Story 8.6a AC-3, FR63, FR71.
//!
//! 5-phase wizard following `rustain init` patterns:
//! Phase 1 — Profile identity (name, description, extends)
//! Phase 2 — Per-port adapter selection (7 ports in canonical order)
//! Phase 3 — Optional overrides (key = value sub-loop)
//! Phase 4 — Preview + confirm
//! Phase 5 — Save + validate

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::Result;

use super::prompt::{
    escape_toml_string, format_toml_value, prompt_line, prompt_required_line, prompt_yes_no,
    validate_profile_name,
};
use crate::adapters::cli::commands::Cli;
use crate::domain::models::{AppConfig, PortDimension};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::identity_color;
use crate::infrastructure::paths;

pub async fn run_profile_create(
    name: Option<String>,
    extends: Option<String>,
    from: Option<String>,
    _profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    // TTY guard FIRST
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("rustain profile create requires an interactive terminal.");
    }

    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");
    std::fs::create_dir_all(&profiles_dir)?;

    // Load source profile for --from flag
    let from_resolved = if let Some(ref from_name) = from {
        match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            from_name,
            profiles_dir.clone(),
        ) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!(
                    "Warning: could not load source profile '{}': {}",
                    from_name, e
                );
                None
            }
        }
    } else {
        None
    };

    println!("rustain profile create\n");
    println!("This wizard will help you create a new agent profile.");
    println!("Enter a blank line at any prompt to go back (except required fields).\n");

    // ── Phase 1: Profile identity ──

    let profile_name = if let Some(n) = name.clone() {
        validate_name(&n).map_err(|e| anyhow::anyhow!(e))?;
        n
    } else {
        prompt_required_line("Profile name: ", |s| validate_name(s))?
    };

    let description = prompt_line("Description (optional): ")?;
    let description_opt = if description.is_empty() {
        None
    } else {
        if description.chars().count() > 200 {
            eprintln!(
                "Description too long ({} chars). Truncated to 200.",
                description.chars().count()
            );
        }
        Some(description.chars().take(200).collect())
    };

    // Show available parents for extends
    if extends.is_none() {
        println!();
        println!("Available parents:");
        let resolver = crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            "coding",
            profiles_dir.clone(),
        );
        if let Ok(r) = resolver {
            for p in r.list_profiles() {
                println!("  {}", p.name);
            }
        }
        println!();
    }

    let extends_val = if let Some(e) = extends {
        Some(e)
    } else {
        let input = prompt_line("Extends (optional, leave blank for none): ")?;
        if input.is_empty() { None } else { Some(input) }
    };

    // ── Phase 2: Per-port adapter selection ──

    println!();
    let mut persona: Option<String> = None;
    let mut memory: Option<String> = None;
    let mut session: Option<String> = None;
    let mut tools: Option<String> = None;
    let mut channels: Option<String> = None;
    let mut scheduler: Option<String> = None;
    let mut context: Option<String> = None;
    let mut preview = false;

    let ports: &mut [(&str, &mut Option<String>)] = &mut [
        ("persona", &mut persona),
        ("memory", &mut memory),
        ("session", &mut session),
        ("tools", &mut tools),
        ("channels", &mut channels),
        ("scheduler", &mut scheduler),
        ("context", &mut context),
    ];

    for (port_name, slot) in ports.iter_mut() {
        let port_dim = port_name_to_dimension(port_name);
        let available = AdapterCatalog::known_for(port_dim);
        println!("Available {} adapters: {}", port_name, available.join(", "));

        // Default from --from flag
        let default_adapter = from_resolved.as_ref().and_then(|r| {
            r.resolve_active().and_then(|resolved| {
                resolved
                    .selection
                    .dimensions
                    .get(&port_dim)
                    .map(|ar| ar.adapter.clone())
            })
        });
        let default_hint = default_adapter
            .as_deref()
            .map(|d| format!(" [default: {}]", d))
            .unwrap_or_default();

        loop {
            let input = prompt_line(&format!("{} adapter{}: ", port_name, default_hint))?;
            let adapter_name = if input.is_empty() {
                if let Some(ref def) = default_adapter {
                    def.clone()
                } else {
                    eprintln!(
                        "{} adapter is required; type one of: {}",
                        port_name,
                        available.join(", ")
                    );
                    continue;
                }
            } else {
                input
            };

            // Validate against AdapterCatalog
            if AdapterCatalog::lookup(port_dim, &adapter_name).is_none() {
                let suggestion = crate::domain::services::profile_loader::closest_match(
                    &adapter_name,
                    &available,
                    1,
                );
                if let Some(sug) = suggestion {
                    eprintln!(
                        "Unknown adapter '{}'. Did you mean: '{}'?",
                        adapter_name, sug
                    );
                } else {
                    eprintln!(
                        "Unknown adapter '{}'. Available: {}",
                        adapter_name,
                        available.join(", ")
                    );
                }
                continue;
            }

            // Feature-gate check
            let desc = AdapterCatalog::lookup(port_dim, &adapter_name).unwrap();
            if let Some(feature) = desc.feature_gate {
                if !AdapterCatalog::is_feature_compiled(feature) {
                    if prompt_yes_no(&format!(
                        "Adapter '{}' requires the '{}' cargo feature, which is NOT compiled into this binary. Mark profile as preview=true (recommended for sharing)?",
                        adapter_name, feature
                    ))? {
                        preview = true;
                    } else {
                        eprintln!("Please choose a different adapter.");
                        continue;
                    }
                }
            }

            **slot = Some(adapter_name);
            break;
        }
    }

    // ── Phase 3: Optional overrides ──

    println!();
    println!("Common overrides:");
    println!("  default_plan_mode = false      # Disable plan mode prompt at startup");
    println!("  model = \"claude-opus-4-7\"      # Override default model");
    println!("  log_level = \"debug\"            # Override log verbosity");
    println!();

    let mut overrides_toml = String::new();
    if prompt_yes_no("Add overrides? (e.g., default_plan_mode, model)")? {
        println!("Enter key = value pairs (one per line). Empty line to finish:");
        loop {
            let line = prompt_line("> ")?;
            if line.is_empty() {
                break;
            }
            // Parse key = value
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();
                // Emit valid TOML scalar
                if value.is_empty() {
                    eprintln!("Skipping empty value for key '{}'", key);
                    continue;
                }
                // Try to parse as TOML scalar
                let toml_line = format_toml_scalar(key, value);
                overrides_toml.push_str(&toml_line);
                overrides_toml.push('\n');
            } else {
                eprintln!("Invalid format. Use: key = value");
            }
        }
    }

    // ── Phase 4: Preview + confirm ──

    println!();
    println!("Profile preview:");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━");

    // Build the TOML for preview
    let toml_content = build_profile_toml(
        &profile_name,
        &description_opt,
        &extends_val,
        preview,
        &persona,
        &memory,
        &session,
        &tools,
        &channels,
        &scheduler,
        &context,
        &overrides_toml,
    );
    println!("{}", toml_content);

    let color = identity_color::derive_identity_color(&profile_name, None);
    println!("Identity color: {}", color.0);
    println!();

    if !prompt_yes_no(&format!(
        "Save to {}?",
        profiles_dir
            .join(format!("{}.toml", profile_name))
            .display()
    ))? {
        println!("Profile creation cancelled. No changes made.");
        return Ok(());
    }

    // ── Phase 5: Save ──

    let dest = profiles_dir.join(format!("{}.toml", profile_name));

    // Overwrite check
    if dest.exists() {
        if !prompt_yes_no(&format!(
            "Profile '{}' already exists at {}. Overwrite?",
            profile_name,
            dest.display()
        ))? {
            println!("Profile creation cancelled. Existing profile preserved.");
            return Ok(());
        }
    }

    std::fs::write(&dest, &toml_content)?;

    // Validate via round-trip
    match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
        &profile_name,
        profiles_dir.clone(),
    ) {
        Ok(_) => {
            println!(
                "Profile '{}' created at {}. Activate now? Run: rustain --profile {}",
                profile_name,
                dest.display(),
                profile_name
            );
        }
        Err(e) => {
            eprintln!("Validation warning: {}", e);
            if prompt_yes_no("Save anyway as draft?")? {
                println!(
                    "Profile '{}' saved as draft at {}. Edit with: rustain profile edit {}",
                    profile_name,
                    dest.display(),
                    profile_name
                );
            } else {
                std::fs::remove_file(&dest)?;
                println!("Profile creation cancelled. Draft removed.");
            }
        }
    }

    tracing::info!(subcommand = "profile-create", profile = %profile_name);
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    validate_profile_name(name)
}

fn port_name_to_dimension(name: &str) -> PortDimension {
    match name {
        "persona" => PortDimension::Persona,
        "memory" => PortDimension::Memory,
        "session" => PortDimension::Session,
        "tools" => PortDimension::Tools,
        "channels" => PortDimension::Channels,
        "scheduler" => PortDimension::Scheduler,
        "context" => PortDimension::Context,
        _ => PortDimension::Persona, // unreachable
    }
}

fn format_toml_scalar(key: &str, value: &str) -> String {
    format_toml_value(key, value)
}

fn build_profile_toml(
    name: &str,
    description: &Option<String>,
    extends: &Option<String>,
    preview: bool,
    persona: &Option<String>,
    memory: &Option<String>,
    session: &Option<String>,
    tools: &Option<String>,
    channels: &Option<String>,
    scheduler: &Option<String>,
    context: &Option<String>,
    overrides_toml: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("name = \"{}\"\n", escape_toml_string(name)));

    if let Some(desc) = description {
        out.push_str(&format!("description = \"{}\"\n", escape_toml_string(desc)));
    }
    if let Some(ext) = extends {
        out.push_str(&format!("extends = \"{}\"\n", escape_toml_string(ext)));
    }
    if preview {
        out.push_str("preview = true\n");
    }

    let ports: &[(&str, &Option<String>)] = &[
        ("persona", persona),
        ("memory", memory),
        ("session", session),
        ("tools", tools),
        ("channels", channels),
        ("scheduler", scheduler),
        ("context", context),
    ];

    for (port_name, adapter) in ports {
        if let Some(adapter_name) = adapter {
            out.push_str(&format!(
                "\n[{0}]\nadapter = \"{1}\"\n",
                port_name,
                escape_toml_string(adapter_name)
            ));
        }
    }

    if !overrides_toml.is_empty() {
        out.push_str("\n[overrides]\n");
        out.push_str(overrides_toml);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn test_validate_name_rejects_path_traversal() {
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("foo/bar").is_err());
    }

    #[test]
    fn test_validate_name_accepts_valid() {
        assert!(validate_name("my-profile").is_ok());
        assert!(validate_name("test_123").is_ok());
        assert!(validate_name("abcdef").is_ok());
    }

    #[test]
    fn test_validate_name_rejects_special_chars() {
        assert!(validate_name("hello world").is_err());
        assert!(validate_name("foo@bar").is_err());
    }

    #[test]
    fn test_format_toml_scalar_bool_true() {
        assert_eq!(format_toml_scalar("flag", "true"), "flag = true");
    }

    #[test]
    fn test_format_toml_scalar_bool_false() {
        assert_eq!(format_toml_scalar("flag", "false"), "flag = false");
    }

    #[test]
    fn test_format_toml_scalar_integer() {
        assert_eq!(format_toml_scalar("count", "42"), "count = 42");
    }

    #[test]
    fn test_format_toml_scalar_string() {
        assert_eq!(
            format_toml_scalar("model", "claude-sonnet-4-6"),
            "model = \"claude-sonnet-4-6\""
        );
    }

    #[test]
    fn test_format_toml_scalar_escapes_quotes() {
        assert_eq!(
            format_toml_scalar("desc", r#"my "cool" thing"#),
            r#"desc = "my \"cool\" thing""#
        );
    }

    #[test]
    fn test_build_profile_toml_round_trip() {
        let toml = build_profile_toml(
            "test-profile",
            &Some("A test profile".into()),
            &Some("base".into()),
            true,
            &Some("coding".into()),
            &Some("project-scoped".into()),
            &Some("workspace".into()),
            &Some("builtin-full".into()),
            &Some("terminal".into()),
            &Some("none".into()),
            &Some("default".into()),
            "default_plan_mode = false\n",
        );
        assert!(toml.contains("name = \"test-profile\""));
        assert!(toml.contains("extends = \"base\""));
        assert!(toml.contains("preview = true"));
        assert!(toml.contains("[persona]"));
        assert!(toml.contains("adapter = \"coding\""));
        assert!(toml.contains("[overrides]"));
        assert!(toml.contains("default_plan_mode = false"));
    }
}
