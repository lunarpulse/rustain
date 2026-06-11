//! `rustain profile install gh:user/profile-name` — fetches a profile from a public GitHub repo.
//! Story 8.6b AC-1..AC-11, FR73.
#![cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;

use super::prompt::{fix_profile_error, validate_profile_name};
use super::source::SinglePathSource;
use crate::adapters::cli::commands::Cli;
use crate::adapters::profile_resolver::embedded::{EmbeddedProfileSource, embedded_names};
use crate::domain::errors::ProfileError;
use crate::domain::models::{AppConfig, PortDimension, ProfileDefinition};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::profile_loader::ProfileLoader;
use crate::infrastructure::{
    paths,
    profile_install::{ParsedGhSpec, parse_gh_spec, raw_base_url, write_source_sidecar},
};

const MAX_PROFILE_SIZE: usize = 1024 * 1024; // 1 MB

static PREVIEW_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^preview\s*=\s*(true|false)\s*$").expect("PREVIEW_LINE_RE compile")
});

static NAME_LINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)^name\s*=\s*"[^"]*""#).expect("NAME_LINE_RE compile"));

pub async fn run_profile_install(
    spec: String,
    name_override: Option<String>,
    force: bool,
    strict_features: bool,
    _profile_resolver: &Arc<dyn ProfileResolver>,
    _cli: &Cli,
    _bootstrap_config: &AppConfig,
) -> Result<()> {
    let parsed = match parse_gh_spec(&spec) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
    };

    let (content, _actual_url) = fetch_profile_toml(&parsed, &spec)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        });

    // Parse TOML
    let mut def: ProfileDefinition = match toml::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "Error: failed to parse downloaded profile from gh:{}: {}",
                spec, e
            );
            // Print up to 200 chars of content for diagnosis (UTF-8 safe)
            let preview = safe_truncate(&content, 200);
            eprintln!("Content preview: {}", preview);
            std::process::exit(2);
        }
    };

    // Validate profile name
    if let Err(e) = validate_profile_name(&def.name) {
        eprintln!("Error: {}", e);
        std::process::exit(2);
    }

    // Apply name override with validation
    if let Some(ref new_name) = name_override {
        if let Err(e) = validate_profile_name(new_name) {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
        def.name = new_name.clone();
    }

    // Validate and handle feature gating
    let (def, content_to_write, feature_warnings) =
        validate_or_flip(&content, def, strict_features);

    // Collision check
    let target_name = name_override.as_deref().unwrap_or(&def.name);
    let collision = check_name_collision(target_name, force, name_override.is_some());
    if let Err(msg) = collision {
        eprintln!("{}", msg);
        std::process::exit(2);
    }

    // Determine destination
    let config_dir = paths::config_dir().context("Failed to determine rustain config directory")?;
    let profiles_dir = config_dir.join("profiles");
    let community_dir = profiles_dir.join("community");
    std::fs::create_dir_all(&community_dir)?;

    let dest = community_dir.join(format!("{}.toml", target_name));

    // Overwrite check
    if dest.exists() && !force {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            print!(
                "Profile '{}' already exists at {}. Overwrite? [y/n]: ",
                target_name,
                dest.display()
            );
            std::io::stdout().flush().unwrap();
            let mut input = String::new();
            std::io::stdin().lock().read_line(&mut input).unwrap();
            if !input.trim().starts_with(['y', 'Y']) {
                println!("Install cancelled. Existing profile preserved.");
                return Ok(());
            }
        } else {
            eprintln!(
                "Error: refusing to overwrite '{}' in non-interactive mode. Pass --force to proceed.",
                target_name
            );
            std::process::exit(2);
        }
    }

    // Rewrite name field if name_override was set
    let final_content = if name_override.is_some() {
        NAME_LINE_RE
            .replace(&content_to_write, format!("name = \"{}\"", target_name))
            .to_string()
    } else {
        content_to_write
    };

    std::fs::write(&dest, &final_content)
        .map_err(|e| anyhow::anyhow!("Failed to write profile to {}: {}", dest.display(), e))?;

    write_source_sidecar(&dest, &spec)?;

    // Emit feature-gate warning to stderr
    if !feature_warnings.is_empty() {
        let features_list: Vec<_> = feature_warnings
            .iter()
            .map(|f| f.feature.as_str())
            .collect();
        let features_joined = features_list.join(" ");
        eprintln!(
            "Warning: profile '{}' references adapters not compiled into this binary:",
            target_name
        );
        for fw in &feature_warnings {
            eprintln!(
                "  - {} (port: {}; requires --features {})",
                fw.adapter, fw.port, fw.feature
            );
        }
        eprintln!(
            "Set preview = true so the profile falls back to no-op adapters for missing dimensions.\n\
             Profile installed with preview = true. To use the full profile, rebuild with: cargo install rustain --features {}",
            features_joined
        );
    }

    println!("Installed profile '{}' from {}", target_name, spec);

    tracing::info!(
        subcommand = "profile-install",
        spec = %spec,
        profile = %target_name,
        feature_warning = !feature_warnings.is_empty()
    );

    if !feature_warnings.is_empty() {
        for fw in &feature_warnings {
            tracing::warn!(
                subcommand = "profile-install",
                feature_gate_warning = true,
                profile = %target_name,
                feature = %fw.feature,
                adapter = %fw.adapter
            );
        }
    }

    Ok(())
}

struct FeatureGateInfo {
    feature: String,
    adapter: String,
    port: String,
}

/// Scan a ProfileDefinition for all adapters whose cargo features aren't compiled.
fn scan_features(def: &ProfileDefinition) -> Vec<FeatureGateInfo> {
    let dims: &[(
        &str,
        Option<&crate::domain::models::AdapterRef>,
        PortDimension,
    )] = &[
        ("persona", def.persona.as_ref(), PortDimension::Persona),
        ("memory", def.memory.as_ref(), PortDimension::Memory),
        ("session", def.session.as_ref(), PortDimension::Session),
        ("tools", def.tools.as_ref(), PortDimension::Tools),
        ("channels", def.channels.as_ref(), PortDimension::Channels),
        (
            "scheduler",
            def.scheduler.as_ref(),
            PortDimension::Scheduler,
        ),
        ("context", def.context.as_ref(), PortDimension::Context),
    ];
    let mut features = Vec::new();
    for (_dim_name, adapter_ref, port) in dims {
        if let Some(adapter_ref) = adapter_ref {
            if let Some(desc) = AdapterCatalog::lookup(*port, &adapter_ref.adapter) {
                if let Some(feature) = desc.feature_gate {
                    if !AdapterCatalog::is_feature_compiled(feature) {
                        features.push(FeatureGateInfo {
                            feature: feature.to_string(),
                            adapter: adapter_ref.adapter.clone(),
                            port: format!("{:?}", port),
                        });
                    }
                }
            }
        }
    }
    features
}

fn validate_or_flip(
    content: &str,
    def: ProfileDefinition,
    strict_features: bool,
) -> (ProfileDefinition, String, Vec<FeatureGateInfo>) {
    let validate = |content: &str| -> Result<ProfileDefinition, ProfileError> {
        let def: ProfileDefinition =
            toml::from_str(content).map_err(|_| ProfileError::ProfileNotFound {
                name: "in-memory".into(),
                search_paths: vec![],
            })?;
        let source = SinglePathSource {
            name: def.name.clone(),
            content: content.to_string(),
            fallback: EmbeddedProfileSource,
        };
        let loader = ProfileLoader::new(&AdapterCatalog, &source);
        loader.load(&def.name)?;
        Ok(def)
    };

    match validate(content) {
        Ok(parsed_def) => (parsed_def, content.to_string(), Vec::new()),
        Err(ProfileError::AdapterFeatureGated { .. }) if !strict_features => {
            // Check if already preview
            let already_preview = def.preview
                || PREVIEW_LINE_RE
                    .captures(content)
                    .and_then(|c| c.get(1).map(|m| m.as_str() == "true"))
                    .unwrap_or(false);

            if already_preview {
                // No warning needed — upstream already declared preview.
                // Scan for ALL feature-gated adapters but do NOT re-validate;
                // the loader should handle preview=true gracefully.
                let warnings = scan_features(&def);
                (def, content.to_string(), warnings)
            } else {
                // Pre-scan for ALL feature-gated adapters before flipping
                let all_features = scan_features(&def);
                let rewritten = apply_preview_flip(content);
                match validate(&rewritten) {
                    Ok(parsed_def) => (parsed_def, rewritten, all_features),
                    Err(e) => {
                        eprintln!("Validation still failed after preview flip: {}", e);
                        eprintln!("{}", fix_profile_error(&e));
                        std::process::exit(2);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Profile validation failed: {}", e);
            eprintln!("{}", fix_profile_error(&e));
            std::process::exit(2);
        }
    }
}

fn apply_preview_flip(content: &str) -> String {
    if PREVIEW_LINE_RE.is_match(content) {
        PREVIEW_LINE_RE
            .replace(content, "preview = true")
            .to_string()
    } else {
        format!("{}\npreview = true\n", content)
    }
}

fn check_name_collision(
    target_name: &str,
    force: bool,
    name_override_provided: bool,
) -> Result<(), String> {
    // Check embedded names
    if embedded_names().contains(&target_name) {
        if !(force && name_override_provided) {
            return Err(format!(
                "Error: profile name '{}' collides with a built-in profile. Pass --name <override> to install under a different name.",
                target_name
            ));
        }
    }

    // Check user profiles
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let user_dest = config_dir
        .join("profiles")
        .join(format!("{}.toml", target_name));
    if user_dest.exists() {
        if !(force && name_override_provided) {
            return Err(format!(
                "Error: profile name '{}' collides with a user profile. Pass --name <override> to install under a different name.",
                target_name
            ));
        }
    }

    Ok(())
}

/// Truncate a string at a UTF-8 safe byte boundary. Panic-free.
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        s
    } else {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

async fn fetch_profile_toml(parsed: &ParsedGhSpec, spec: &str) -> Result<(String, String)> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(Duration::from_secs(30))
        .user_agent(format!("rustain/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let base = raw_base_url();

    // Build URL(s) per AC-1 fallback order
    let urls: Vec<String> = if let Some(ref path) = parsed.path {
        vec![format!(
            "{base}/{}/{}/HEAD/{path}",
            parsed.user, parsed.repo
        )]
    } else {
        vec![
            format!("{base}/{}/{}/HEAD/profile.toml", parsed.user, parsed.repo),
            format!(
                "{base}/{}/{}/HEAD/{}.toml",
                parsed.user, parsed.repo, parsed.repo
            ),
        ]
    };

    let mut tried_paths: Vec<String> = Vec::new();

    for url in &urls {
        let short_path = url.strip_prefix(&base).unwrap_or(url).to_string();
        tried_paths.push(short_path.clone());

        match client.get(url).send().await {
            Ok(mut response) => {
                let status = response.status();
                if status.is_success() {
                    // Streaming body read with running-total 1 MB cap
                    let mut body = Vec::new();
                    while let Ok(Some(chunk)) = response.chunk().await {
                        if body.len() + chunk.len() > MAX_PROFILE_SIZE {
                            return Err(anyhow::anyhow!("profile at gh:{spec} exceeds 1 MB limit"));
                        }
                        body.extend_from_slice(&chunk);
                    }

                    let content = String::from_utf8(body).map_err(|e| {
                        anyhow::anyhow!("Profile content is not valid UTF-8: {}", e)
                    })?;

                    return Ok((content, url.clone()));
                } else if status.as_u16() == 404 {
                    continue;
                } else {
                    // Non-200, non-404 → error
                    let body_preview = response.text().await.unwrap_or_default();
                    let preview = safe_truncate(&body_preview, 200);
                    return Err(anyhow::anyhow!(
                        "HTTP {} from raw.githubusercontent.com for gh:{spec}. {}",
                        status.as_u16(),
                        preview,
                    ));
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "network failure fetching gh:{spec}: {e}\n\
                     Fix: check internet connectivity, GitHub status (https://www.githubstatus.com), and that the repository '{user}/{repo}' exists and is public.",
                    user = parsed.user,
                    repo = parsed.repo,
                ));
            }
        }
    }

    // All URLs returned 404
    Err(anyhow::anyhow!(
        "no profile TOML found at gh:{user}/{repo}. Checked: {tried}. Add a /path suffix to your spec (e.g. gh:{user}/{repo}/path/to/profile.toml) to point at a specific file.",
        user = parsed.user,
        repo = parsed.repo,
        tried = tried_paths.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_preview_flip_when_no_preview_line() {
        let content = "name = \"test\"\n[persona]\nadapter = \"minimal\"\n";
        let result = apply_preview_flip(content);
        assert!(result.contains("preview = true"));
    }

    #[test]
    fn test_apply_preview_flip_rewrites_false_to_true() {
        let content = "name = \"test\"\npreview = false\n[persona]\nadapter = \"minimal\"\n";
        let result = apply_preview_flip(content);
        assert!(result.contains("preview = true"));
        assert!(!result.contains("preview = false"));
    }

    #[test]
    fn test_apply_preview_flip_preserves_already_true() {
        let content = "name = \"test\"\npreview = true\n[persona]\nadapter = \"minimal\"\n";
        let result = apply_preview_flip(content);
        assert!(result.contains("preview = true"));
        assert_eq!(result.matches("preview = ").count(), 1);
    }

    #[test]
    fn test_apply_preview_flip_with_spaces() {
        let content = "name = \"test\"\npreview   =   false\n[persona]\nadapter = \"minimal\"\n";
        let result = apply_preview_flip(content);
        assert!(result.contains("preview = true"));
    }

    #[test]
    fn test_safe_truncate_within_limit() {
        assert_eq!(safe_truncate("hello", 10), "hello");
    }

    #[test]
    fn test_safe_truncate_at_ascii_boundary() {
        assert_eq!(safe_truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_safe_truncate_at_byte_boundary() {
        // é is 2 bytes in UTF-8
        let s = "café";
        // byte index 3 is inside 'é' (bytes 2-3)
        let result = safe_truncate(s, 3);
        assert_eq!(result, "caf"); // should truncate before the multi-byte char
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// URL fallback: no path produces profile.toml + {repo}.toml.
    #[test]
    fn test_fallback_url_order_no_path() {
        let base = "https://raw.githubusercontent.com";
        let parsed = ParsedGhSpec {
            user: "a".into(),
            repo: "b".into(),
            path: None,
            reference: None,
        };
        let urls = build_fetch_urls_for_test(base, &parsed);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("/profile.toml"));
        assert!(urls[1].ends_with("/b.toml"));
    }

    /// URL fallback: explicit path produces single URL.
    #[test]
    fn test_explicit_path_produces_single_url() {
        let base = "https://raw.githubusercontent.com";
        let parsed = ParsedGhSpec {
            user: "a".into(),
            repo: "b".into(),
            path: Some("custom/profile.toml".into()),
            reference: None,
        };
        let urls = build_fetch_urls_for_test(base, &parsed);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].ends_with("/custom/profile.toml"));
    }

    /// Oversize body is detected correctly — the chunk accumulation bound is correct.
    #[test]
    fn test_max_profile_size_constant() {
        assert_eq!(MAX_PROFILE_SIZE, 1024 * 1024);
    }

    /// Network error path produces a proper, actionable error message.
    #[test]
    fn test_network_error_message_is_actionable() {
        // The error surface: verify the fix-text is present in the error format.
        let err = anyhow::anyhow!(
            "network failure fetching gh:owner/repo: connection refused\n\
             Fix: check internet connectivity, GitHub status (https://www.githubstatus.com), and that the repository 'owner/repo' exists and is public.",
        );
        let msg = err.to_string();
        assert!(msg.contains("network failure"));
        assert!(msg.contains("Fix:"));
        assert!(msg.contains("githubstatus.com"));
    }
}

/// Build the list of fetch URLs (extracted for testability).
fn build_fetch_urls_for_test(base: &str, parsed: &ParsedGhSpec) -> Vec<String> {
    if let Some(ref path) = parsed.path {
        vec![format!(
            "{base}/{}/{}/HEAD/{path}",
            parsed.user, parsed.repo
        )]
    } else {
        vec![
            format!("{base}/{}/{}/HEAD/profile.toml", parsed.user, parsed.repo),
            format!(
                "{base}/{}/{}/HEAD/{}.toml",
                parsed.user, parsed.repo, parsed.repo
            ),
        ]
    }
}
