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
}
