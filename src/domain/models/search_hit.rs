use serde::{Deserialize, Serialize};

use crate::domain::models::capability_kind::CapabilityKind;

/// LLM-facing capability-search projection per ADR-09-02 v2 §LLM-Only Payload.
///
/// # Schema lock-down — DO NOT ADD FIELDS
///
/// The deliberate field exclusion of `description`, `input_schema`,
/// `parameters`, `version`, `category`, `icon` is THE structural countermeasure
/// to Mary v2 transformed dissent (LLM noun-conflation under unified
/// retrieval). Adding ANY of these fields silently re-introduces the
/// 2-stage discovery violation — L1 search → L2 hydrate becomes L1 search →
/// "good enough to invoke from L1 alone" — and burns the per-turn token
/// budget on metadata the LLM can fetch on demand via `tools/list` or
/// implicit body hydration.
///
/// Schema lock-down is enforced by:
/// 1. The struct definition itself (no `#[serde(flatten)]`, no catch-all
///    `extra: Map<String, Value>` field, no opt-in `#[serde(other)]` variant).
/// 2. `tests/conformance_search_hit_schema.rs` (AC-9-7-3) — `serde_json::Value`
///    introspection AFTER serialization asserts the field set is exactly
///    `{name, kind, terse, score}` ∪ `{provider, matched_terms}` (the
///    optional set).
/// 3. `tests/search_hit_roundtrip.rs` (AC-9-7-3) — every `SearchHit.name`
///    for `kind == Tool` round-trips through `CapabilityId::from_mcp_wire_name`
///    or `CapabilityId::parse("builtin::<name>")`.
///
/// # Field semantics
///
/// - `name` — MANDATORY. For MCP tools: the `mcp__<server>__<tool>` wire form
///   (round-trippable via `CapabilityId::from_mcp_wire_name`). For builtin
///   tools: the bare tool name (`Bash`, `Read`, `Write`, etc.). For skills:
///   the skill name (matches `^[a-z][a-z0-9-]{0,63}$` per Anthropic
///   frontmatter contract from 9.6).
/// - `kind` — MANDATORY. Determines hydration path: `Tool` → existing
///   `tools/list` flow; `Skill` → `skill_view` builtin tool from 9.6.
/// - `terse` — MANDATORY. Index-time first-sentence + 120-char UTF-8-safe
///   truncate of `description`, with fallback to `name` when description is
///   empty. BM25 tokenizer NOT used (stemming breaks readability). Cached
///   in `MergedIndex` per `DocKey`.
/// - `score` — MANDATORY. BM25 score; preserves ranking determinism for
///   the LLM (the LLM can compare scores when deciding which hit to act on
///   first).
/// - `provider` — OPTIONAL. Disambiguation only — populated when the same
///   `(kind, name)` exists across multiple providers (e.g. an MCP `query`
///   tool from `postgres` server AND a `query` tool from `mysql` server).
///   `None` when not name-colliding. `#[serde(skip_serializing_if =
///   "Option::is_none")]` so the field is OMITTED from the JSON payload
///   when None — saves tokens in the common case.
/// - `matched_terms` — OPTIONAL. Eval/debug only — populated when the
///   `search_capabilities` builtin tool is invoked with the debug flag set
///   (Story 9.8's `rustain catalog search --json` consumes this for
///   diff-based regression testing). Production LLM consumption defaults
///   `None`; the engine never speculatively populates this field on the LLM
///   path because the token cost is non-trivial on a 5-hit response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub kind: CapabilityKind,
    pub terse: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_terms: Option<Vec<String>>,
}

impl SearchHit {
    /// Construct a minimal hit (4 mandatory fields, no optionals).
    pub fn minimal(
        name: impl Into<String>,
        kind: CapabilityKind,
        terse: impl Into<String>,
        score: f32,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            terse: terse.into(),
            score,
            provider: None,
            matched_terms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_hit_minimal_constructor() {
        let hit = SearchHit::minimal("Bash", CapabilityKind::Tool, "Execute shell commands.", 8.4);
        assert_eq!(hit.name, "Bash");
        assert_eq!(hit.kind, CapabilityKind::Tool);
        assert_eq!(hit.terse, "Execute shell commands.");
        assert_eq!(hit.score, 8.4);
        assert!(hit.provider.is_none());
        assert!(hit.matched_terms.is_none());
    }

    #[test]
    fn test_search_hit_serialization_field_set_locked() {
        // Construct a hit with ALL fields populated.
        let hit = SearchHit {
            name: "mcp__postgres__query".into(),
            kind: CapabilityKind::Tool,
            terse: "Run SQL against the configured PostgreSQL instance.".into(),
            score: 12.7,
            provider: Some("postgres".into()),
            matched_terms: Some(vec!["sql".into(), "postgres".into()]),
        };
        let v: serde_json::Value = serde_json::to_value(&hit).unwrap();
        let obj = v.as_object().expect("SearchHit serializes to JSON object");
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "kind",
                "matched_terms",
                "name",
                "provider",
                "score",
                "terse"
            ],
            "SearchHit serialized field set MUST be exactly {{name, kind, terse, score, provider?, matched_terms?}} \
             per ADR-09-02 v2 §LLM-Only Payload + Mary amendment A2. \
             Adding fields silently re-opens the 2-stage discovery violation. \
             If you need a new field, re-open ADR-09-02 v3."
        );
    }

    #[test]
    fn test_search_hit_omits_none_optionals_from_payload() {
        let hit = SearchHit::minimal("Bash", CapabilityKind::Tool, "Execute shell commands.", 8.4);
        let v: serde_json::Value = serde_json::to_value(&hit).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["kind", "name", "score", "terse"],
            "Minimal SearchHit (provider=None, matched_terms=None) MUST serialize to exactly 4 keys"
        );
    }
}
