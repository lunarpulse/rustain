//! Flat (brute-force) cosine vector index with binary (`bincode`) persistence
//! (Story 11.3a, AC2/AC3/AC6).
//!
//! Deliberately NOT HNSW: for ≤10k × 384-dim a full cosine scan is sub-10ms
//! (3.84M MACs), trivially under NFR56's 200ms bound. HNSW's tuning params
//! (M, ef) + a second serialization concern buy nothing at this scale and would
//! hurt determinism. The internal shape is kept small so HNSW can be swapped in
//! later if telemetry shows >10k entries (the deferred upgrade trigger).
//!
//! ## Persistence (architecture.md:576 — the index is BINARY)
//! `MemoryEntry` is intentionally serde-free (it round-trips through markdown in
//! the daily-log/long-term adapters). To persist it WITHOUT changing that domain
//! type's surface, we encode through a private DTO ([`PersistedIndex`] /
//! [`PersistedEntry`]) that derives `bincode::{Encode, Decode}` — mirroring how
//! Story 11.2a kept `MemoryFact` serde-free behind a private `ProposedFact` DTO.
//!
//! The header carries `(model_id, dimension, version)` so a load can detect a
//! provider switch (both are `Vec<f32>`; the type system won't catch a dim
//! mismatch — architecture.md:174). The *adapter* owns the mismatch policy
//! (refuse + rebuild, 11.3a); this module just stores and reports the header.

use std::collections::HashSet;
use std::path::Path;

use bincode::{Decode, Encode};
use chrono::{DateTime, Local, TimeZone, Utc};

use crate::domain::errors::MemoryError;
use crate::domain::models::MemoryEntry;

/// On-disk layout version. Bump on any incompatible change to the DTOs below;
/// a load with a different version is treated like a model/dim mismatch.
pub const INDEX_VERSION: u32 = 1;

/// Header persisted with every index: the embedding identity it was built under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub model_id: String,
    pub dimension: usize,
    pub version: u32,
}

/// One indexed memory entry: its stable content key, its embedding, and the
/// source [`MemoryEntry`] (returned verbatim by search — 11.3a does no ranking
/// metadata; `ProvenancedEntry` is Story 11.4).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedEntry {
    pub key: u64,
    pub vector: Vec<f32>,
    pub entry: MemoryEntry,
}

/// The whole flat index: header + entries scanned on every `search`.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorIndex {
    pub meta: IndexMeta,
    pub entries: Vec<IndexedEntry>,
}

// ── Private persistence DTOs (keep `MemoryEntry` serde/bincode-free) ──

#[derive(Encode, Decode)]
struct PersistedIndex {
    model_id: String,
    dimension: u64,
    version: u32,
    entries: Vec<PersistedEntry>,
}

#[derive(Encode, Decode)]
struct PersistedEntry {
    key: u64,
    vector: Vec<f32>,
    /// `MemoryEntry::timestamp` as Unix milliseconds (daily logs are second-
    /// granular, so ms is lossless in practice).
    ts_millis: i64,
    summary: String,
    context: Option<String>,
}

impl VectorIndex {
    /// An empty index carrying the active provider's identity header.
    pub fn empty(meta: IndexMeta) -> Self {
        Self {
            meta,
            entries: Vec::new(),
        }
    }

    /// The set of content keys currently indexed (drives incremental diffing).
    pub fn keys(&self) -> HashSet<u64> {
        self.entries.iter().map(|e| e.key).collect()
    }

    /// Cosine top-`k`, descending score. Ties broken by insertion order so the
    /// result is fully deterministic. Returns `(score, entry)` pairs.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(f32, MemoryEntry)> {
        if k == 0 || self.entries.is_empty() {
            return Vec::new();
        }
        let q_norm = norm(query);
        if q_norm == 0.0 {
            return Vec::new();
        }
        let mut scored: Vec<(f32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (cosine_pre(query, q_norm, &e.vector), i))
            .collect();
        // Descending score; stable on original index for ties.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        scored
            .into_iter()
            .take(k)
            .map(|(s, i)| (s, self.entries[i].entry.clone()))
            .collect()
    }

    /// Drop every entry whose key is NOT in `keep` (AC6 — vanished entries).
    pub fn retain_keys(&mut self, keep: &HashSet<u64>) {
        self.entries.retain(|e| keep.contains(&e.key));
    }

    /// Append freshly-embedded entries (AC6 — new entries).
    pub fn extend(&mut self, new: Vec<IndexedEntry>) {
        self.entries.extend(new);
    }

    /// Encode to the binary `index.bin` payload (sync — safe to call under a
    /// lock guard; the async fs write happens after the guard is released).
    pub fn to_bytes(&self) -> Result<Vec<u8>, MemoryError> {
        let dto = PersistedIndex {
            model_id: self.meta.model_id.clone(),
            dimension: self.meta.dimension as u64,
            version: self.meta.version,
            entries: self
                .entries
                .iter()
                .map(|e| PersistedEntry {
                    key: e.key,
                    vector: e.vector.clone(),
                    ts_millis: e.entry.timestamp.timestamp_millis(),
                    summary: e.entry.summary.clone(),
                    context: e.entry.context.clone(),
                })
                .collect(),
        };
        bincode::encode_to_vec(&dto, bincode::config::standard())
            .map_err(|e| MemoryError::IoError(format!("index encode failed: {e}")))
    }

    /// Decode an `index.bin` payload back into a [`VectorIndex`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MemoryError> {
        let (dto, _read): (PersistedIndex, usize) =
            bincode::decode_from_slice(bytes, bincode::config::standard())
                .map_err(|e| MemoryError::ParseError(format!("index decode failed: {e}")))?;
        let meta = IndexMeta {
            model_id: dto.model_id,
            dimension: dto.dimension as usize,
            version: dto.version,
        };
        let dim = meta.dimension;
        let entries = dto
            .entries
            .into_iter()
            .filter(|p| {
                let ok = p.vector.len() == dim;
                if !ok {
                    tracing::warn!(
                        key = p.key,
                        expected = dim,
                        actual = p.vector.len(),
                        "skipping index entry with wrong vector dimension"
                    );
                }
                ok
            })
            .map(|p| IndexedEntry {
                key: p.key,
                vector: p.vector,
                entry: MemoryEntry {
                    timestamp: millis_to_local(p.ts_millis),
                    summary: p.summary,
                    context: p.context,
                },
            })
            .collect();
        Ok(Self { meta, entries })
    }
}

/// Read + decode a persisted index. `Ok(None)` if the file is absent (build from
/// scratch). The single `read` await holds no lock guard.
pub async fn load_index(path: &Path) -> Result<Option<VectorIndex>, MemoryError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(VectorIndex::from_bytes(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MemoryError::IoError(format!(
            "failed to read {}: {e}",
            path.display()
        ))),
    }
}

/// L2 norm of a vector.
fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Cosine similarity with a precomputed query norm (hot-path helper).
fn cosine_pre(q: &[f32], q_norm: f32, d: &[f32]) -> f32 {
    if q.len() != d.len() {
        return 0.0;
    }
    let dot: f32 = q.iter().zip(d).map(|(a, b)| a * b).sum();
    let d_norm = norm(d);
    if d_norm == 0.0 {
        0.0
    } else {
        dot / (q_norm * d_norm)
    }
}

/// Cosine similarity of two vectors (mismatched length / zero vector → 0.0).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let a_norm = norm(a);
    if a_norm == 0.0 {
        return 0.0;
    }
    cosine_pre(a, a_norm, b)
}

/// Reconstruct a local timestamp from persisted Unix milliseconds.
fn millis_to_local(ms: i64) -> DateTime<Local> {
    match Utc.timestamp_millis_opt(ms).single() {
        Some(u) => u.with_timezone(&Local),
        None => {
            tracing::warn!(ms, "out-of-range timestamp in index — substituting current time");
            Local::now()
        }
    }
}

/// Test-only: build a millisecond-exact local timestamp (so cross-module tests
/// produce stable content keys + bit-stable bincode round-trips).
#[cfg(test)]
pub(crate) fn tests_millis_to_local(ms: i64) -> DateTime<Local> {
    millis_to_local(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `MemoryEntry` whose timestamp is exact at millisecond precision,
    /// so the bincode round-trip is bit-stable (Local::now() carries sub-ms ns
    /// that ms persistence would truncate).
    fn entry_at(ms: i64, summary: &str, context: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            timestamp: millis_to_local(ms),
            summary: summary.to_string(),
            context: context.map(|s| s.to_string()),
        }
    }

    fn meta() -> IndexMeta {
        IndexMeta {
            model_id: "stub-v1".to_string(),
            dimension: 4,
            version: INDEX_VERSION,
        }
    }

    #[test]
    fn cosine_math() {
        // Identical → 1.0, orthogonal → 0.0, opposite → -1.0.
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        // Zero vector / length mismatch → 0.0 (never NaN).
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn top_k_ordering_descending_and_capped() {
        let mut idx = VectorIndex::empty(meta());
        idx.extend(vec![
            IndexedEntry {
                key: 1,
                vector: vec![1.0, 0.0, 0.0, 0.0],
                entry: entry_at(1, "x-axis", None),
            },
            IndexedEntry {
                key: 2,
                vector: vec![0.0, 1.0, 0.0, 0.0],
                entry: entry_at(2, "y-axis", None),
            },
            IndexedEntry {
                key: 3,
                vector: vec![0.9, 0.1, 0.0, 0.0],
                entry: entry_at(3, "near-x", None),
            },
        ]);
        // Query along x: x-axis (1.0) > near-x (~0.99) > y-axis (~0).
        let hits = idx.search(&[1.0, 0.0, 0.0, 0.0], 2);
        assert_eq!(hits.len(), 2, "k cap respected");
        assert_eq!(hits[0].1.summary, "x-axis");
        assert_eq!(hits[1].1.summary, "near-x");
        assert!(hits[0].0 >= hits[1].0, "scores descending");
    }

    #[test]
    fn search_empty_or_zero_query_is_empty() {
        let idx = VectorIndex::empty(meta());
        assert!(
            idx.search(&[1.0, 0.0, 0.0, 0.0], 5).is_empty(),
            "empty index"
        );
        let mut idx = VectorIndex::empty(meta());
        idx.extend(vec![IndexedEntry {
            key: 1,
            vector: vec![1.0, 0.0, 0.0, 0.0],
            entry: entry_at(1, "x", None),
        }]);
        assert!(
            idx.search(&[0.0, 0.0, 0.0, 0.0], 5).is_empty(),
            "zero query"
        );
        assert!(idx.search(&[1.0, 0.0, 0.0, 0.0], 0).is_empty(), "k=0");
    }

    #[test]
    fn bincode_round_trip_preserves_entries_and_header() {
        let mut idx = VectorIndex::empty(meta());
        idx.extend(vec![
            IndexedEntry {
                key: 42,
                vector: vec![0.1, 0.2, 0.3, 0.4],
                entry: entry_at(
                    1_700_000_000_000,
                    "database is postgres",
                    Some("primary store"),
                ),
            },
            IndexedEntry {
                key: 7,
                vector: vec![-0.5, 0.5, 0.0, 1.0],
                entry: entry_at(1_700_000_500_000, "parser uses pratt", None),
            },
        ]);
        let bytes = idx.to_bytes().unwrap();
        let back = VectorIndex::from_bytes(&bytes).unwrap();
        assert_eq!(idx, back, "round-trip is bit-stable");
        assert_eq!(back.meta.model_id, "stub-v1");
        assert_eq!(back.meta.dimension, 4);
        assert_eq!(back.meta.version, INDEX_VERSION);
    }

    #[test]
    fn retain_and_extend_drive_incremental_shape() {
        let mut idx = VectorIndex::empty(meta());
        idx.extend(vec![
            IndexedEntry {
                key: 1,
                vector: vec![1.0, 0.0, 0.0, 0.0],
                entry: entry_at(1, "a", None),
            },
            IndexedEntry {
                key: 2,
                vector: vec![0.0, 1.0, 0.0, 0.0],
                entry: entry_at(2, "b", None),
            },
        ]);
        // Drop key 1, keep key 2.
        let keep: HashSet<u64> = [2].into_iter().collect();
        idx.retain_keys(&keep);
        assert_eq!(idx.keys(), [2].into_iter().collect());
        // Add a new key 3.
        idx.extend(vec![IndexedEntry {
            key: 3,
            vector: vec![0.0, 0.0, 1.0, 0.0],
            entry: entry_at(3, "c", None),
        }]);
        assert_eq!(idx.keys(), [2, 3].into_iter().collect());
    }
}
