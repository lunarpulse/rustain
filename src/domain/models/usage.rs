#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::provider::ModelCapability;
use super::router::{EscalationReason, ModelTier, StepKind};

/// Token usage of one provider call, in rustain's single counting convention.
///
/// The counts are **disjoint** for every provider:
/// - `input_tokens`: prompt tokens billed at the input rate. Excludes
///   `cache_read_input_tokens` and `cache_creation_input_tokens`.
/// - `cache_read_input_tokens`, `cache_creation_input_tokens`: prompt tokens
///   read from and written to the provider's cache. Disjoint from
///   `input_tokens` and from each other.
/// - `output_tokens`: every generated token, reasoning included.
/// - `reasoning_tokens`: the part of `output_tokens` spent on reasoning; never
///   greater than `output_tokens`. Informational for pricing unless
///   `reasoning_per_million` is set.
///
/// The prompt a call sent is `input + cache_read + cache_creation`, see
/// [`UsageInfo::prompt_tokens`]. Adapters whose provider reports inclusive
/// counts (OpenAI Chat, Responses, Gemini) build this with
/// [`UsageInfo::from_inclusive_counts`]; Anthropic and Ollama report disjoint
/// counts already. `TokenUsage` (the ledger row) has the same meaning field
/// for field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageInfo {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    /// Reasoning tokens (e.g., Claude extended thinking), a part of
    /// `output_tokens`. Added in S7.1a for cost tracking parity (FR36).
    pub reasoning_tokens: Option<u32>,
    /// The serving transport established the counters used for accounting.
    /// Not part of normalized usage or persisted session data.
    #[serde(skip)]
    pub counters_observed: bool,
}

impl UsageInfo {
    /// Normalise a provider's inclusive counts: `prompt` already contains the
    /// cache reads and writes, `completion` already contains the reasoning.
    ///
    /// A cache count above the prompt it is a part of is a provider error and
    /// is clamped, so [`UsageInfo::prompt_tokens`] of the result always equals
    /// `prompt` (cache reads first, then writes).
    /// This does not establish provenance; the adapter must validate the
    /// provider's counters before trusting the result.
    pub fn from_inclusive_counts(
        prompt: u32,
        completion: u32,
        cache_read: Option<u32>,
        cache_write: Option<u32>,
        reasoning: Option<u32>,
    ) -> Self {
        let cache_read = cache_read.map(|read| read.min(prompt));
        let cache_write = cache_write.map(|write| write.min(prompt - cache_read.unwrap_or(0)));
        Self {
            input_tokens: prompt - cache_read.unwrap_or(0) - cache_write.unwrap_or(0),
            output_tokens: completion,
            cache_creation_input_tokens: cache_write,
            cache_read_input_tokens: cache_read,
            reasoning_tokens: reasoning.map(|r| r.min(completion)),
            counters_observed: false,
        }
    }

    /// Tokens of the prompt the call sent: input plus cache reads and writes
    /// (saturating). The context-size readers use this; pricing never does.
    pub fn prompt_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
    }
}

/// Token usage of one provider call as the ledger stores it — distinct from
/// the raw provider `UsageInfo` only in carrying `parent_ctx`. The counts mean
/// the same, field for field, as in [`UsageInfo`]: disjoint, reasoning inside
/// output.
///
/// Story 7.5 AC8: extended with optional cache/reasoning fields so cost
/// arithmetic can charge each token class at its correct rate. Legacy ledger
/// lines (pre-7.5) deserialize with all three new fields as `None` via
/// `#[serde(default)]` — backward-compat is asserted by the
/// `flat_file::ledger_append_roundtrip` test. Rows written before eval-2 by
/// OpenAI-style adapters counted cache reads inside `tokens_in`; they are read
/// as written (see `cost_calculator`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub parent_ctx: u32,
    /// Cache-write tokens (Story 7.5 AC8), disjoint from `tokens_in`.
    #[serde(default)]
    pub cache_creation_tokens: Option<u32>,
    /// Cache-read tokens (Story 7.5 AC8), disjoint from `tokens_in`.
    #[serde(default)]
    pub cache_read_tokens: Option<u32>,
    /// Reasoning tokens (extended thinking) (Story 7.5 AC8), a part of `tokens_out`.
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl TokenUsage {
    /// The ledger row of one provider call: the five counts of `usage` and the
    /// parent context size.
    pub fn from_usage(usage: &UsageInfo, parent_ctx: u32) -> Self {
        Self {
            tokens_in: usage.input_tokens,
            tokens_out: usage.output_tokens,
            parent_ctx,
            cache_creation_tokens: usage.cache_creation_input_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

/// A single entry in the usage ledger — one row per provider call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLedgerEntry {
    pub timestamp_ms: i64,
    pub session_id: String,
    pub conversation_id: String,
    pub provider_id: String,
    pub model: String,
    pub tier: ModelTier,
    pub step_kind: Option<StepKind>,
    pub escalation_reason: EscalationReason,
    pub usage: TokenUsage,
    /// `None` preserves the legacy/unmarked format and does not imply observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<UsageProvenance>,
}

/// Whether the ledger row's counters were established by the serving transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    Observed,
    Unknown,
}

/// Information about an available model from a provider.
///
/// **Legacy struct** — kept for backward compatibility until a dedicated
/// story migrates all consumers to `ModelDescriptor`. Both live side-by-side
/// in the provider registry; `ProviderRegistry::list_models()` returns
/// `ModelDescriptor`, but existing code reading `ModelInfo` still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    /// Capabilities this model supports (added in S7.1a).
    #[serde(default)]
    pub capabilities: std::collections::HashSet<ModelCapability>,
    /// Optional pricing tier for UI grouping (added in S7.1a).
    #[serde(default)]
    pub pricing_tier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(input, output, cache_read, cache_creation, reasoning)` of a usage.
    type Counts = (u32, u32, Option<u32>, Option<u32>, Option<u32>);

    fn counts(u: &UsageInfo) -> Counts {
        (
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
            u.reasoning_tokens,
        )
    }

    #[test]
    fn inclusive_counts_become_disjoint_and_keep_the_prompt_size() {
        // (prompt, completion, cache_read, cache_write, reasoning) → counts, prompt size
        let cases: [(
            (u32, u32, Option<u32>, Option<u32>, Option<u32>),
            Counts,
            u32,
        ); 5] = [
            // The owner's Copilot call.
            (
                (1549, 10, Some(1408), None, None),
                (141, 10, Some(1408), None, None),
                1549,
            ),
            // A DeepSeek call with a cache hit and reasoning.
            (
                (3465, 3959, Some(2176), None, Some(3803)),
                (1289, 3959, Some(2176), None, Some(3803)),
                3465,
            ),
            // Cache reads and writes together.
            (
                (100, 7, Some(40), Some(10), Some(5)),
                (50, 7, Some(40), Some(10), Some(5)),
                100,
            ),
            // A cache count above its prompt is clamped to it; so is reasoning.
            (
                (1, 2, Some(10), None, Some(9)),
                (0, 2, Some(1), None, Some(2)),
                1,
            ),
            (
                (100, 7, Some(90), Some(20), None),
                (0, 7, Some(90), Some(10), None),
                100,
            ),
        ];
        for ((prompt, completion, read, write, reasoning), want, size) in cases {
            let got = UsageInfo::from_inclusive_counts(prompt, completion, read, write, reasoning);
            assert_eq!(
                counts(&got),
                want,
                "{prompt}/{completion}/{read:?}/{write:?}"
            );
            assert_eq!(got.prompt_tokens(), size, "{prompt}/{read:?}/{write:?}");
            assert!(!got.counters_observed);
        }
    }

    #[test]
    fn prompt_tokens_sums_the_three_prompt_fields_and_saturates() {
        let u = UsageInfo {
            input_tokens: 141,
            output_tokens: 10,
            cache_creation_input_tokens: Some(2000),
            cache_read_input_tokens: Some(1408),
            reasoning_tokens: None,
            counters_observed: false,
        };
        assert_eq!(u.prompt_tokens(), 3549);
        let big = UsageInfo {
            input_tokens: u32::MAX,
            cache_read_input_tokens: Some(1),
            ..u
        };
        assert_eq!(big.prompt_tokens(), u32::MAX);
    }

    #[test]
    fn ledger_row_copies_the_five_counts_and_the_parent_context() {
        let u = UsageInfo {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: Some(3),
            cache_read_input_tokens: Some(4),
            reasoning_tokens: Some(5),
            counters_observed: false,
        };
        let row = TokenUsage::from_usage(&u, 7);
        assert_eq!(
            (
                row.tokens_in,
                row.tokens_out,
                row.cache_creation_tokens,
                row.cache_read_tokens,
                row.reasoning_tokens,
                row.parent_ctx
            ),
            (1, 2, Some(3), Some(4), Some(5), 7)
        );
    }

    #[test]
    fn normalized_usage_neither_serializes_nor_trusts_the_observation_flag() {
        let mut usage = UsageInfo::from_inclusive_counts(3, 2, Some(1), None, Some(1));
        usage.counters_observed = true;
        let expected = serde_json::json!({
            "inputTokens": 2,
            "outputTokens": 2,
            "cacheCreationInputTokens": null,
            "cacheReadInputTokens": 1,
            "reasoningTokens": 1
        });
        let mut json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json, expected);
        json["countersObserved"] = true.into();
        let decoded: UsageInfo = serde_json::from_value(json).unwrap();
        assert_eq!(counts(&decoded), counts(&usage));
        assert!(!decoded.counters_observed);
    }

    #[test]
    fn ledger_row_provenance_is_compatible_in_both_directions() {
        // Exact pre-provenance row schema used to decode JSONL.
        #[derive(Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LegacyRow {
            timestamp_ms: i64,
            session_id: String,
            conversation_id: String,
            provider_id: String,
            model: String,
            tier: ModelTier,
            step_kind: Option<StepKind>,
            escalation_reason: EscalationReason,
            usage: TokenUsage,
        }

        let old_json = r#"{"timestampMs":1747238400000,"sessionId":"s","conversationId":"c","providerId":"router","model":"m","tier":"cheap_agentic","stepKind":"edit","escalationReason":"none","usage":{"tokensIn":0,"tokensOut":0,"parentCtx":7,"cacheCreationTokens":null,"cacheReadTokens":null,"reasoningTokens":null}}"#;
        // Match the production reader's per-line UsageLedgerEntry decode.
        let mut row: UsageLedgerEntry = serde_json::from_str(old_json).unwrap();
        assert_eq!(row.provenance, None);
        assert_eq!(serde_json::to_string(&row).unwrap(), old_json);

        for (flag, wire) in [
            (UsageProvenance::Observed, "observed"),
            (UsageProvenance::Unknown, "unknown"),
        ] {
            row.provenance = Some(flag);
            let new_json = serde_json::to_string(&row).unwrap();
            let decoded: UsageLedgerEntry = serde_json::from_str(&new_json).unwrap();
            assert_eq!(decoded.provenance, Some(flag));
            let new_value: serde_json::Value = serde_json::from_str(&new_json).unwrap();
            assert_eq!(new_value["provenance"], wire);

            let legacy: LegacyRow = serde_json::from_str(&new_json).unwrap();
            assert_eq!(serde_json::to_string(&legacy).unwrap(), old_json);
        }
    }
}
