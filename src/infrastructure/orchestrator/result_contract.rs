//! Structured result contract (Story 14.3, AC6).
//!
//! A child spoke returns via a structured `yield` validated against an output
//! schema, with **schema-retry** and **last-assistant salvage on cancel**. This
//! is the "structured result contract" in the story title — distinct from the
//! CLI `--output-format json` of Story 13.1b (net-new here).
//!
//! ## Parse, don't validate
//!
//! [`validate_yield`] parses the raw child output into a [`SpokeYield`]
//! (serde does the validation). A second pass that re-checks fields would be
//! "validate, don't parse" — rejected. R1's schema is minimal and additive:
//!
//! ```jsonc
//! { "summary": "<compact salience noun-phrase>", "detail": "<optional body>" }
//! ```
//!
//! `summary` is the compact metadata that enters the [`Window`](super::Window);
//! `detail` is the full payload that stays in the [`ResultStore`](super::ResultStore).

use serde::{Deserialize, Serialize};

use crate::domain::models::orchestration::SpokeResult;

/// The validated child yield. `summary` is compact (window-bound); `detail` is
/// the full body (side-table only, AC5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpokeYield {
    /// Compact salience noun-phrase — what enters the prompt window.
    #[serde(rename = "summary")]
    pub summary: String,
    /// Full payload body — stays in the ResultStore, fetched on drill.
    #[serde(rename = "detail", default)]
    pub detail: String,
}

/// Why a child yield failed schema validation (→ schema-retry or salvage).
#[non_exhaustive]
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum YieldError {
    #[error("yield was not valid JSON: {0}")]
    NotJson(String),
    #[error("yield missing required field `summary`")]
    MissingSummary,
    #[error("yield `summary` was empty")]
    EmptySummary,
}

/// Validate a raw child yield against the schema (parse, don't validate).
///
/// Tolerant of a leading ``` ```json ``` fence (children often wrap output).
pub fn validate_yield(raw: &str) -> Result<SpokeYield, YieldError> {
    let trimmed = strip_code_fence(raw.trim());
    let parsed: SpokeYield =
        serde_json::from_str(trimmed).map_err(|e| YieldError::NotJson(e.to_string()))?;
    if parsed.summary.trim().is_empty() {
        return Err(YieldError::EmptySummary);
    }
    Ok(parsed)
}

/// Schema-retry: try each candidate yield in order; the first that validates
/// wins. Mirrors the model-retry discipline (a malformed first attempt does not
/// forfeit the spoke — the next attempt is tried).
pub fn retry_on_schema_failure(yields: &[String]) -> Result<SpokeYield, YieldError> {
    let mut last_err = YieldError::NotJson("no yields supplied".into());
    for y in yields {
        match validate_yield(y) {
            Ok(parsed) => return Ok(parsed),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Last-assistant salvage on cancel: when a spoke is cancelled mid-stream,
/// salvage the partial output as a `Cancelled` result with whatever summary
/// can be extracted (never confident noise — the outcome stays `Cancelled`).
pub fn salvage_on_cancel(raw: &str) -> (SpokeResult, String) {
    // Try to parse whatever was emitted; if it yields a summary, keep it as the
    // body for drill, but the OUTCOME stays Cancelled (no false "Completed").
    let body = raw.to_string();
    if let Ok(parsed) = validate_yield(raw) {
        return (SpokeResult::Cancelled, parsed.detail);
    }
    (SpokeResult::Cancelled, body)
}

fn strip_code_fence(s: &str) -> &str {
    // Tolerate ```json\n ... ``` wrappers.
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Default byte-cap for spoke summaries entering the prompt window (DD3).
/// ~3 lines of English prose. Configurable by callers if needed.
pub const SPOKE_SUMMARY_MAX_BYTES: usize = 240;

/// Extract the "lede" — text up to the first blank-line separator, or the
/// whole text if there is none. Matches BOTH Unix (`\n\n`) and Windows
/// (`\r\n\r\n`) paragraph separators (PATCH-10: the prior `\n\n`-only match
/// treated CRLF bodies as a single lede, defeating the lede bound). Returns
/// the slice up to the EARLIEST separator (no allocation).
pub fn first_paragraph(text: &str) -> &str {
    match (text.find("\n\n"), text.find("\r\n\r\n")) {
        (Some(a), Some(b)) => &text[..a.min(b)],
        (Some(a), None) => &text[..a],
        (None, Some(b)) => &text[..b],
        (None, None) => text,
    }
}

/// Build a compact spoke summary: lede-bounded, then byte-capped on a char
/// boundary so multi-byte UTF-8 is never split.
///
/// Invariants (proven by `prop_spoke_summary`):
/// - `result.len() <= max_bytes`
/// - `text.starts_with(result)`
/// - `result.len() <= first_paragraph(text).len()`
/// - if `first_paragraph(text).len() <= max_bytes` then `result == first_paragraph(text)`
pub fn spoke_summary(text: &str, max_bytes: usize) -> &str {
    let lede = first_paragraph(text);
    if lede.len() <= max_bytes {
        return lede;
    }
    // floor_char_boundary: find the largest byte index <= max_bytes that is
    // a char boundary. Rust 1.73+ has str::floor_char_boundary, but we
    // hand-roll for compatibility.
    let mut end = max_bytes;
    while end > 0 && !lede.is_char_boundary(end) {
        end -= 1;
    }
    &lede[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_yield_parses_minimal_schema() {
        let y = validate_yield(r#"{"summary":"found 3 races","detail":"..."}"#).unwrap();
        assert_eq!(y.summary, "found 3 races");
        assert_eq!(y.detail, "...");
    }

    #[test]
    fn validate_yield_detail_is_optional() {
        let y = validate_yield(r#"{"summary":"ok"}"#).unwrap();
        assert_eq!(y.summary, "ok");
        assert!(y.detail.is_empty());
    }

    #[test]
    fn validate_yield_rejects_non_json_and_empty_summary() {
        assert!(matches!(
            validate_yield("not json"),
            Err(YieldError::NotJson(_))
        ));
        assert!(matches!(
            validate_yield(r#"{"summary":"  "}"#),
            Err(YieldError::EmptySummary)
        ));
        assert!(matches!(
            validate_yield(r#"{"detail":"x"}"#),
            Err(YieldError::NotJson(_)) // missing `summary` → serde error
        ));
    }

    #[test]
    fn validate_yield_strips_code_fence() {
        let y = validate_yield("```json\n{\"summary\":\"fenced\"}\n```").unwrap();
        assert_eq!(y.summary, "fenced");
    }

    #[test]
    fn schema_retry_uses_first_valid_yield() {
        let yields = vec![
            "garbage".to_string(),
            r#"{"summary":"second try"}"#.to_string(),
            r#"{"summary":"third"}"#.to_string(),
        ];
        let y = retry_on_schema_failure(&yields).unwrap();
        assert_eq!(y.summary, "second try");
    }

    #[test]
    fn schema_retry_fails_when_all_invalid() {
        let yields = vec!["nope".to_string(), "also no".to_string()];
        assert!(retry_on_schema_failure(&yields).is_err());
    }

    #[test]
    fn salvage_on_cancel_keeps_cancelled_outcome_with_body() {
        // A partial JSON yield is salvaged into the detail body, but the OUTCOME
        // stays Cancelled (never a false Completed).
        let (result, body) = salvage_on_cancel(r#"{"summary":"partial","detail":"salvaged body"}"#);
        assert!(matches!(result, SpokeResult::Cancelled));
        assert_eq!(body, "salvaged body");
    }

    #[test]
    fn salvage_on_cancel_handles_garbage() {
        let (result, _body) = salvage_on_cancel("streaming garbage mid-cancel");
        assert!(matches!(result, SpokeResult::Cancelled));
    }

    // ── spoke_summary / first_paragraph unit tests ────────────────────────

    #[test]
    fn first_paragraph_extracts_lede() {
        assert_eq!(first_paragraph("hello\n\nworld"), "hello");
        assert_eq!(first_paragraph("single line"), "single line");
        assert_eq!(first_paragraph(""), "");
        assert_eq!(first_paragraph("a\n\nb\n\nc"), "a");
    }

    #[test]
    fn spoke_summary_respects_byte_cap() {
        let text = "a".repeat(300);
        let s = spoke_summary(&text, SPOKE_SUMMARY_MAX_BYTES);
        assert!(s.len() <= SPOKE_SUMMARY_MAX_BYTES);
        assert!(text.starts_with(s));
    }

    #[test]
    fn spoke_summary_preserves_short_lede() {
        let text = "short lede\n\nlong body that goes on and on";
        let s = spoke_summary(text, SPOKE_SUMMARY_MAX_BYTES);
        assert_eq!(s, "short lede");
    }

    #[test]
    fn spoke_summary_multibyte_does_not_split() {
        // 4-byte emoji: 🦀 = 4 bytes; cap at 3 should truncate to 0 not mid-char
        let text = "🦀🦀🦀";
        let s = spoke_summary(text, 3);
        assert!(s.is_empty()); // can't fit even one 4-byte char
        let s = spoke_summary(text, 4);
        assert_eq!(s, "🦀");
        let s = spoke_summary(text, 8);
        assert_eq!(s, "🦀🦀");
    }

    #[test]
    fn spoke_summary_lede_identity_when_short() {
        // I4: if first_paragraph fits within max, summary == first_paragraph
        let text = "short\n\nlong body";
        let s = spoke_summary(text, 240);
        assert_eq!(s, first_paragraph(text));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy that generates multi-paragraph and multibyte strings.
    fn text_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop::string::string_regex(
                // Mix ASCII, multibyte (Greek, emoji), and paragraph separators
                "[a-zA-Z0-9 αβγδ🦀🎯]{0,120}",
            )
            .unwrap(),
            1..=5,
        )
        .prop_map(|parts| parts.join("\n\n"))
    }

    proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// I1: `summary.len() <= max`
        /// I2: `text.starts_with(summary)` (subsumes char-boundary safety)
        /// I3: `summary.len() <= first_paragraph(text).len()`
        /// I4: `first_paragraph(text).len() <= max => summary == first_paragraph(text)`
        #[test]
        fn prop_spoke_summary(
            text in text_strategy(),
            max in 1..=512usize,
        ) {
            let summary = spoke_summary(&text, max);

            // I1: byte-capped
            prop_assert!(
                summary.len() <= max,
                "I1 violated: summary.len()={} > max={}",
                summary.len(), max
            );

            // I2: prefix of original text (subsumes char-boundary)
            prop_assert!(
                text.starts_with(summary),
                "I2 violated: text does not start with summary"
            );

            // I3: lede-bounded
            let lede = first_paragraph(&text);
            prop_assert!(
                summary.len() <= lede.len(),
                "I3 violated: summary.len()={} > lede.len()={}",
                summary.len(), lede.len()
            );

            // I4: lede-identity when lede fits
            if lede.len() <= max {
                prop_assert_eq!(
                    summary, lede,
                    "I4 violated: lede fits but summary != lede"
                );
            }
        }
    }
}
