//! `rustain profile list` — enumerates all available profiles with source + active marker.
//! Story 8.6a AC-1, FR71.

use std::sync::Arc;

use anyhow::Result;

use super::prompt::source_label;
use crate::adapters::cli::commands::Cli;
use crate::domain::models::{AppConfig, ProfileSource};
use crate::domain::ports::ProfileResolver;
use crate::infrastructure::profile_resolution::effective_profile_name;

pub async fn run_profile_list(
    json: bool,
    profile_resolver: &Arc<dyn ProfileResolver>,
    cli: &Cli,
    bootstrap_config: &AppConfig,
) -> Result<()> {
    let profiles = profile_resolver.list_profiles();
    let active_name = effective_profile_name(cli, bootstrap_config);

    if json {
        let items: Vec<serde_json::Value> = profiles
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "source": source_label(p.source),
                    "source_origin": p.source_origin,
                    "preview": p.preview,
                    "description": p.description.as_deref().unwrap_or(""),
                    "identity_color": p.identity_color.0,
                    "active": p.name == active_name,
                })
            })
            .collect();
        let json_str = serde_json::to_string_pretty(&items)?;
        println!("{}", json_str);
    } else {
        // Print header
        println!(
            "{:<2} {:<25} {:<40} {:<10} {:<60}",
            "", "NAME", "SOURCE", "PREVIEW", "DESCRIPTION"
        );

        for p in &profiles {
            let active_marker = if p.name == active_name { "*" } else { " " };
            let preview_str = if p.preview { "(preview)" } else { "" };
            let source_str = match p.source {
                ProfileSource::Community => {
                    if let Some(ref origin) = p.source_origin {
                        format!("community ({})", origin)
                    } else {
                        source_label(p.source).to_string()
                    }
                }
                _ => source_label(p.source).to_string(),
            };
            let source_trunc = truncate_str(&source_str, 40);
            let desc = p.description.as_deref().unwrap_or("");
            let desc_trunc = truncate_str(desc, 60);
            println!(
                "{:<2} {:<25} {:<40} {:<10} {:<60}",
                active_marker, p.name, source_trunc, preview_str, desc_trunc
            );
        }
    }

    tracing::info!(subcommand = "profile-list", profile_count = profiles.len());
    Ok(())
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 1).collect();
        format!("{}\u{2026}", truncated) // … ellipsis
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_label_builtin() {
        assert_eq!(source_label(ProfileSource::Builtin), "builtin");
    }

    #[test]
    fn test_source_label_user() {
        assert_eq!(source_label(ProfileSource::User), "user");
    }

    #[test]
    fn test_source_label_community() {
        assert_eq!(source_label(ProfileSource::Community), "community");
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate_str("hello", 60), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        let long = "a".repeat(70);
        let result = truncate_str(&long, 60);
        assert_eq!(result.chars().count(), 60); // 59 chars + ellipsis = 60
        assert!(result.ends_with('\u{2026}'));
    }
}
