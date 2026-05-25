//! Two-layer skill cache (hermes-agent pattern; ADR-09-02 §Phase A).
//!
//! # L1: in-memory LRU
//!
//! `tokio::sync::Mutex<lru::LruCache<String, CachedEntry>>` (~64 entries by
//! default). Bypasses YAML frontmatter parse on warm reads. Per CLAUDE.md
//! async-lock policy: uses `tokio::sync::Mutex` (NOT `std::sync::Mutex`).
//!
//! # L2: manifest-keyed disk snapshot
//!
//! Persisted at `~/.rustain/.skills_snapshot.json` (mode `0700` dir, `0600`
//! file on Unix). Manifest key is `BLAKE3(skill_directory_listing + mtime + size)`.
//! Snapshot survives across processes and warms L1 on startup.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::domain::models::SkillSource;
use crate::domain::models::skill_metadata::SkillMetadata;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub metadata: SkillMetadata,
    pub body: String,
    #[serde(skip)]
    pub file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SkillCacheConfig {
    pub l1_capacity: NonZeroUsize,
    /// L2 disk snapshot path. `None` disables L2 (in-memory only).
    pub l2_path: Option<PathBuf>,
}

impl Default for SkillCacheConfig {
    fn default() -> Self {
        Self {
            l1_capacity: NonZeroUsize::new(64).unwrap(),
            l2_path: dirs::home_dir().map(|h| h.join(".rustain/.skills_snapshot.json")),
        }
    }
}

pub struct SkillCache {
    inner: Mutex<lru::LruCache<String, CachedEntry>>,
    l2_path: Option<PathBuf>,
    current_manifest: Mutex<Option<[u8; 32]>>,
}

impl std::fmt::Debug for SkillCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillCache")
            .field("l2_path", &self.l2_path)
            .finish_non_exhaustive()
    }
}

impl SkillCache {
    pub fn new(config: SkillCacheConfig) -> Self {
        Self {
            inner: Mutex::new(lru::LruCache::new(config.l1_capacity)),
            l2_path: config.l2_path,
            current_manifest: Mutex::new(None),
        }
    }

    /// In-memory-only variant for tests (no L2 disk path).
    pub fn new_in_memory() -> Self {
        Self::new(SkillCacheConfig {
            l1_capacity: NonZeroUsize::new(16).unwrap(),
            l2_path: None,
        })
    }

    /// Populate the cache from a discovered SkillRegistry.
    pub async fn populate_from_registry(
        &self,
        registry: &crate::adapters::skill_registry::SkillRegistry,
    ) {
        let mut guard = self.inner.lock().await;
        for def in registry.skills() {
            match std::fs::read_to_string(&def.file) {
                Ok(body) => {
                    let entry = CachedEntry {
                        metadata: SkillMetadata::from_def(def),
                        body,
                        file: def.file.clone(),
                    };
                    guard.put(def.name.clone(), entry);
                }
                Err(e) => {
                    tracing::warn!(
                        "Skill cache: failed to read body for '{}' ({}): {}",
                        def.name,
                        def.file.display(),
                        e
                    );
                }
            }
        }
    }

    /// Insert a skill entry directly (used by tests).
    pub async fn insert(&self, name: &str, metadata: SkillMetadata, body: String) {
        let mut guard = self.inner.lock().await;
        guard.put(
            name.to_string(),
            CachedEntry {
                metadata,
                body,
                file: PathBuf::new(),
            },
        );
    }

    /// Look up a skill's metadata. L1 hit → return; L1 miss → return None.
    /// Uses `get()` so the LRU recency bit is updated on each access —
    /// frequently-accessed skills are less likely to be evicted.
    pub async fn metadata(&self, name: &str) -> Option<SkillMetadata> {
        let mut guard = self.inner.lock().await;
        guard.get(name).map(|e| e.metadata.clone())
    }

    /// Look up a skill's body.
    pub async fn body(&self, name: &str) -> Result<String, SkillCacheError> {
        let mut guard = self.inner.lock().await;
        guard
            .get(name)
            .map(|e| e.body.clone())
            .ok_or_else(|| SkillCacheError::SkillNotFound(name.to_string()))
    }

    /// Look up a skill's source tier (for trust-marker rendering in
    /// `skill_view`).
    pub async fn source(&self, name: &str) -> Result<SkillSource, SkillCacheError> {
        let mut guard = self.inner.lock().await;
        guard
            .get(name)
            .map(|e| e.metadata.source)
            .ok_or_else(|| SkillCacheError::SkillNotFound(name.to_string()))
    }

    /// Synchronous snapshot of all cached skill metadata (for rebuild_fn).
    /// Uses `try_lock()` — returns empty vec on contention, matching
    /// `CapabilityRegistry::snapshot()` semantics.
    pub fn try_snapshot_metadata(&self) -> Vec<SkillMetadata> {
        match self.inner.try_lock() {
            Ok(guard) => guard.iter().map(|(_, e)| e.metadata.clone()).collect(),
            Err(_) => {
                tracing::warn!(
                    "SkillCache::try_snapshot_metadata() lock contention — returning empty vec"
                );
                Vec::new()
            }
        }
    }

    /// Build the FilteredSkillCatalog from the cache.
    pub async fn snapshot_catalog(
        &self,
    ) -> crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog {
        let guard = self.inner.lock().await;
        let metas: Vec<SkillMetadata> = guard.iter().map(|(_, e)| e.metadata.clone()).collect();
        crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog::from_metadata(metas)
    }

    /// Compute BLAKE3 manifest over directory listing + per-file mtime+size.
    pub fn manifest(skill_dirs: &[&Path]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for dir in skill_dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.path());
            for entry in entries {
                let path = entry.path();
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    hasher.update(path.to_string_lossy().as_bytes());
                    hasher.update(&mtime.to_le_bytes());
                    hasher.update(&meta.len().to_le_bytes());
                }
            }
        }
        *hasher.finalize().as_bytes()
    }

    /// Persist L1 contents to L2 disk snapshot. Mode 0700 dir + 0600 file
    /// enforced on Unix.
    ///
    /// The L1 LRU lock is released BEFORE serialization and disk I/O so that
    /// concurrent readers (`body()`, `metadata()`, `source()`) are never blocked
    /// on filesystem operations.
    pub async fn save_snapshot(&self, manifest: [u8; 32]) -> Result<(), SkillCacheError> {
        let Some(l2_path) = &self.l2_path else {
            return Ok(());
        };
        // Clone entries under the lock, then release before I/O
        let entries: Vec<(String, CachedEntry)> = {
            let guard = self.inner.lock().await;
            guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        let snapshot = SnapshotFile {
            manifest_hex: hex::encode(manifest),
            entries,
        };
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| SkillCacheError::Serde(e.to_string()))?;
        if let Some(parent) = l2_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SkillCacheError::Io(e.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| SkillCacheError::Io(e.to_string()))?;
            }
        }
        std::fs::write(l2_path, json).map_err(|e| SkillCacheError::Io(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(l2_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| SkillCacheError::Io(e.to_string()))?;
        }
        let mut manifest_guard = self.current_manifest.lock().await;
        *manifest_guard = Some(manifest);
        Ok(())
    }

    /// Phase A cache warm-up: compute manifest for the given skill directories,
    /// attempt L2 disk snapshot load, fall back to populating from the registry
    /// on miss/stale, then persist back to L2.
    ///
    /// Call this after `SkillRegistry::discover()` has completed (in
    /// `event_loop.rs`) so the registry's `skills()` list is populated.
    /// The function is idempotent — subsequent calls with the same manifest
    /// are a fast no-op (L2 hit → no I/O beyond the snapshot read).
    pub async fn warm_up(
        &self,
        skill_dirs: &[&Path],
        registry: &crate::adapters::skill_registry::SkillRegistry,
    ) {
        let manifest = Self::manifest(skill_dirs);
        match self.load_snapshot(manifest).await {
            Ok(true) => {
                tracing::info!("Skill cache: L2 snapshot loaded (manifest valid)");
                return;
            }
            Ok(false) => {
                tracing::info!(
                    "Skill cache: L2 snapshot stale or missing — populating from registry"
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Skill cache: L2 snapshot load failed ({}) — populating from registry",
                    e
                );
            }
        }
        self.populate_from_registry(registry).await;
        if let Err(e) = self.save_snapshot(manifest).await {
            tracing::warn!("Skill cache: failed to persist L2 snapshot: {}", e);
        } else {
            tracing::info!("Skill cache: L2 snapshot saved for next startup");
        }
    }

    /// Load L2 disk snapshot into L1 if manifest matches. Returns false if
    /// stale or missing L2 path.
    pub async fn load_snapshot(
        &self,
        expected_manifest: [u8; 32],
    ) -> Result<bool, SkillCacheError> {
        let Some(l2_path) = &self.l2_path else {
            return Ok(false);
        };
        if !l2_path.exists() {
            return Ok(false);
        }
        let json =
            std::fs::read_to_string(l2_path).map_err(|e| SkillCacheError::Io(e.to_string()))?;
        let snapshot: SnapshotFile =
            serde_json::from_str(&json).map_err(|e| SkillCacheError::Serde(e.to_string()))?;
        let stored = hex::decode(&snapshot.manifest_hex)
            .map_err(|e| SkillCacheError::Manifest(e.to_string()))?;
        if stored.len() != 32 || stored.as_slice() != expected_manifest {
            return Ok(false);
        }
        let mut guard = self.inner.lock().await;
        for (k, v) in snapshot.entries {
            guard.put(k, v);
        }
        let mut manifest_guard = self.current_manifest.lock().await;
        *manifest_guard = Some(expected_manifest);
        Ok(true)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotFile {
    manifest_hex: String,
    entries: Vec<(String, CachedEntry)>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillCacheError {
    #[error("skill not found: {0}")]
    SkillNotFound(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("serde error: {0}")]
    Serde(String),
    #[error("manifest decode error: {0}")]
    Manifest(String),
}
