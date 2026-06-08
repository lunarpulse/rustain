//! Shared text normalization — the identity function for dedup and redaction.
//!
//! Two facts/entries are "the same information" iff their normalized text is
//! equal. Used by `LongTermMemory` (within-category dedup), `ProjectScopedMemory`
//! (cross-tier dedup, long-term wins), and `VectorSearchMemory` (content-stable
//! redaction token, Story 12.1c AC3).

/// Normalize text for dedup: trim, collapse internal whitespace, lowercase.
pub fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_and_lowercases() {
        assert_eq!(normalize("  Prefers   snake_case "), "prefers snake_case");
        assert_eq!(normalize("A\tB  C"), "a b c");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
    }
}
