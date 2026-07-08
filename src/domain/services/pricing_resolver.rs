//! Effective-pricing resolution for the cost calculator.
//!
//! Bridges three pricing sources into one lookup with a clear precedence.
//!
//! # Precedence
//!
//! 1. **`config.pricing`** (highest) — the figment-merged result of the bundled
//!    `default_pricing_catalog()` and any user `[pricing.<model>]` overrides.
//!    Because figment field-level-merges user entries onto the bundled defaults
//!    (Story 8.1, asserted by `infrastructure/config.rs` tests), this map already
//!    expresses "user wins, bundled fills the rest". We honour that by checking
//!    it FIRST.
//! 2. **models.dev live cache** (fills gaps) — canonical-upstream per-million
//!    prices for models the config does not cover. This is what turns `n/a`
//!    into a real USD cost for models like `gemini-3.5-flash`.
//! 3. `None` → the calculator renders `n/a` (AC6 graceful degradation).
//!
//! # Model-id canonicalization
//!
//! The usage ledger records whatever id the provider sent — sometimes
//! provider-prefixed (`google/gemini-3.5-flash` via OpenRouter), sometimes bare
//! (`gemini-3.5-flash`). [`lookup_pricing`] tries the exact id first, then
//! strips a single `<provider>/` prefix, so both shapes resolve against the same
//! effective map.

use std::collections::HashMap;

use crate::domain::models::pricing::PricingConfig;

/// Build the effective pricing map: `config_pricing` wins, `models_dev` fills
/// any model id not already present.
///
/// `models_dev` is expected to be the **canonical-upstream, bare-keyed** map
/// produced by [`crate::adapters::models_dev`] (gateway markup entries filtered
/// out, prefixes stripped). Passing a raw models.dev blob here would let gateway
/// prices leak in — do the canonical reduction at the adapter boundary.
pub fn resolve_effective_pricing(
    config_pricing: &HashMap<String, PricingConfig>,
    models_dev: Option<&HashMap<String, PricingConfig>>,
) -> HashMap<String, PricingConfig> {
    let mut effective = config_pricing.clone();
    if let Some(md) = models_dev {
        for (id, price) in md {
            // entry().or_insert() = config wins, models.dev only fills gaps.
            effective.entry(id.clone()).or_insert_with(|| price.clone());
        }
    }
    effective
}

/// Strip a single leading `<segment>/` provider prefix, returning the bare id.
/// `google/gemini-3.5-flash` → `gemini-3.5-flash`; `gemini-3.5-flash` → itself.
///
/// Uses `split_once('/')` (first segment is the provider/org) so a multi-segment
/// id keeps its tail intact (`mistralai/devstral-2512` → `devstral-2512`).
pub fn bare_model_id(model_id: &str) -> &str {
    match model_id.split_once('/') {
        Some((_, bare)) => bare,
        None => model_id,
    }
}
/// Resolve a ledger model id against the effective pricing map.
///
/// Tries the exact id first (so a user override keyed by a prefixed id, or a
/// models.dev entry keyed by a prefixed id, still matches), then falls back to
/// the bare id. Returns `None` when neither matches → caller renders `n/a`.
pub fn lookup_pricing<'a>(
    effective: &'a HashMap<String, PricingConfig>,
    model_id: &str,
) -> Option<&'a PricingConfig> {
    if let Some(p) = effective.get(model_id) {
        return Some(p);
    }
    let bare = bare_model_id(model_id);
    if bare != model_id {
        return effective.get(bare);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(in_: f64, out: f64) -> PricingConfig {
        PricingConfig {
            input_per_million: in_,
            output_per_million: out,
            cache_creation_per_million: None,
            cache_read_per_million: None,
            reasoning_per_million: None,
        }
    }

    fn map<const N: usize>(entries: [(&str, PricingConfig); N]) -> HashMap<String, PricingConfig> {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn resolve_config_wins_over_models_dev() {
        let config = map([("gemini-3.5-flash", price(99.0, 99.0))]);
        let md = map([("gemini-3.5-flash", price(1.5, 9.0))]);
        let eff = resolve_effective_pricing(&config, Some(&md));
        // config entry present → models.dev must NOT overwrite it.
        assert_eq!(eff.get("gemini-3.5-flash").unwrap().input_per_million, 99.0);
    }

    #[test]
    fn resolve_models_dev_fills_missing_keys() {
        let config = map([("gpt-4o", price(2.5, 10.0))]);
        let md = map([("gemini-3.5-flash", price(1.5, 9.0))]);
        let eff = resolve_effective_pricing(&config, Some(&md));
        // config key preserved.
        assert_eq!(eff.get("gpt-4o").unwrap().input_per_million, 2.5);
        // models.dev gap-fill added.
        assert_eq!(eff.get("gemini-3.5-flash").unwrap().input_per_million, 1.5);
    }

    #[test]
    fn resolve_none_models_dev_returns_config_as_is() {
        let config = map([("gpt-4o", price(2.5, 10.0))]);
        let eff = resolve_effective_pricing(&config, None);
        assert_eq!(eff.len(), 1);
        assert!(eff.contains_key("gpt-4o"));
    }

    #[test]
    fn bare_model_id_strips_single_prefix() {
        assert_eq!(bare_model_id("google/gemini-3.5-flash"), "gemini-3.5-flash");
        assert_eq!(
            bare_model_id("anthropic/claude-haiku-4-5"),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn bare_model_id_passes_through_unprefixed() {
        assert_eq!(bare_model_id("gemini-3.5-flash"), "gemini-3.5-flash");
        assert_eq!(bare_model_id("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn bare_model_id_keeps_tail_on_multi_segment() {
        assert_eq!(bare_model_id("a/b/c"), "b/c");
    }

    #[test]
    fn lookup_exact_match_wins() {
        let eff = map([("google/gemini-3.5-flash", price(1.5, 9.0))]);
        let p = lookup_pricing(&eff, "google/gemini-3.5-flash").unwrap();
        assert_eq!(p.output_per_million, 9.0);
    }

    #[test]
    fn lookup_strips_prefix_to_bare_key() {
        // Ledger records prefixed id; effective map keyed by bare id.
        let eff = map([("gemini-3.5-flash", price(1.5, 9.0))]);
        let p = lookup_pricing(&eff, "google/gemini-3.5-flash").unwrap();
        assert_eq!(p.input_per_million, 1.5);
    }

    #[test]
    fn lookup_returns_none_when_truly_missing() {
        let eff = map([("gpt-4o", price(2.5, 10.0))]);
        assert!(lookup_pricing(&eff, "totally-unknown-model").is_none());
        assert!(lookup_pricing(&eff, "vendor/totally-unknown-model").is_none());
    }
}
