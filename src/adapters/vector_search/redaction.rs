//! Durable redaction tombstone store — the `redactions.bin` sidecar that makes
//! memory removal survive `refresh()` and a full reindex (Story 11.4a / FR122).
//!
//! ## Why a SIBLING file, not a section inside `index.bin` (Q2)
//! The linchpin (AC-R3) is that a removal survives **purging `index.bin` and
//! rebuilding from a still-dirty source**. If the tombstone lived inside
//! `index.bin`, deleting/rebuilding the index would discard the gravestone and
//! "the ghost walks again". A sibling `redactions.bin` is loaded independently of
//! the index, so it outlives any index rebuild and is replayed on every refresh.
//! It is the **source of truth** for removal (AC-R6): written FIRST, before the
//! one-time index purge.
//!
//! Like the vector index, the domain [`RedactionRecord`] is serde-free; it
//! round-trips through a private bincode DTO ([`PersistedRedactions`]) — the same
//! discipline `index.rs` uses for `MemoryEntry`/`PersistedEntry`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bincode::{Decode, Encode};
use chrono::{DateTime, Local, TimeZone, Utc};

use crate::domain::errors::MemoryError;
use crate::domain::models::{RedactionOp, RedactionRecord};

/// On-disk layout version for the redactions sidecar. Bump on any incompatible
/// DTO change; a load with a different version discards (re-derived removals are
/// safe — a lost tombstone is the ONE thing we never want, so a version bump that
/// dropped tombstones would itself be a bug to catch in review).
///
/// **v2 (Story 12.1c AC3)** adds the content-stable `token` per entry. To honor
/// the "never drop a tombstone" contract across this bump, [`RedactionStore::
/// from_bytes`] MIGRATES a v1 sidecar (token defaults to `""` → the record keeps
/// suppressing via its `u64` key exactly as in 11.4a) rather than discarding it.
pub const REDACTIONS_VERSION: u32 = 2;

/// The previous (11.4a) on-disk version — key-only, no content-stable token.
/// Read-migrated, never written.
const REDACTIONS_VERSION_V1: u32 = 1;

/// The full set of durable redaction tombstones, keyed by stable `u64` content
/// key. Held in memory by the adapter (behind a `tokio::sync::RwLock`) and
/// mirrored to the `redactions.bin` sidecar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactionStore {
    records: Vec<RedactionRecord>,
}

// ── Private persistence DTOs (keep `RedactionRecord` serde/bincode-free) ──

#[derive(Encode, Decode)]
struct PersistedRedactions {
    version: u32,
    entries: Vec<PersistedRedaction>,
}

#[derive(Encode, Decode)]
struct PersistedRedaction {
    key: u64,
    /// `RedactionOp` discriminant (0 = Forget). Unknown values decode to `Forget`
    /// — a tombstone we don't understand still suppresses retrieval, which is the
    /// safe default for removal-integrity.
    op: u8,
    /// Redaction timestamp as Unix milliseconds.
    ts_millis: i64,
    /// Content-stable suppression token (Story 12.1c AC3). `""` = key-only / a
    /// migrated v1 record.
    token: String,
}

// ── v1 (11.4a) DTOs — read-only, for the no-drop migration in `from_bytes` ──

#[derive(Decode)]
#[cfg_attr(test, derive(Encode))]
struct PersistedRedactionsV1 {
    version: u32,
    entries: Vec<PersistedRedactionV1>,
}

#[derive(Decode)]
#[cfg_attr(test, derive(Encode))]
struct PersistedRedactionV1 {
    key: u64,
    op: u8,
    ts_millis: i64,
}

fn op_to_u8(op: RedactionOp) -> u8 {
    match op {
        RedactionOp::Forget => 0,
    }
}

fn u8_to_op(_v: u8) -> RedactionOp {
    // Only `Forget` exists; any value is treated as `Forget` (safe-by-default).
    RedactionOp::Forget
}

impl RedactionStore {
    /// An empty store (no redactions).
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// The set of redacted content keys — the gate consulted by `refresh()`,
    /// rebuild, and read-time masking.
    pub fn keys(&self) -> HashSet<u64> {
        self.records.iter().map(|r| r.key).collect()
    }

    /// The set of **content-stable suppression tokens** (Story 12.1c AC3) —
    /// `normalize(summary)` identities, empty tokens excluded. The gate drops a
    /// candidate whose `normalize(text)` is in this set, so ONE tombstone
    /// suppresses the same fact across every timestamp namespace (the
    /// `MEMORY.md`-mtime copy AND the daily-log-realts re-derivation copy).
    pub fn tokens(&self) -> HashSet<String> {
        self.records
            .iter()
            .filter(|r| !r.token.is_empty())
            .map(|r| r.token.clone())
            .collect()
    }

    /// Is this key redacted?
    pub fn contains(&self, key: u64) -> bool {
        self.records.iter().any(|r| r.key == key)
    }

    /// The full tombstone for `key`, if present (the parity test asserts the whole
    /// `RedactionRecord` — key/op/token — is identical across both producers).
    pub fn get(&self, key: u64) -> Option<&RedactionRecord> {
        self.records.iter().find(|r| r.key == key)
    }

    /// Number of tombstones.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the store holds no tombstones.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Add a tombstone (idempotent — re-redacting an already-redacted key is a
    /// no-op, so a repeated `/memory forget` doesn't grow the store).
    pub fn insert(&mut self, record: RedactionRecord) {
        if !self.contains(record.key) {
            self.records.push(record);
        }
    }

    /// Encode to the binary `redactions.bin` payload (sync — safe under a guard).
    /// Always writes the current (v2, token-bearing) layout.
    pub fn to_bytes(&self) -> Result<Vec<u8>, MemoryError> {
        let dto = PersistedRedactions {
            version: REDACTIONS_VERSION,
            entries: self
                .records
                .iter()
                .map(|r| PersistedRedaction {
                    key: r.key,
                    op: op_to_u8(r.op),
                    ts_millis: r.timestamp.timestamp_millis(),
                    token: r.token.clone(),
                })
                .collect(),
        };
        bincode::encode_to_vec(&dto, bincode::config::standard())
            .map_err(|e| MemoryError::IoError(format!("redactions encode failed: {e}")))
    }

    /// Decode a `redactions.bin` payload. Tries the current v2 (token-bearing)
    /// layout first; if that doesn't yield `version == REDACTIONS_VERSION`, falls
    /// back to MIGRATING a v1 (11.4a, key-only) sidecar rather than discarding it
    /// — losing a tombstone is the ONE thing we never do (AC-R3). A migrated v1
    /// record gets an empty `token`, so it keeps suppressing via its `u64` key
    /// exactly as before. Any other version / a genuine decode failure is a parse
    /// error (NOT silently empty).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MemoryError> {
        let cfg = bincode::config::standard();

        // v2 first. We must check the version BEFORE trusting the decode, because a
        // v1 buffer can decode into the v2 struct with trailing-byte slack — but it
        // will carry `version == 1`, so the guard below rejects it and we migrate.
        if let Ok((dto, _read)) = bincode::decode_from_slice::<PersistedRedactions, _>(bytes, cfg) {
            if dto.version == REDACTIONS_VERSION {
                let records = dto
                    .entries
                    .into_iter()
                    .map(|p| RedactionRecord {
                        key: p.key,
                        token: p.token,
                        op: u8_to_op(p.op),
                        timestamp: millis_to_local(p.ts_millis),
                    })
                    .collect();
                return Ok(Self { records });
            }
        }

        // v1 migration (no-drop upgrade).
        let (v1, _read): (PersistedRedactionsV1, usize) = bincode::decode_from_slice(bytes, cfg)
            .map_err(|e| MemoryError::ParseError(format!("redactions decode failed: {e}")))?;
        if v1.version != REDACTIONS_VERSION_V1 {
            return Err(MemoryError::ParseError(format!(
                "redactions.bin version mismatch: loaded={}, expected={} (or v{} for migration) — refusing to discard tombstones",
                v1.version, REDACTIONS_VERSION, REDACTIONS_VERSION_V1
            )));
        }
        let records = v1
            .entries
            .into_iter()
            .map(|p| RedactionRecord {
                key: p.key,
                token: String::new(), // key-only; suppression via the u64 key (11.4a)
                op: u8_to_op(p.op),
                timestamp: millis_to_local(p.ts_millis),
            })
            .collect();
        Ok(Self { records })
    }
}

/// The sibling sidecar path for a given `index.bin` path
/// (`…/memory/index.bin` → `…/memory/redactions.bin`).
pub fn sidecar_path(index_path: &Path) -> PathBuf {
    index_path.with_file_name("redactions.bin")
}

/// Read + decode the redactions sidecar. `Ok(empty)` if the file is absent (no
/// redactions yet). ANY read/decode error (corrupt, permission denied, is a
/// directory, version mismatch) is logged and treated as empty rather than
/// failing init — a missing tombstone is recoverable by re-running `/memory
/// forget`, but a hard init failure would strand the whole memory subsystem.
/// The severity is uniform: all I/O/decode failures are handled the same way.
pub async fn load_redactions(path: &Path) -> Result<RedactionStore, MemoryError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => match RedactionStore::from_bytes(&bytes) {
            Ok(store) => Ok(store),
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "corrupt or incompatible redactions.bin — starting empty (re-run /memory forget to recreate tombstones)");
                Ok(RedactionStore::empty())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RedactionStore::empty()),
        Err(e) => {
            tracing::error!(error = %e, path = %path.display(), "cannot read redactions.bin — starting empty");
            Ok(RedactionStore::empty())
        }
    }
}

/// Reconstruct a local timestamp from persisted Unix milliseconds.
fn millis_to_local(ms: i64) -> DateTime<Local> {
    match Utc.timestamp_millis_opt(ms).single() {
        Some(u) => u.with_timezone(&Local),
        None => {
            tracing::warn!(
                ms,
                "out-of-range redaction timestamp — substituting current time"
            );
            Local::now()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: u64, ms: i64) -> RedactionRecord {
        RedactionRecord::forget(key, millis_to_local(ms))
    }

    #[test]
    fn insert_is_idempotent_by_key() {
        let mut store = RedactionStore::empty();
        store.insert(rec(7, 1_700_000_000_000));
        store.insert(rec(7, 1_700_000_999_000)); // same key, later ts → ignored
        store.insert(rec(9, 1_700_000_000_000));
        assert_eq!(store.len(), 2);
        assert!(store.contains(7));
        assert!(store.contains(9));
        assert_eq!(store.keys(), [7, 9].into_iter().collect());
    }

    #[test]
    fn bincode_round_trip_preserves_keys() {
        let mut store = RedactionStore::empty();
        store.insert(rec(42, 1_700_000_000_000));
        store.insert(rec(7, 1_700_000_500_000));
        let bytes = store.to_bytes().unwrap();
        let back = RedactionStore::from_bytes(&bytes).unwrap();
        assert_eq!(store, back, "round-trip is bit-stable");
        assert_eq!(back.keys(), [42, 7].into_iter().collect());
    }

    #[test]
    fn empty_store_round_trips() {
        let store = RedactionStore::empty();
        let bytes = store.to_bytes().unwrap();
        let back = RedactionStore::from_bytes(&bytes).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn v2_round_trip_preserves_token() {
        let mut store = RedactionStore::empty();
        store.insert(RedactionRecord::redact(
            7,
            "the launch code".into(),
            millis_to_local(1_700_000_000_000),
        ));
        let back = RedactionStore::from_bytes(&store.to_bytes().unwrap()).unwrap();
        assert_eq!(store, back, "v2 token round-trips");
        assert_eq!(
            back.tokens(),
            ["the launch code".to_string()].into_iter().collect()
        );
    }

    /// Story 12.1c — a pre-12.1c (v1, key-only) sidecar MIGRATES rather than being
    /// discarded (losing a tombstone is the one thing we never do). The migrated
    /// record keeps its `u64` key (still suppresses) with an empty token.
    #[test]
    fn v1_sidecar_migrates_without_dropping_tombstones() {
        let v1 = PersistedRedactionsV1 {
            version: REDACTIONS_VERSION_V1,
            entries: vec![
                PersistedRedactionV1 {
                    key: 42,
                    op: 0,
                    ts_millis: 1_700_000_000_000,
                },
                PersistedRedactionV1 {
                    key: 99,
                    op: 0,
                    ts_millis: 1_700_000_500_000,
                },
            ],
        };
        let bytes = bincode::encode_to_vec(&v1, bincode::config::standard()).unwrap();
        let store = RedactionStore::from_bytes(&bytes).expect("v1 migrates, never discards");
        assert_eq!(
            store.keys(),
            [42, 99].into_iter().collect(),
            "both tombstones kept"
        );
        assert!(
            store.tokens().is_empty(),
            "migrated records are key-only (empty token)"
        );
    }

    #[test]
    fn sidecar_path_sits_beside_index() {
        let p = sidecar_path(Path::new("/tmp/x/.rustain/memory/index.bin"));
        assert_eq!(p, Path::new("/tmp/x/.rustain/memory/redactions.bin"));
    }
}
