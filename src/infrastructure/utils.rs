//! Shared utility functions extracted from recurring patterns across Epic 2.
//!
//! These consolidate input validation, URL normalization, and ID sanitization
//! patterns that were independently implemented (and repeatedly flagged in review)
//! across Stories 2-0 through 2-4.

/// Read an environment variable, returning `None` if unset, empty, or whitespace-only.
///
/// This replaces the recurring pattern:
/// ```ignore
/// std::env::var(name).ok().filter(|s| !s.is_empty()).map(|s| s.trim().to_string())
/// ```
///
/// Returns `Some(trimmed_value)` only when the variable is set and contains
/// non-whitespace content.
pub fn env_var_trimmed(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Check if an environment variable is set to a non-empty value.
///
/// Equivalent to `env_var_trimmed(name).is_some()` but reads more clearly
/// in boolean contexts like `NO_COLOR` detection.
pub fn env_var_is_set(name: &str) -> bool {
    env_var_trimmed(name).is_some()
}

/// Normalize a base URL by trimming whitespace and removing trailing slashes.
///
/// This prevents double-slash issues when paths are appended:
/// `"https://api.example.com/" + "/v1/messages"` → double slash.
///
/// Returns the trimmed input unchanged if normalization would produce an empty
/// string (e.g., input is `"/"` or `"///"`).
pub fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim();
    let normalized = trimmed.trim_end_matches('/');
    if normalized.is_empty() {
        trimmed.to_string()
    } else {
        normalized.to_string()
    }
}

/// Sanitize an ID string to prevent path traversal attacks.
///
/// Returns `Ok(id)` if the ID is non-empty and contains only safe characters
/// (ASCII alphanumeric, hyphen, underscore). Returns `Err` otherwise.
///
/// This consolidates the `sanitize_id` implementations from `FileSystemStorage`
/// and crash recovery code.
pub fn sanitize_id(id: &str) -> Result<&str, SanitizeError> {
    if id.is_empty() {
        return Err(SanitizeError::Empty);
    }
    if id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(id)
    } else {
        Err(SanitizeError::InvalidCharacters)
    }
}

/// Error type for ID sanitization failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizeError {
    /// The ID string was empty.
    Empty,
    /// The ID contains characters outside the allowed set `[a-zA-Z0-9_-]`.
    InvalidCharacters,
}

impl std::fmt::Display for SanitizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SanitizeError::Empty => write!(f, "ID must not be empty"),
            SanitizeError::InvalidCharacters => {
                write!(
                    f,
                    "ID contains invalid characters (allowed: a-z, A-Z, 0-9, -, _)"
                )
            }
        }
    }
}

impl std::error::Error for SanitizeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: safely set env var (unsafe in Rust 2024 edition).
    unsafe fn set_env(key: &str, val: &str) {
        unsafe { std::env::set_var(key, val) }
    }

    /// Helper: safely remove env var (unsafe in Rust 2024 edition).
    unsafe fn remove_env(key: &str) {
        unsafe { std::env::remove_var(key) }
    }

    // ── env_var_trimmed ────────────────────────────────────────────────

    #[test]
    fn test_env_var_trimmed_unset() {
        unsafe { remove_env("_RUSTAIN_TEST_UNSET_VAR") };
        assert_eq!(env_var_trimmed("_RUSTAIN_TEST_UNSET_VAR"), None);
    }

    #[test]
    fn test_env_var_trimmed_empty() {
        unsafe { set_env("_RUSTAIN_TEST_EMPTY", "") };
        assert_eq!(env_var_trimmed("_RUSTAIN_TEST_EMPTY"), None);
        unsafe { remove_env("_RUSTAIN_TEST_EMPTY") };
    }

    #[test]
    fn test_env_var_trimmed_whitespace_only() {
        unsafe { set_env("_RUSTAIN_TEST_WS", "   ") };
        assert_eq!(env_var_trimmed("_RUSTAIN_TEST_WS"), None);
        unsafe { remove_env("_RUSTAIN_TEST_WS") };
    }

    #[test]
    fn test_env_var_trimmed_valid() {
        unsafe { set_env("_RUSTAIN_TEST_VALID", "my-value") };
        assert_eq!(
            env_var_trimmed("_RUSTAIN_TEST_VALID"),
            Some("my-value".to_string())
        );
        unsafe { remove_env("_RUSTAIN_TEST_VALID") };
    }

    #[test]
    fn test_env_var_trimmed_leading_trailing_whitespace() {
        unsafe { set_env("_RUSTAIN_TEST_PADDED", "  sk-ant-abc123  ") };
        assert_eq!(
            env_var_trimmed("_RUSTAIN_TEST_PADDED"),
            Some("sk-ant-abc123".to_string())
        );
        unsafe { remove_env("_RUSTAIN_TEST_PADDED") };
    }

    // ── env_var_is_set ─────────────────────────────────────────────────

    #[test]
    fn test_env_var_is_set_true() {
        unsafe { set_env("_RUSTAIN_TEST_SET", "yes") };
        assert!(env_var_is_set("_RUSTAIN_TEST_SET"));
        unsafe { remove_env("_RUSTAIN_TEST_SET") };
    }

    #[test]
    fn test_env_var_is_set_false_unset() {
        unsafe { remove_env("_RUSTAIN_TEST_NOT_SET") };
        assert!(!env_var_is_set("_RUSTAIN_TEST_NOT_SET"));
    }

    #[test]
    fn test_env_var_is_set_false_empty() {
        unsafe { set_env("_RUSTAIN_TEST_EMPTY_SET", "") };
        assert!(!env_var_is_set("_RUSTAIN_TEST_EMPTY_SET"));
        unsafe { remove_env("_RUSTAIN_TEST_EMPTY_SET") };
    }

    // ── normalize_base_url ─────────────────────────────────────────────

    #[test]
    fn test_normalize_base_url_no_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com"),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn test_normalize_base_url_single_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com/"),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn test_normalize_base_url_multiple_trailing_slashes() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com///"),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn test_normalize_base_url_whitespace() {
        assert_eq!(
            normalize_base_url("  https://api.z.ai/  "),
            "https://api.z.ai"
        );
    }

    #[test]
    fn test_normalize_base_url_preserves_path() {
        assert_eq!(
            normalize_base_url("https://proxy.example.com/api/anthropic"),
            "https://proxy.example.com/api/anthropic"
        );
    }

    #[test]
    fn test_normalize_base_url_slash_only_input() {
        assert_eq!(normalize_base_url("/"), "/");
        assert_eq!(normalize_base_url("///"), "///");
    }

    // ── sanitize_id ────────────────────────────────────────────────────

    #[test]
    fn test_sanitize_id_valid_nanoid() {
        assert_eq!(sanitize_id("abc123-XYZ_456"), Ok("abc123-XYZ_456"));
    }

    #[test]
    fn test_sanitize_id_empty() {
        assert_eq!(sanitize_id(""), Err(SanitizeError::Empty));
    }

    #[test]
    fn test_sanitize_id_path_traversal_unix() {
        assert_eq!(
            sanitize_id("../etc/passwd"),
            Err(SanitizeError::InvalidCharacters)
        );
    }

    #[test]
    fn test_sanitize_id_path_traversal_windows() {
        assert_eq!(
            sanitize_id("..\\windows\\system32"),
            Err(SanitizeError::InvalidCharacters)
        );
    }

    #[test]
    fn test_sanitize_id_slash() {
        assert_eq!(sanitize_id("a/b"), Err(SanitizeError::InvalidCharacters));
    }

    #[test]
    fn test_sanitize_id_special_characters() {
        assert_eq!(
            sanitize_id("hello world"),
            Err(SanitizeError::InvalidCharacters)
        );
        assert_eq!(
            sanitize_id("id@home"),
            Err(SanitizeError::InvalidCharacters)
        );
        assert_eq!(
            sanitize_id("id;rm -rf"),
            Err(SanitizeError::InvalidCharacters)
        );
    }

    #[test]
    fn test_sanitize_id_simple_valid() {
        assert_eq!(sanitize_id("session-01"), Ok("session-01"));
        assert_eq!(sanitize_id("abc"), Ok("abc"));
        assert_eq!(sanitize_id("A"), Ok("A"));
    }
}
