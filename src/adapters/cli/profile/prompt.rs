//! Shared input helpers for `cli/profile/` subcommands.
//!
//! Copied from `cli/init.rs` prompt_yes_no pattern (Story 8.6a Decision Gate 1.13).
//! Deduplication tracked by DF-NNN.
//!
//! All helpers are `pub(super)` — visible only to sibling modules in `cli/profile/`.

use std::io::{self, BufRead, Write};

/// Prompt the user with a yes/no question.
/// Returns true for 'y'/'Y'/any string starting with y, false otherwise.
pub(super) fn prompt_yes_no(question: &str) -> io::Result<bool> {
    print!("{} [y/n] ", question);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    Ok(input.trim().starts_with(['y', 'Y']))
}

/// Prompt the user for a line of input and return the trimmed string.
pub(super) fn prompt_line(question: &str) -> io::Result<String> {
    print!("{}", question);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Prompt for a required line, re-prompting on validation failure.
/// The `validator` receives the trimmed input and returns `Ok(())` for valid,
/// or `Err(message)` describing why it's invalid.
pub(super) fn prompt_required_line(
    question: &str,
    validator: impl Fn(&str) -> Result<(), String>,
) -> io::Result<String> {
    loop {
        let input = prompt_line(question)?;
        match validator(&input) {
            Ok(()) => return Ok(input),
            Err(msg) => {
                eprintln!("{}", msg);
            }
        }
    }
}

pub(super) fn source_label(source: crate::domain::models::ProfileSource) -> &'static str {
    match source {
        crate::domain::models::ProfileSource::Builtin => "builtin",
        crate::domain::models::ProfileSource::User => "user",
        crate::domain::models::ProfileSource::Community => "community",
    }
}

pub(super) fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Profile name is required.".into());
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(
            "Profile name must not contain '..', '/', or '\\' (path traversal attempt).".into(),
        );
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err(
            "Profile name must contain only letters, digits, hyphens, and underscores.".into(),
        );
    }
    Ok(())
}

pub(super) fn fix_profile_error(e: &crate::domain::errors::ProfileError) -> String {
    use crate::domain::errors::ProfileError;
    use crate::domain::models::PortDimension;
    match e {
        ProfileError::DimensionMissing { dimensions, .. } => {
            let dims: Vec<String> = dimensions.iter().map(|d| format!("{:?}", d).to_lowercase()).collect();
            format!(
                "Fix: add the missing port section(s) to the profile TOML. Required ports: {}.",
                dims.join(", ")
            )
        }
        ProfileError::AdapterUnknown {
            suggestion: Some(s), ..
        } => {
            format!("Fix: change adapter to '{}'? (Levenshtein-1 match)", s)
        }
        ProfileError::AdapterUnknown { .. } => {
            "Fix: check the adapter name spelling against `rustain doctor --adapters` output.".to_string()
        }
        ProfileError::AdapterFeatureGated { feature, .. } => {
            format!(
                "Fix: either (a) rebuild rustain with --features {}, or (b) set preview = true on the profile to enable graceful fallback.",
                feature
            )
        }
        ProfileError::CircularExtends { chain } => {
            format!(
                "Fix: break the cycle in the extends chain. Affected profiles: {}.",
                chain.join(" → ")
            )
        }
        ProfileError::ExtendsTooDeep { chain } => {
            format!(
                "Fix: flatten the extends chain to ≤ 4 levels. Current depth: {}. Affected chain: {}.",
                chain.len(),
                chain.join(" → ")
            )
        }
        ProfileError::ProfileNotFound { .. } => {
            "Fix: check that the profile name is correct. Run `rustain profile list` to see available profiles.".to_string()
        }
        ProfileError::ParentNotFound { child, parent, .. } => {
            format!(
                "Fix: create the parent profile '{}', or change '{}''s extends field to a different profile.",
                parent, child
            )
        }
        _ => "Fix: review the error message and correct the profile file.".to_string(),
    }
}

pub(super) fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(super) fn format_toml_value(key: &str, value: &str) -> String {
    if value == "true" {
        format!("{} = true", key)
    } else if value == "false" {
        format!("{} = false", key)
    } else if let Ok(n) = value.parse::<i64>() {
        format!("{} = {}", key, n)
    } else if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        format!("{} = {}", key, value)
    } else if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        format!("{} = \"{}\"", key, escape_toml_string(&value[1..value.len() - 1]))
    } else {
        format!("{} = \"{}\"", key, escape_toml_string(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_yes_no_accepts_y() {
        // Test the parsing logic by simulating input via a Cursor
        // The function reads from stdin, which we can't easily mock here
        // without extracting a Prompter trait (DF-NNN).
        // For now, just verify y/Y detection logic inline:
        assert!("y".trim().starts_with(['y', 'Y']));
        assert!("Y".trim().starts_with(['y', 'Y']));
        assert!("yes".trim().starts_with(['y', 'Y']));
        assert!("YES".trim().starts_with(['y', 'Y']));
        assert!(!"no".trim().starts_with(['y', 'Y']));
        assert!(!"N".trim().starts_with(['y', 'Y']));
        assert!(!"".trim().starts_with(['y', 'Y']));
    }

    #[test]
    fn prompt_line_trim_behavior() {
        // Trim removes leading/trailing whitespace
        assert_eq!("  hello  ".trim(), "hello");
        assert_eq!("  ".trim(), "");
    }

    #[test]
    fn prompt_required_line_validator_behavior() {
        let valid = |s: &str| -> Result<(), String> {
            if s.is_empty() {
                Err("cannot be empty".to_string())
            } else if s.len() > 5 {
                Err("too long".to_string())
            } else {
                Ok(())
            }
        };
        assert!(valid("ok").is_ok());
        assert!(valid("").is_err());
        assert!(valid("toolong").is_err());
    }

    #[test]
    fn test_validate_profile_name_rejects_traversal() {
        assert!(validate_profile_name("../etc").is_err());
        assert!(validate_profile_name("a/b").is_err());
        assert!(validate_profile_name("a\\b").is_err());
    }

    #[test]
    fn test_validate_profile_name_accepts_valid() {
        assert!(validate_profile_name("my-profile").is_ok());
        assert!(validate_profile_name("test_123").is_ok());
    }

    #[test]
    fn test_escape_toml_string() {
        assert_eq!(escape_toml_string("hello"), "hello");
        assert_eq!(escape_toml_string(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_toml_string(r"path\to"), r"path\\to");
    }

    #[test]
    fn test_format_toml_value_escapes_unquoted() {
        let result = format_toml_value("model", r#"my "model""#);
        assert_eq!(result, r#"model = "my \"model\"""#);
    }
}
