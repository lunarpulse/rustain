//! Pricing configuration for cost-tracking (Story 7.5 AC1).
//!
//! Per-model rates (USD per 1,000,000 tokens) shipped via `[pricing.<model_id>]`
//! TOML sections on `AppConfig`. The structure mirrors the on-the-wire
//! tokens-emitted shape so `cost_calculator` can do straight-line arithmetic
//! over a `UsageLedgerEntry`.
//!
//! ## Multiplier defaults (consumed by `cost_calculator`, NOT by serde)
//!
//! When `cache_creation_per_million` / `cache_read_per_million` / `reasoning_per_million`
//! are `None`, `cost_calculator` substitutes published multipliers:
//!
//! - **`cache_creation_per_million` → 1.25 × `input_per_million`** — Anthropic's
//!   published cache-write surcharge.
//! - **`cache_read_per_million` → 0.10 × `input_per_million`** — Anthropic's
//!   90%-off cache-read discount.
//! - **`reasoning_per_million` → `output_per_million`** — Anthropic + OpenAI bill
//!   reasoning tokens at the output rate.
//!
//! These are **calculation** defaults — they are NOT applied by `#[serde(default)]`
//! (which would force a specific concrete number into the parsed struct). The struct
//! retains `None` and the calculator decides at cost-arithmetic time.

use serde::{Deserialize, Serialize};

/// Per-model pricing rates, USD per 1,000,000 tokens (Story 7.5 AC1).
///
/// **Serialization:** snake_case canonical (matches TOML idiom + user configs).
/// The prior `rename_all = "camelCase"` + per-field snake_case alias dual-form
/// support was dropped post-Epic-7 to fix a figment merge conflict — when both
/// the defaults layer (camelCase canonical) and a user TOML layer (snake_case
/// alias) define the same field, figment produces a value tree with both keys
/// and serde rejects it as a duplicate field. camelCase is preserved as an
/// alias for any existing JSON-format configs. See Epic 7 retro AI-7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingConfig {
    /// Cost in USD per million **input** tokens.
    #[serde(alias = "inputPerMillion")]
    pub input_per_million: f64,
    /// Cost in USD per million **output** tokens.
    #[serde(alias = "outputPerMillion")]
    pub output_per_million: f64,
    /// Cost in USD per million **cache-creation** tokens (Anthropic prompt caching).
    /// When `None`, `cost_calculator` defaults to `1.25 × input_per_million`.
    #[serde(default, alias = "cacheCreationPerMillion")]
    pub cache_creation_per_million: Option<f64>,
    /// Cost in USD per million **cache-read** tokens (Anthropic prompt caching).
    /// When `None`, `cost_calculator` defaults to `0.10 × input_per_million`.
    #[serde(default, alias = "cacheReadPerMillion")]
    pub cache_read_per_million: Option<f64>,
    /// Cost in USD per million **reasoning** tokens (extended thinking).
    /// When `None`, `cost_calculator` defaults to `output_per_million`.
    #[serde(default, alias = "reasoningPerMillion")]
    pub reasoning_per_million: Option<f64>,
}
