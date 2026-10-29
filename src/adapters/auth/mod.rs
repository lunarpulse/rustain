pub mod codex_jwt;
pub mod flows;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
pub mod gemini_cli_project;
pub mod host;
pub mod opener;
pub mod policies;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
pub mod xai_oidc;

/// Longest provider-supplied message a login host shows, in characters.
pub const PROVIDER_TEXT_CAP: usize = 500;

/// Provider-supplied text (an error message) made safe for a terminal before
/// it reaches stderr or the TUI: escape sequences stripped, every control
/// character a space, and cut to `PROVIDER_TEXT_CAP` characters (the cut the
/// Gemini error paths already applied to raw bodies).
pub fn bounded_provider_text(text: &str) -> String {
    crate::adapters::cli::session::list::sanitize_title(text)
        .chars()
        .take(PROVIDER_TEXT_CAP)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{PROVIDER_TEXT_CAP, bounded_provider_text};

    #[test]
    fn provider_text_is_control_free_and_capped() {
        let hostile = format!(
            "bad\x1b]52;c;ZXZpbA==\x07 \x1b[2Jthing\u{9b}\n{}",
            "x".repeat(4000)
        );
        let shown = bounded_provider_text(&hostile);
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(!shown.contains("ZXZpbA"), "{shown:?}");
        assert_eq!(shown.chars().count(), PROVIDER_TEXT_CAP);
        assert_eq!(bounded_provider_text("plain"), "plain");
    }
}
