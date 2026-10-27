//! models.dev live pricing and provider-aware model metadata adapter.
//!
//! Fetches `https://models.dev/api.json` (a single JSON blob: provider → model →
//! cost), reduces it to a **canonical-upstream, bare-keyed** `HashMap<String,
//! PricingConfig>`, and caches the result to disk with a TTL. The
//! `pricing_resolver` then merges this cache under `config.pricing` so the usage
//! & cost dashboard shows real USD instead of `n/a` for models the bundled
//! catalog doesn't cover.
//! The same download also supplies sparse model metadata, keyed by rustain
//! provider id, for discovery merges. Pricing retains its canonical bare-id rules.
//!
//! # Canonical-upstream selection
//!
//! models.dev lists the same model under many gateways (vercel, zenmux, …) with
//! markups. We keep ONLY the canonical provider's entry: the provider named by
//! the model-id prefix (`google/gemini-3.5-flash` → `google`), or — for bare ids
//! (`gemini-3.5-flash`) — the provider the model is listed under. Gateway entries
//! are dropped. Keys are stripped to the bare id so both prefixed and bare ledger
//! ids resolve.
//!
//! # Offline / failure
//!
//! Fetch failures are non-fatal: callers fall back to the last-good cache, then
//! to the bundled `default_pricing_catalog()`, then to `n/a` (AC6). Nothing here
//! panics or blocks startup.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::adapters::model_filter::is_noise_model_id;
use crate::domain::models::AppConfig;
use crate::domain::models::pricing::PricingConfig;
use crate::domain::models::provider::{ModelCapability, ModelRecord};
use crate::domain::services::pricing_resolver::bare_model_id;
use crate::domain::services::pricing_resolver::resolve_effective_pricing;
use crate::infrastructure::paths;
use crate::infrastructure::utils::env_var_trimmed;

/// Default source URL. Overridable via `RUSTAIN_MODELS_DEV_URL` for tests/CI.
pub const DEFAULT_SOURCE: &str = "https://models.dev";

/// Disk-cache schema version — bump on incompatible `PricingCache` changes.
const CACHE_VERSION: u32 = 1;

/// Freshness window for the on-disk cache (mirrors opencode's 5-minute TTL).
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Persisted pricing snapshot. `fetched_at_unix` drives staleness checks so a
/// copied cache file still ages correctly (mtime alone is fragile across copies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingCache {
    pub version: u32,
    pub fetched_at_unix: i64,
    pub pricing: HashMap<String, PricingConfig>,
    #[serde(default)]
    pub catalog: BTreeMap<String, Vec<ModelRecord>>,
}

impl PricingCache {
    /// Wall-clock age of this snapshot. `Duration::MAX` if the timestamp is
    /// implausible (treated as always-stale so a refresh is attempted).
    pub fn age(&self) -> Duration {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // `checked_sub` avoids an i64 overflow panic when a corrupt/hand-edited
        // cache carries an extreme `fetched_at_unix` (e.g. near i64::MIN).
        // Any implausible value → treat as always-stale so a refresh is attempted.
        match now.checked_sub(self.fetched_at_unix) {
            Some(elapsed) if elapsed >= 0 => Duration::from_secs(elapsed as u64),
            _ => Duration::MAX,
        }
    }

    /// True when the snapshot is older than `ttl` (or the wrong schema version).
    pub fn is_stale(&self, ttl: Duration) -> bool {
        self.version != CACHE_VERSION || self.age() > ttl
    }
}

// ─── Wire-format schema (relaxed: unknown fields ignored, optionals defaulted) ─

#[derive(Debug, Deserialize)]
/// Top-level models.dev payload. Values are kept as `serde_json::Value` so a
/// future non-provider metadata key (e.g. `_version`) doesn't fail the whole
/// parse — non-provider entries are skipped during reduction.
struct Catalog(HashMap<String, serde_json::Value>);

#[derive(Debug, Deserialize)]
struct Provider {
    #[serde(default)]
    models: HashMap<String, Model>,
}

#[derive(Debug, Deserialize)]
struct Model {
    #[serde(default)]
    cost: Option<Cost>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct Cost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
    /// models.dev `cache_write` ≈ Anthropic cache-creation (writing the cache).
    #[serde(default)]
    cache_write: Option<f64>,
}

impl From<Cost> for PricingConfig {
    fn from(c: Cost) -> Self {
        PricingConfig {
            input_per_million: c.input,
            output_per_million: c.output,
            cache_creation_per_million: c.cache_write,
            cache_read_per_million: c.cache_read,
            // models.dev exposes no separate reasoning rate; cost_calculator
            // defaults reasoning to the output rate when this is None.
            reasoning_per_million: None,
        }
    }
}

/// Reduce a raw models.dev `api.json` payload into a canonical-upstream,
/// bare-keyed pricing map. Pure + testable (no network, no disk).
pub fn reduce_catalog(raw_json: &str) -> Result<HashMap<String, PricingConfig>> {
    let catalog: Catalog = serde_json::from_str(raw_json).context("parse models.dev api.json")?;
    reduce_pricing(&catalog)
}

fn reduce_pricing(catalog: &Catalog) -> Result<HashMap<String, PricingConfig>> {
    // Iterate providers in SORTED key order so a bare-id collision between two
    // canonical providers resolves deterministically across runs (HashMap
    // iteration order is randomized). Alphabetically-first provider wins.
    let mut providers: Vec<(&String, &serde_json::Value)> = catalog.0.iter().collect();
    providers.sort_by_key(|(k, _)| k.as_str());
    let mut out: HashMap<String, PricingConfig> = HashMap::new();
    for (provider_id, value) in providers {
        // Skip non-provider top-level entries (e.g. a future `_metadata` key)
        // whose value isn't a Provider-shaped object.
        let Ok(provider) = serde_json::from_value::<Provider>(value.clone()) else {
            continue;
        };
        for (model_id, model) in &provider.models {
            let Some(cost) = model.cost.as_ref() else {
                continue;
            };
            let canonical = match model_id.split_once('/') {
                Some((prefix, _)) => prefix,
                None => provider_id.as_str(),
            };
            if canonical != provider_id.as_str() {
                continue; // gateway entry — keep the canonical provider's copy only
            }
            let bare = bare_model_id(model_id);
            // entry().or_insert = first (alphabetically-earliest) canonical
            // provider wins on collision, deterministically.
            out.entry(bare.to_string())
                .or_insert_with(|| cost.clone().into());
        }
    }
    Ok(out)
}

/// Supported rustain provider ids and their models.dev source keys.
/// Codex has no source key in models.dev; OpenAI API-only rows are not a
/// subscription roster and must not be mapped into its bundled catalogue.
pub const MODELS_DEV_PROVIDERS: &[(&str, &str)] = &[
    ("anthropic", "anthropic"),
    ("openai", "openai"),
    ("google", "google"),
    ("deepseek", "deepseek"),
    ("moonshot", "moonshotai"),
    ("openrouter", "openrouter"),
];

/// Reduce model metadata without stripping provider prefixes from model ids.
pub fn reduce_models(raw_json: &str) -> Result<BTreeMap<String, Vec<ModelRecord>>> {
    let catalog: Catalog = serde_json::from_str(raw_json).context("parse models.dev api.json")?;
    Ok(reduce_model_metadata(&catalog))
}

fn reduce_model_metadata(catalog: &Catalog) -> BTreeMap<String, Vec<ModelRecord>> {
    let mut out = BTreeMap::new();
    for &(provider_id, source_key) in MODELS_DEV_PROVIDERS {
        let Some(models) = catalog
            .0
            .get(source_key)
            .and_then(|provider| provider.get("models"))
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        let mut records = Vec::new();
        for (model_id, model) in models {
            if !model.is_object()
                || model.get("status").and_then(serde_json::Value::as_str) == Some("deprecated")
                || is_noise_model_id(model_id)
            {
                continue;
            }
            let mut capabilities = HashSet::new();
            let tool_call = model.get("tool_call").and_then(serde_json::Value::as_bool);
            let reasoning = model.get("reasoning").and_then(serde_json::Value::as_bool);
            let input = model
                .get("modalities")
                .and_then(|m| m.get("input"))
                .and_then(serde_json::Value::as_array);
            if tool_call == Some(true) {
                capabilities.insert(ModelCapability::ToolUse);
            }
            if reasoning == Some(true) {
                capabilities.insert(ModelCapability::Thinking);
            }
            if input
                .is_some_and(|modalities| modalities.iter().any(|m| m.as_str() == Some("image")))
            {
                capabilities.insert(ModelCapability::Vision);
            }
            records.push(ModelRecord {
                model_id: model_id.clone(),
                display_name: model
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                context_window: model
                    .get("limit")
                    .and_then(|limit| limit.get("context"))
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                capabilities: (tool_call.is_some() || reasoning.is_some() || input.is_some())
                    .then_some(capabilities),
                pricing_tier: None,
            });
        }
        records.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        out.insert(provider_id.to_owned(), records);
    }
    out
}

/// Read a provider's metadata from the existing pricing cache without network I/O.
pub fn catalog_for(provider_id: &str) -> Vec<ModelRecord> {
    load_cache()
        .and_then(|mut cache| cache.catalog.remove(provider_id))
        .unwrap_or_default()
}

/// Load the on-disk cache. Returns `None` if missing, unreadable, or the wrong
/// schema version — callers then fall back to bundled pricing.
pub fn load_cache() -> Option<PricingCache> {
    let path = paths::models_dev_pricing_path().ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let cache: PricingCache = serde_json::from_slice(&bytes).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    Some(cache)
}

/// Persist a pricing snapshot to disk ATOMICALLY (write-to-tmp + rename), so a
/// crash mid-write or a concurrent reader can never observe a partial file.
/// Mirrors the `ModelCatalogCache::save` pattern in this crate.
pub fn save_cache(cache: &PricingCache) -> Result<()> {
    let path = paths::models_dev_pricing_path()?;
    let json = serde_json::to_vec(cache).context("serialize pricing cache")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

/// Merge the on-disk models.dev cache into `config.pricing` (config wins,
/// models.dev fills gaps). No network — reads only the cached snapshot. Safe to
/// call at config-load time; missing/unreadable cache is a silent no-op so the
/// bundled catalog remains in effect.
pub fn merge_into_config(config: &mut AppConfig) {
    let Some(cache) = load_cache() else {
        return;
    };
    config.pricing = resolve_effective_pricing(&config.pricing, Some(&cache.pricing));
}

/// Fetch the live catalog, reduce it, persist it, and return the fresh cache.
///
/// Network is bounded by a 10s timeout and a single retry (matches the
/// `update-catalog` CLI posture). Failures propagate so the caller can log and
/// fall back to the existing cache / bundled pricing.
pub async fn refresh() -> Result<PricingCache> {
    let source =
        env_var_trimmed("RUSTAIN_MODELS_DEV_URL").unwrap_or_else(|| DEFAULT_SOURCE.to_string());
    let url = format!("{}/api.json", source.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..2u8 {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                // A 200 with an unparseable body (CDN error page, schema drift)
                // must consume the retry budget, not bypass it — so parse + save
                // failures set `last_err` and continue, not `?`-propagate.
                let outcome: anyhow::Result<_> = async {
                    let text = resp.text().await.context("read models.dev response")?;
                    let payload: Catalog =
                        serde_json::from_str(&text).context("parse models.dev api.json")?;
                    Ok((reduce_pricing(&payload)?, reduce_model_metadata(&payload)))
                }
                .await;
                match outcome {
                    Ok((pricing, catalog)) => {
                        // Build the cache ONCE so the persisted and returned
                        // snapshots share a single timestamp.
                        let cache = PricingCache {
                            version: CACHE_VERSION,
                            fetched_at_unix: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0),
                            pricing,
                            catalog,
                        };
                        match save_cache(&cache) {
                            Ok(()) => return Ok(cache),
                            Err(e) => last_err = Some(e),
                        }
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Ok(resp) => {
                last_err = Some(anyhow::anyhow!("models.dev HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(e.into());
            }
        }
        if attempt == 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let err = last_err.unwrap_or_else(|| anyhow::anyhow!("unknown models.dev fetch error"));
    Err(err.context("models.dev fetch failed after retry"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_keeps_provider_association_and_prefixed_ids() {
        let raw = r#"{
            "openai":{"models":{"shared":{"name":"Direct"}}},
            "openrouter":{"models":{"shared":{"name":"Gateway"},"openai/gpt-chat":{}}},
            "moonshotai":{"models":{"kimi-chat":{}}},
            "anthropic":{"models":{"claude-chat":{}}},
            "google":{"models":{"gemini-chat":{}}},
            "deepseek":{"models":{"deepseek-chat":{}}},
            "unmapped":{"models":{"ignored":{}}}
        }"#;
        let models = reduce_models(raw).unwrap();
        assert_eq!(models.len(), 6);
        assert_eq!(models["openai"][0].display_name.as_deref(), Some("Direct"));
        assert_eq!(
            models["openrouter"][1].display_name.as_deref(),
            Some("Gateway")
        );
        assert_eq!(models["openrouter"][0].model_id, "openai/gpt-chat");
        assert_eq!(models["moonshot"][0].model_id, "kimi-chat");
        assert_eq!(models["anthropic"][0].model_id, "claude-chat");
        assert_eq!(models["google"][0].model_id, "gemini-chat");
        assert_eq!(models["deepseek"][0].model_id, "deepseek-chat");
    }

    #[test]
    fn metadata_preserves_sparse_fields_and_reports_capabilities() {
        let raw = r#"{"openai":{"models":{
            "rich":{"name":"Rich","limit":{"context":32000},"tool_call":true,
                "reasoning":true,"modalities":{"input":["text","image"]}},
            "sparse":{},
            "explicit":{"name":"explicit","limit":{"context":0},"tool_call":false},
            "too-large":{"limit":{"context":4294967296}}
        }}}"#;
        let models = reduce_models(raw).unwrap();
        let rows = &models["openai"];
        let rich = rows.iter().find(|row| row.model_id == "rich").unwrap();
        assert_eq!(rich.context_window, Some(32000));
        assert_eq!(
            rich.capabilities,
            Some(HashSet::from([
                ModelCapability::ToolUse,
                ModelCapability::Thinking,
                ModelCapability::Vision,
            ]))
        );
        let sparse = rows.iter().find(|row| row.model_id == "sparse").unwrap();
        assert_eq!(sparse.display_name, None);
        assert_eq!(sparse.context_window, None);
        assert_eq!(sparse.capabilities, None);
        assert_eq!(sparse.pricing_tier, None);
        let explicit = rows.iter().find(|row| row.model_id == "explicit").unwrap();
        assert_eq!(explicit.display_name.as_deref(), Some("explicit"));
        assert_eq!(explicit.context_window, Some(0));
        assert_eq!(explicit.capabilities, Some(HashSet::new()));
        assert_eq!(
            rows.iter()
                .find(|row| row.model_id == "too-large")
                .unwrap()
                .context_window,
            None
        );
    }

    #[test]
    fn metadata_skips_deprecated_noise_and_malformed_rows() {
        let raw = r#"{"_metadata":2,"openai":{"models":{
            "chat":{"status":"active"},
            "old-chat":{"status":"deprecated"},
            "text-embedding-3":{},
            "voice-tts":{},
            "generate-image":{},
            "malformed":false
        }}}"#;
        let models = reduce_models(raw).unwrap();
        assert_eq!(models["openai"].len(), 1);
        assert_eq!(models["openai"][0].model_id, "chat");
        assert!(reduce_models("{bad").is_err());
    }

    #[test]
    fn old_pricing_cache_has_empty_metadata_and_roundtrips_additive_catalog() {
        let raw = r#"{"version":1,"fetched_at_unix":10,"pricing":{"chat":{"input_per_million":1.0,"output_per_million":2.0}}}"#;
        let mut cache: PricingCache = serde_json::from_str(raw).unwrap();
        assert!(cache.catalog.is_empty());
        assert_eq!(cache.pricing["chat"].input_per_million, 1.0);
        cache.catalog = reduce_models(r#"{"openai":{"models":{"chat":{}}}}"#).unwrap();
        let roundtrip: PricingCache =
            serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
        assert_eq!(roundtrip.version, 1);
        assert_eq!(roundtrip.catalog["openai"][0].model_id, "chat");
        assert_eq!(roundtrip.pricing["chat"].output_per_million, 2.0);
    }

    #[test]
    fn reduce_keeps_canonical_bare_entry() {
        // google lists gemini-3.5-flash bare under provider "google" (canonical).
        let raw = r#"{"google":{"models":{"gemini-3.5-flash":{"cost":{"input":1.5,"output":9,"cache_read":0.15}}}}}"#;
        let m = reduce_catalog(raw).unwrap();
        let p = m
            .get("gemini-3.5-flash")
            .expect("canonical bare entry kept");
        assert_eq!(p.input_per_million, 1.5);
        assert_eq!(p.output_per_million, 9.0);
        assert_eq!(p.cache_read_per_million, Some(0.15));
        assert_eq!(p.cache_creation_per_million, None);
    }

    #[test]
    fn reduce_drops_gateway_markup_entries() {
        // vercel lists google/gemini-3.5-flash (prefixed) — canonical is google,
        // not vercel → dropped even though no separate google entry exists here.
        let raw = r#"{"vercel":{"models":{"google/gemini-3.5-flash":{"cost":{"input":1.5,"output":9}}}}}"#;
        let m = reduce_catalog(raw).unwrap();
        assert!(m.is_empty(), "gateway entry must be dropped, got {m:?}");
    }

    #[test]
    fn reduce_prefers_canonical_when_both_present() {
        // google (canonical, bare) + vercel (gateway, prefixed) for same model.
        let raw = r#"{
            "google":{"models":{"gemini-3.5-flash":{"cost":{"input":1.5,"output":9}}}},
            "vercel":{"models":{"google/gemini-3.5-flash":{"cost":{"input":99.0,"output":99.0}}}}
        }"#;
        let m = reduce_catalog(raw).unwrap();
        // Canonical google price wins; vercel markup ignored.
        assert_eq!(m.get("gemini-3.5-flash").unwrap().input_per_million, 1.5);
    }

    #[test]
    fn reduce_skips_models_without_cost() {
        let raw = r#"{"qiniu-ai":{"models":{"kimi-k2":{}}}}"#;
        let m = reduce_catalog(raw).unwrap();
        assert!(m.is_empty(), "costless model must be skipped");
    }

    #[test]
    fn reduce_maps_cache_write_to_cache_creation() {
        let raw = r#"{"anthropic":{"models":{"claude-haiku-4-5":{"cost":{"input":1,"output":5,"cache_read":0.1,"cache_write":1.25}}}}}"#;
        let m = reduce_catalog(raw).unwrap();
        let p = m.get("claude-haiku-4-5").unwrap();
        assert_eq!(p.cache_creation_per_million, Some(1.25));
        assert_eq!(p.cache_read_per_million, Some(0.1));
    }

    #[test]
    fn reduce_ignores_unknown_fields() {
        // Extra fields (limit, modalities, …) must not break deserialization.
        let raw = r#"{"google":{"models":{"gemini-3.5-flash":{
            "cost":{"input":1.5,"output":9},
            "limit":{"context":1048576,"output":65536},
            "modalities":{"input":["text","image"]}
        }}}}"#;
        let m = reduce_catalog(raw).unwrap();
        assert!(m.contains_key("gemini-3.5-flash"));
    }

    #[test]
    fn reduce_skips_non_provider_top_level_keys() {
        // A future non-provider metadata key must not fail the whole parse.
        let raw = r#"{"_metadata":{"schema":2},"google":{"models":{"gemini-3.5-flash":{"cost":{"input":1.5,"output":9}}}}}"#;
        let m = reduce_catalog(raw).unwrap();
        assert!(
            m.contains_key("gemini-3.5-flash"),
            "provider entry survives"
        );
    }

    #[test]
    fn reduce_collision_picks_alphabetically_first_provider() {
        // Two canonical providers list the same bare id at different prices.
        // Sorted iteration → alphabetically-earliest provider wins (deterministic).
        let raw = r#"{
            "deepseek":{"models":{"deepseek-v4-flash":{"cost":{"input":0.27,"output":0.28}}}},
            "deepinfra":{"models":{"deepseek-v4-flash":{"cost":{"input":0.55,"output":0.99}}}}
        }"#;
        let m = reduce_catalog(raw).unwrap();
        // "deepinfra" < "deepseek" → deepinfra's price wins.
        assert_eq!(
            m.get("deepseek-v4-flash").unwrap().input_per_million,
            0.55,
            "alphabetically-first canonical provider must win the collision"
        );
    }

    #[test]
    fn age_handles_corrupt_timestamp_without_overflow() {
        // A corrupt/hand-edited fetched_at_unix near i64::MIN must not panic on
        // subtraction; it is treated as always-stale (Duration::MAX).
        let corrupt = PricingCache {
            version: CACHE_VERSION,
            fetched_at_unix: i64::MIN,
            pricing: HashMap::new(),
            catalog: Default::default(),
        };
        assert_eq!(corrupt.age(), Duration::MAX);
        assert!(corrupt.is_stale(CACHE_TTL));
    }

    #[test]
    fn cache_is_stale_after_ttl() {
        let old = PricingCache {
            version: CACHE_VERSION,
            fetched_at_unix: 0, // epoch → very old
            pricing: HashMap::new(),
            catalog: Default::default(),
        };
        assert!(old.is_stale(CACHE_TTL));
    }

    #[test]
    fn cache_wrong_version_is_stale() {
        let wrong = PricingCache {
            version: 999,
            fetched_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            pricing: HashMap::new(),
            catalog: Default::default(),
        };
        assert!(wrong.is_stale(CACHE_TTL));
    }

    /// Live end-to-end smoke test: fetch the real models.dev catalog, reduce it,
    /// merge against the bundled config, and confirm the user's actual `n/a`
    /// models resolve with real pricing. `#[ignore]` — needs network; run with
    /// `cargo test --lib -- --ignored models_dev_live`.
    #[tokio::test]
    #[ignore]
    async fn models_dev_live_resolves_user_na_models() {
        use crate::domain::models::AppConfig;
        use crate::domain::services::pricing_resolver::{
            lookup_pricing, resolve_effective_pricing,
        };

        let cache = refresh().await.expect("live models.dev fetch");
        // The user's dashboard `n/a` models must now carry canonical pricing.
        for bare in ["gemini-3.5-flash", "claude-haiku-4-5", "deepseek-v4-flash"] {
            assert!(
                cache.pricing.contains_key(bare),
                "models.dev missing canonical price for {bare}"
            );
        }

        // Full merge chain: bundled config + models.dev → prefixed ledger id resolves.
        let mut bundled = AppConfig::default_pricing_catalog();
        let effective = resolve_effective_pricing(&bundled, Some(&cache.pricing));
        // Prefixed ledger id (OpenRouter shape) → strips to bare → models.dev price.
        let gemini = lookup_pricing(&effective, "google/gemini-3.5-flash")
            .expect("prefixed gemini id resolves via models.dev");
        assert!(gemini.input_per_million > 0.0, "real input price, not n/a");
        // Bundled entry still wins for an overlapping key (config > models.dev).
        let _ = bundled.insert(
            "gemini-3.5-flash".to_string(),
            PricingConfig {
                input_per_million: 999.0,
                output_per_million: 999.0,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        let effective2 = resolve_effective_pricing(&bundled, Some(&cache.pricing));
        assert_eq!(
            lookup_pricing(&effective2, "google/gemini-3.5-flash")
                .unwrap()
                .input_per_million,
            999.0,
            "config override must win over models.dev"
        );
    }
}
