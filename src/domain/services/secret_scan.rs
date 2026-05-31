//! `scan_for_secrets` — a pure, high-confidence secret-pattern detector that
//! gates the LLM-driven memory-capture path (Story 11.2, Task 7 / Q4 LOCKED).
//!
//! Privacy-by-default (ux-spec:2442/2482): a secret the model decides to
//! "remember" must never be written to disk "regardless of the LLM's notable
//! judgment". Both write-trigger tools (`remember_fact`, new; `remember`,
//! retrofitted) run this scan at capture and BLOCK on a hit.
//!
//! Design constraints:
//! - **High-confidence patterns ONLY** — prefix/structure, never entropy /
//!   Shannon heuristics. A false positive blocks legitimate memory, so the set
//!   is deliberately small and conservative.
//! - **Pure** — no I/O, no async (domain/services discipline: pure functions on
//!   domain types, alongside `permission_chain.rs`, `command_normalize.rs`).
//! - **Block, do not redact** — callers return an error result so the model
//!   knows its capture failed and can retry without the secret.
//! - Uses the existing `regex` dependency (Cargo.toml). NO new crate.
//! - **Human manual edits are NOT gated** — the user owns `MEMORY.md`; this
//!   governs LLM-driven capture only.

use std::sync::LazyLock;

use regex::Regex;

/// The high-confidence secret patterns, each paired with a human-readable name.
/// Compiled once on first use. Order is the match-priority order.
static SECRET_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        // AWS access-key id: `AKIA` + 16 uppercase alphanumerics.
        (
            "AWS access key id",
            Regex::new(r"AKIA[0-9A-Z]{16}").expect("valid AKIA regex"),
        ),
        // OpenAI-style key: `sk-` (optionally `sk-proj-`) + 20+ alphanumerics.
        // The 20-char floor avoids false positives like "sk-style"/"sk-prefix".
        (
            "OpenAI API key",
            Regex::new(r"sk-(?:proj-)?[A-Za-z0-9]{20,}").expect("valid sk- regex"),
        ),
        // GitHub classic personal access token: `ghp_` + 36 alphanumerics.
        (
            "GitHub personal access token",
            Regex::new(r"ghp_[A-Za-z0-9]{36}").expect("valid ghp_ regex"),
        ),
        // GitHub fine-grained PAT: `github_pat_` + a long base62/underscore tail.
        (
            "GitHub fine-grained token",
            Regex::new(r"github_pat_[A-Za-z0-9_]{20,}").expect("valid github_pat_ regex"),
        ),
        // PEM private-key block header (RSA/EC/OPENSSH/generic).
        (
            "PEM private key block",
            Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("valid PEM regex"),
        ),
    ]
});

/// Return the name of the first high-confidence secret pattern matched in
/// `text`, or `None` if none match. Conservative by design — only prefix /
/// structural patterns, no entropy heuristics.
pub fn scan_for_secrets(text: &str) -> Option<&'static str> {
    SECRET_PATTERNS
        .iter()
        .find(|(_, re)| re.is_match(text))
        .map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_aws_access_key() {
        assert_eq!(
            scan_for_secrets("creds: AKIAIOSFODNN7EXAMPLE end"),
            Some("AWS access key id")
        );
    }

    #[test]
    fn matches_openai_key_classic_and_project() {
        assert_eq!(
            scan_for_secrets("key sk-abcdefghijklmnopqrstuvwx done"),
            Some("OpenAI API key")
        );
        assert_eq!(
            scan_for_secrets("token sk-proj-abcdefghijklmnopqrstuvwx"),
            Some("OpenAI API key")
        );
    }

    #[test]
    fn matches_github_tokens() {
        // ghp_ + exactly 36 chars.
        let ghp = format!("ghp_{}", "a".repeat(36));
        assert_eq!(
            scan_for_secrets(&format!("token {ghp}")),
            Some("GitHub personal access token")
        );
        let pat = format!("github_pat_{}", "B1".repeat(15));
        assert_eq!(scan_for_secrets(&pat), Some("GitHub fine-grained token"));
    }

    #[test]
    fn matches_pem_private_key_block() {
        assert_eq!(
            scan_for_secrets("-----BEGIN RSA PRIVATE KEY-----\nMIIE..."),
            Some("PEM private key block")
        );
        assert_eq!(
            scan_for_secrets("-----BEGIN PRIVATE KEY-----"),
            Some("PEM private key block")
        );
        assert_eq!(
            scan_for_secrets("-----BEGIN OPENSSH PRIVATE KEY-----"),
            Some("PEM private key block")
        );
    }

    #[test]
    fn no_false_positive_on_ordinary_prose() {
        // The classic trap strings from the story's test spec.
        assert_eq!(scan_for_secrets("name it sk-style"), None);
        assert_eq!(scan_for_secrets("use sk-prefix naming"), None);
        // A 16-char token without the AKIA prefix.
        assert_eq!(scan_for_secrets("0123456789ABCDEF"), None);
        // Plain durable facts.
        assert_eq!(scan_for_secrets("the user prefers snake_case"), None);
        assert_eq!(scan_for_secrets("the DB is PostgreSQL 15"), None);
        // A short ghp_ that isn't a real token length.
        assert_eq!(scan_for_secrets("ghp_short"), None);
    }

    // DF-4: Per-field scanning catches secrets that were previously split
    // across field boundaries when joined with \n.
    #[test]
    fn catches_secret_in_individual_field() {
        assert_eq!(
            scan_for_secrets("-----BEGIN RSA PRIVATE KEY-----"),
            Some("PEM private key block")
        );
        assert_eq!(
            scan_for_secrets("key is AKIAIOSFODNN7EXAMPLE"),
            Some("AWS access key id")
        );
    }
}
