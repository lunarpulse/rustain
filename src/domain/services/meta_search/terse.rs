//! `terse` derivation per ADR-09-02 v2 §LLM-Only Payload.
//!
//! # Algorithm
//!
//! 1. Strip leading whitespace from `desc`.
//! 2. Find the first sentence boundary (`. `, `! `, `? `) within the first
//!    `TERSE_MAX_BYTES + 8` byte window. The 8-byte slack absorbs the
//!    sentence terminator and trailing space.
//! 3. If a sentence boundary is found and the sentence fits in `TERSE_MAX_BYTES`,
//!    return it verbatim (including the terminator).
//! 4. Otherwise, truncate to `TERSE_MAX_BYTES` at a UTF-8-safe boundary
//!    (the nearest preceding char boundary per `str::is_char_boundary`),
//!    append a `…` ellipsis.
//! 5. If `desc` is empty or whitespace-only, fall back to `name`.
//!
//! # NOT done
//!
//! - BM25 tokenizer is NOT used here. Stemming breaks readability — `terse`
//!   is shown to the LLM verbatim.
//! - HTML / markdown stripping is NOT done here. Skills with Markdown in
//!   `description` get Markdown in `terse` — the LLM handles Markdown fine.
//! - Language detection is NOT done here. Partial multilingual descriptions
//!   pass through to BM25's UTF-8 word-segmentation.

/// Maximum byte length of the produced `terse` string (UTF-8 safe — we do
/// NOT truncate mid-char). 120 bytes ≈ 24-30 tokens at the 4-chars-per-token
/// English heuristic, well under the 30-tok p95 budget from AC-9-7b (Story 9.7b
/// owns the budget enforcement test against the 60+ fixture corpus).
pub const TERSE_MAX_BYTES: usize = 120;

/// Derive the `terse` projection for a capability with the given description
/// and fallback `name`.
pub fn compute_terse(desc: &str, name: &str) -> String {
    let trimmed = desc.trim_start();
    if trimmed.is_empty() {
        return name.to_string();
    }

    // Look for sentence boundary within the first TERSE_MAX_BYTES + 8 bytes.
    let mut window_end = trimmed.len().min(TERSE_MAX_BYTES + 8);
    // Ensure window_end is at a char boundary to avoid panic on slice.
    while window_end > 0 && !trimmed.is_char_boundary(window_end) {
        window_end -= 1;
    }
    let window = &trimmed[..window_end];
    for terminator in &[". ", "! ", "? "] {
        if let Some(pos) = window.find(terminator) {
            // Include the terminator char (one byte for ASCII period/!/?).
            let sentence_end = pos + 1;
            if sentence_end <= TERSE_MAX_BYTES {
                // Find the char boundary at sentence_end. ASCII '.', '!', '?'
                // are all 1-byte so sentence_end IS a char boundary.
                return trimmed[..sentence_end].trim_end().to_string();
            }
        }
    }

    // No sentence boundary in the window: UTF-8-safe truncate.
    if trimmed.len() <= TERSE_MAX_BYTES {
        return trimmed.to_string();
    }
    let mut cut = TERSE_MAX_BYTES;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &trimmed[..cut].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_sentence_returned_verbatim() {
        let desc = "Runs ruff format on the file. The result is written back.";
        assert_eq!(
            compute_terse(desc, "ruff_format"),
            "Runs ruff format on the file."
        );
    }

    #[test]
    fn test_no_sentence_boundary_truncates_with_ellipsis() {
        let desc = "a".repeat(200);
        let out = compute_terse(&desc, "x");
        assert!(
            out.ends_with('…'),
            "long desc without sentence terminator must end with ellipsis"
        );
        // The ellipsis is 3 bytes in UTF-8 ('…' = E2 80 A6). The prefix is
        // TERSE_MAX_BYTES bytes max.
        assert!(out.len() <= TERSE_MAX_BYTES + 3);
    }

    #[test]
    fn test_empty_description_falls_back_to_name() {
        assert_eq!(compute_terse("", "review-code"), "review-code");
        assert_eq!(compute_terse("   ", "review-code"), "review-code");
    }

    #[test]
    fn test_utf8_boundary_safe_truncation() {
        // 4-byte UTF-8 char ('𝛼' = F0 9D 9B BC) positioned across the
        // TERSE_MAX_BYTES boundary. The truncation MUST land on a char
        // boundary, not in the middle of the multi-byte sequence.
        let prefix = "a".repeat(TERSE_MAX_BYTES - 2);
        let desc = format!("{prefix}𝛼𝛽𝛾");
        let out = compute_terse(&desc, "x");
        // Verify out is valid UTF-8 (would panic on bad slicing).
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.ends_with('…'));
    }

    #[test]
    fn test_short_sentence_fits_entirely() {
        let desc = "Quick description.";
        assert_eq!(compute_terse(desc, "x"), "Quick description.");
    }

    #[test]
    fn test_question_mark_terminator_recognized() {
        let desc = "Can you format Python? Yes, with ruff.";
        assert_eq!(compute_terse(desc, "x"), "Can you format Python?");
    }

    #[test]
    fn test_exclamation_terminator_recognized() {
        let desc = "Run this now! It's important.";
        assert_eq!(compute_terse(desc, "x"), "Run this now!");
    }
}
