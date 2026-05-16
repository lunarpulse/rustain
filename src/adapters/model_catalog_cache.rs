//! Disk cache for discovered model catalogs.
//!
//! Story 7.6 AC4 — three-tier catalog system:
//! Tier 0: bundled snapshot (`variant.known_models()`)
//! Tier 1: disk cache (`~/.rustain/models_cache.json`)
//! Tier 2: live `/v1/models` fetch

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::domain::models::provider::ModelDescriptor;
use crate::infrastructure::paths;

/// On-disk cache schema version.
const CURRENT_VERSION: u32 = 2;

/// In-memory representation of the cached catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedCatalog {
    pub version: u32,
    pub providers: HashMap<String, CachedProviderEntry>,
}

impl Default for CachedCatalog {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            providers: HashMap::new(),
        }
    }
}

/// Per-provider cached entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedProviderEntry {
    pub fetched_at_unix: i64,
    pub models: Vec<CachedModelEntry>,
}

/// A single cached model, carrying the ghost-marker (`stale`) via the flattened
/// `ModelDescriptor`. The `stale` field lives in `ModelDescriptor` so the UI
/// can render it without an adapter-layer wrapper (Story 7.6 review patch).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedModelEntry {
    #[serde(flatten)]
    pub descriptor: ModelDescriptor,
}

/// Merge live fetch results with cached entries, marking ghosts as stale.
/// Story 7.6 AC4 — Preflight Consensus #9.
pub fn merge_with_live(
    cached: Option<&CachedProviderEntry>,
    live: &[crate::domain::models::provider::ModelDescriptor],
) -> Vec<CachedModelEntry> {
    use std::collections::HashSet;

    let mut result: Vec<CachedModelEntry> = Vec::new();
    let live_ids: HashSet<&str> = live.iter().map(|m| m.model_id.as_str()).collect();

    // Update existing cached entries
    if let Some(cached_entry) = cached {
        for cached_model in &cached_entry.models {
            if live_ids.contains(cached_model.descriptor.model_id.as_str()) {
                let live_model = live
                    .iter()
                    .find(|m| m.model_id == cached_model.descriptor.model_id)
                    .expect("live_ids and live are in sync");
                result.push(CachedModelEntry {
                    descriptor: live_model.clone(),
                });
            } else {
                let mut ghost = cached_model.descriptor.clone();
                ghost.stale = true;
                result.push(CachedModelEntry {
                    descriptor: ghost,
                });
            }
        }
    }

    // Add new live models not in cache
    let cached_ids: HashSet<&str> = cached
        .map(|c| c.models.iter().map(|m| m.descriptor.model_id.as_str()).collect())
        .unwrap_or_default();
    for live_model in live {
        if !cached_ids.contains(live_model.model_id.as_str()) {
            result.push(CachedModelEntry {
                descriptor: live_model.clone(),
            });
        }
    }

    result
}

/// Cache manager — resolves path per call. Holds a write lock so that
/// concurrent load-modify-save sequences don't lose updates (Story 7.6 AC4).
#[derive(Debug, Clone)]
pub struct ModelCatalogCache {
    write_lock: Arc<Mutex<()>>,
}

impl Default for ModelCatalogCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCatalogCache {
    pub fn new() -> Self {
        Self {
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Acquire the write lock for atomic load-modify-save sequences.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().await
    }

    /// Load the cache from disk. Returns empty default on missing or corrupt file.
    pub async fn load(&self) -> CachedCatalog {
        let path = match paths::models_cache_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("models_cache_path error: {e}; using empty catalog");
                return CachedCatalog::default();
            }
        };

        match tokio::fs::read_to_string(&path).await {
            Ok(text) => match serde_json::from_str::<CachedCatalog>(&text) {
                Ok(catalog) => {
                    if catalog.version > CURRENT_VERSION {
                        tracing::info!(
                            "models_cache.json version {} differs from current {}; rebuilding",
                            catalog.version,
                            CURRENT_VERSION
                        );
                        return CachedCatalog::default();
                    }
                    catalog
                }
                Err(e) => {
                    tracing::warn!("models_cache.json parse error: {e}; rebuilding");
                    CachedCatalog::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CachedCatalog::default(),
            Err(e) => {
                tracing::warn!("models_cache.json read error: {e}; using empty catalog");
                CachedCatalog::default()
            }
        }
    }

    /// Atomically write the cache to disk.
    /// Callers that do load→modify→save should hold `self.lock().await` across the sequence.
    pub async fn save(&self, catalog: &CachedCatalog) -> anyhow::Result<()> {
        let path = paths::models_cache_path()?;
        let json = serde_json::to_string_pretty(catalog)?;
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, json).await?;
        if let Err(e) = tokio::fs::rename(&tmp, &path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        Ok(())
    }

    /// Check whether a provider entry is still fresh given its TTL.
    pub fn is_fresh(&self, entry: &CachedProviderEntry, ttl_seconds: u64, now_unix: i64) -> bool {
        let elapsed = now_unix.saturating_sub(entry.fetched_at_unix) as u64;
        elapsed < ttl_seconds
    }
}

/// Target for background catalog discovery.
#[cfg(feature = "openai")]
#[derive(Debug, Clone)]
pub struct DiscoveryTarget {
    pub provider_id: String,
    pub adapter: Arc<crate::adapters::openai::OpenAiAdapter>,
    pub cache_ttl_seconds: u64,
    pub model_filter: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn temp_dir() -> PathBuf {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        // Leak the TempDir so the directory survives the test
        let _ = Box::leak(Box::new(tmp));
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", &dir);
        }
        dir
    }

    #[tokio::test]
    #[serial]
    async fn save_load_roundtrip() {
        let _dir = temp_dir();
        let mut catalog = CachedCatalog::default();
        catalog.providers.insert(
            "openrouter".to_string(),
            CachedProviderEntry {
                fetched_at_unix: 1000,
                models: vec![
                    CachedModelEntry {
                        descriptor: ModelDescriptor {
                            model_id: "m1".to_string(),
                            display_name: "M1".to_string(),
                            provider_id: "openrouter".to_string(),
                            context_window: 128_000,
                            capabilities: std::collections::HashSet::new(),
                            pricing_tier: None,
                            stale: false,
                        },
                    },
                    CachedModelEntry {
                        descriptor: ModelDescriptor {
                            model_id: "m2".to_string(),
                            display_name: "M2".to_string(),
                            provider_id: "openrouter".to_string(),
                            context_window: 128_000,
                            capabilities: std::collections::HashSet::new(),
                            pricing_tier: None,
                            stale: true,
                        },
                    },
                ],
            },
        );
        let cache = ModelCatalogCache::new();
        cache.save(&catalog).await.unwrap();
        let loaded = cache.load().await;
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert_eq!(loaded.providers.len(), 1);
        let entry = loaded.providers.get("openrouter").unwrap();
        assert_eq!(entry.models.len(), 2);
        assert!(!entry.models[0].descriptor.stale);
        assert!(entry.models[1].descriptor.stale);
    }

    #[tokio::test]
    #[serial]
    async fn load_missing_file_returns_default() {
        let _dir = temp_dir();
        let cache = ModelCatalogCache::new();
        let loaded = cache.load().await;
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert!(loaded.providers.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn load_corrupt_file_returns_default() {
        let dir = temp_dir();
        let path = dir.join("models_cache.json");
        tokio::fs::write(&path, "{garbage}").await.unwrap();
        let cache = ModelCatalogCache::new();
        let loaded = cache.load().await;
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert!(loaded.providers.is_empty());
    }

    #[test]
    fn is_fresh_boundaries() {
        let cache = ModelCatalogCache::new();
        let entry = CachedProviderEntry {
            fetched_at_unix: 1000,
            models: vec![],
        };
        assert!(cache.is_fresh(&entry, 600, 1500)); // 500 < 600
        assert!(!cache.is_fresh(&entry, 600, 1600)); // exactly 600 — strict < means NOT fresh
        assert!(!cache.is_fresh(&entry, 600, 1601)); // 601 > 600
    }

    #[tokio::test]
    #[serial]
    async fn v1_file_deserializes_as_v2() {
        let dir = temp_dir();
        let path = dir.join("models_cache.json");
        // Hand-crafted v1 JSON (no `stale` keys)
        let v1_json = r#"{"version":1,"providers":{"openrouter":{"fetchedAtUnix":1000,"models":[{"modelId":"m1","displayName":"M1","providerId":"openrouter","contextWindow":128000,"capabilities":[],"pricingTier":null}]}}}"#;
        tokio::fs::write(&path, v1_json).await.unwrap();
        let cache = ModelCatalogCache::new();
        let loaded = cache.load().await;
        // v1 accepted via backward-compat; serde(default) gives stale=false
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.providers.len(), 1);
        let entry = loaded.providers.get("openrouter").unwrap();
        assert_eq!(entry.models.len(), 1);
        assert!(!entry.models[0].descriptor.stale);
    }

    #[tokio::test]
    #[serial]
    async fn merge_marks_ghosts_stale() {
        let _dir = temp_dir();
        let mut catalog = CachedCatalog::default();
        catalog.providers.insert(
            "openrouter".to_string(),
            CachedProviderEntry {
                fetched_at_unix: 1000,
                models: vec![
                    CachedModelEntry {
                        descriptor: ModelDescriptor {
                            model_id: "a".to_string(),
                            display_name: "A".to_string(),
                            provider_id: "openrouter".to_string(),
                            context_window: 128_000,
                            capabilities: std::collections::HashSet::new(),
                            pricing_tier: None,
                            stale: false,
                        },
                    },
                    CachedModelEntry {
                        descriptor: ModelDescriptor {
                            model_id: "b".to_string(),
                            display_name: "B".to_string(),
                            provider_id: "openrouter".to_string(),
                            context_window: 128_000,
                            capabilities: std::collections::HashSet::new(),
                            pricing_tier: None,
                            stale: false,
                        },
                    },
                    CachedModelEntry {
                        descriptor: ModelDescriptor {
                            model_id: "c".to_string(),
                            display_name: "C".to_string(),
                            provider_id: "openrouter".to_string(),
                            context_window: 128_000,
                            capabilities: std::collections::HashSet::new(),
                            pricing_tier: None,
                            stale: false,
                        },
                    },
                ],
            },
        );

        // Simulate Tier-2 returning only a and b
        let live_models = vec![
            ModelDescriptor {
                model_id: "a".to_string(),
                display_name: "A-new".to_string(),
                provider_id: "openrouter".to_string(),
                context_window: 128_000,
                capabilities: std::collections::HashSet::new(),
                pricing_tier: None,
            stale: false,
            },
            ModelDescriptor {
                model_id: "b".to_string(),
                display_name: "B-new".to_string(),
                provider_id: "openrouter".to_string(),
                context_window: 128_000,
                capabilities: std::collections::HashSet::new(),
                pricing_tier: None,
            stale: false,
            },
        ];

        // Merge: live models get stale=false; missing models keep stale=true
        let merged = merge_with_live(Some(catalog.providers.get("openrouter").unwrap()), &live_models);
        assert_eq!(merged.len(), 3);
        let a = merged
            .iter()
            .find(|m| m.descriptor.model_id == "a")
            .unwrap();
        let b = merged
            .iter()
            .find(|m| m.descriptor.model_id == "b")
            .unwrap();
        let c = merged
            .iter()
            .find(|m| m.descriptor.model_id == "c")
            .unwrap();
        assert!(!a.descriptor.stale);
        assert!(!b.descriptor.stale);
        assert!(c.descriptor.stale);
        assert_eq!(a.descriptor.display_name, "A-new"); // refreshed from live
    }

}
