//! `FileAuthStore` — persists provider credentials to `~/.rustain/auth.json`.
//!
//! Story 13.4a.  Atomic writes via sibling `.tmp` + `rename(2)`, advisory
//! locking via `flock(2)` on unix, in-process serialisation via `tokio::sync::Mutex`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::errors::AuthError;
use crate::domain::models::credential::{AuthSource, AuthStatus, Credential, ProviderStatus};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::paths;

// ---------------------------------------------------------------------------
// On-disk schema (private)
// ---------------------------------------------------------------------------

/// Top-level `auth.json` schema.
#[derive(Serialize, Deserialize)]
struct AuthFile {
    version: u32,
    providers: HashMap<String, AuthEntry>,
}

/// Per-provider entry. Tagged enum for forward-compat (Epic 19 OAuth).
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum AuthEntry {
    #[serde(rename = "api_key")]
    ApiKey {
        api_key: String,
        /// RFC 3339 timestamp of last validation, if known.
        last_validated: Option<String>,
    },
    // Epic 19 will add:
    // #[serde(rename = "oauth")]
    // OAuth { access: String, refresh: String, expires: String },
}

impl std::fmt::Debug for AuthEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the secret. Never `#[derive(Debug)]` here — it would leak `api_key` (AC6/NFR11).
        match self {
            Self::ApiKey { last_validated, .. } => f
                .debug_struct("AuthEntry::ApiKey")
                .field("api_key", &"<redacted>")
                .field("last_validated", last_validated)
                .finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// FileAuthStore
// ---------------------------------------------------------------------------

/// Adapter that stores provider credentials in `~/.rustain/auth.json`.
pub struct FileAuthStore {
    /// In-process serialisation guard — prevents concurrent read-modify-write
    /// cycles within the same process (cross-process safety is handled by flock).
    lock: tokio::sync::Mutex<()>,
}

impl Default for FileAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FileAuthStore {
    pub fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Synchronous credential lookup for the provider key resolution path.
    ///
    /// Used by `provider_factory` at startup — the async port is not available in
    /// the sync `build_provider_for_config` call chain.  Reads `auth.json` directly
    /// (no lock needed for read-only access on an atomically-written file).
    pub fn get_sync(provider: &str) -> Option<String> {
        let path = Self::auth_file_path().ok()?;
        // A corrupt auth.json must NOT silently look like "no credentials" — surface it
        // so a busted file isn't mistaken for a first-run/empty state (P-7).
        let data = match Self::read_auth_file(&path) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("auth.json unreadable; ignoring stored credentials: {e}");
                return None;
            }
        };
        data.providers.get(provider).map(|entry| match entry {
            AuthEntry::ApiKey { api_key, .. } => api_key.clone(),
        })
    }

    /// Canonical path: `<data_dir>/auth.json`.
    fn auth_file_path() -> Result<PathBuf, AuthError> {
        paths::data_dir()
            .map(|d| d.join("auth.json"))
            .map_err(|e| AuthError::Io(e.to_string()))
    }

    /// Read and parse `auth.json`.  A missing file is not an error — it
    /// returns an empty provider map (first-run case).
    fn read_auth_file(path: &Path) -> Result<AuthFile, AuthError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<AuthFile>(&contents)
                .map_err(|e| AuthError::Parse(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthFile {
                version: 1,
                providers: HashMap::new(),
            }),
            Err(e) => Err(AuthError::Io(e.to_string())),
        }
    }

    /// Acquire the advisory flock, then run the whole read → modify → write cycle
    /// under it (P-2). Holding the flock across the read *and* the write is what
    /// prevents a cross-process lost-update: two concurrent writers can no longer
    /// both read v0 and have the last rename win. `modify` returns whether the map
    /// actually changed; if not, the write (and its exclusive-lock cost) is skipped
    /// (P-8 — a no-op `remove` no longer rewrites the file).
    fn modify_locked<F>(path: &Path, modify: F) -> Result<(), AuthError>
    where
        F: FnOnce(&mut AuthFile) -> Result<bool, AuthError>,
    {
        let lock_path = path.with_extension("json.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| AuthError::Io(format!("opening lock file: {e}")))?;

        // --- advisory lock (unix only) ---
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = lock_file.as_raw_fd();
            // SAFETY: fd is valid and owned by lock_file for the duration of the call.
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if ret != 0 {
                return Err(AuthError::Locked(format!(
                    "flock(LOCK_EX): {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

        let result: Result<(), AuthError> = (|| {
            let mut data = Self::read_auth_file(path)?;
            let changed = modify(&mut data)?;
            if changed {
                Self::write_inner(path, &data)?;
            }
            Ok(())
        })();

        // --- release advisory lock (unix only) ---
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = lock_file.as_raw_fd();
            // SAFETY: fd is still valid.
            unsafe {
                libc::flock(fd, libc::LOCK_UN);
            }
        }

        drop(lock_file);
        result
    }

    /// The inner write: serialize → tmp → rename.  Separated so that the
    /// flock release always executes regardless of write outcome.
    fn write_inner(path: &Path, data: &AuthFile) -> Result<(), AuthError> {
        let tmp_path = path.with_extension("json.tmp");

        let json =
            serde_json::to_string_pretty(data).map_err(|e| AuthError::Parse(e.to_string()))?;

        // If a stale tmp exists from a prior crash, clean it up first.
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)
                .map_err(|e| AuthError::Io(format!("removing stale tmp: {e}")))?;
        }

        let mut tmp_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| AuthError::Io(format!("creating tmp file: {e}")))?;

        tmp_file
            .write_all(json.as_bytes())
            .map_err(|e| AuthError::Io(format!("writing tmp file: {e}")))?;

        // Restrict permissions before the atomic rename so the final file
        // is never world-readable, even momentarily.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp_path, perms)
                .map_err(|e| AuthError::Io(format!("chmod 0600: {e}")))?;
        }

        // Flush to disk before rename.
        tmp_file
            .sync_all()
            .map_err(|e| AuthError::Io(format!("fsync: {e}")))?;
        drop(tmp_file);

        if let Err(e) = std::fs::rename(&tmp_path, path) {
            // Cleanup stale tmp on rename failure.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(AuthError::Io(format!("rename: {e}")));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AuthStorePort implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl AuthStorePort for FileAuthStore {
    async fn get(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        let path = Self::auth_file_path()?;
        let data = Self::read_auth_file(&path)?;
        Ok(data.providers.get(provider).map(|entry| match entry {
            AuthEntry::ApiKey { api_key, .. } => Credential::new_api_key(api_key.clone()),
        }))
    }

    async fn set(&self, provider: &str, cred: Credential) -> Result<(), AuthError> {
        // Default: stamp `last_validated = now`.
        self.set_validated(provider, cred, true).await
    }

    /// Store a credential with an explicit validation outcome (P-3 / spec Q1).
    /// `validated = false` (e.g. an inconclusive `/models` probe) records
    /// `last_validated = None`, so 13.4b's `auth status` won't misreport an
    /// unvalidated key as verified.
    async fn set_validated(
        &self,
        provider: &str,
        cred: Credential,
        validated: bool,
    ) -> Result<(), AuthError> {
        let _guard = self.lock.lock().await;
        let path = Self::auth_file_path()?;

        let key_str = cred
            .expose_api_key()
            .ok_or_else(|| AuthError::Io("Cannot store non-API-key credential".into()))?
            .to_owned();

        let last_validated = if validated {
            Some(chrono::Utc::now().to_rfc3339())
        } else {
            None
        };
        let provider = provider.to_string();

        Self::modify_locked(&path, move |data| {
            data.providers.insert(
                provider,
                AuthEntry::ApiKey {
                    api_key: key_str,
                    last_validated,
                },
            );
            Ok(true)
        })?;
        Ok(())
    }

    async fn remove(&self, provider: &str) -> Result<(), AuthError> {
        let _guard = self.lock.lock().await;
        let path = Self::auth_file_path()?;
        let provider = provider.to_string();
        Self::modify_locked(&path, |data| {
            // P-8: skip the write entirely if nothing was actually removed.
            Ok(data.providers.remove(&provider).is_some())
        })?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<ProviderStatus>, AuthError> {
        let path = Self::auth_file_path()?;
        let data = Self::read_auth_file(&path)?;
        Ok(data
            .providers
            .iter()
            .map(|(id, entry)| {
                let last_validated = match entry {
                    AuthEntry::ApiKey { last_validated, .. } => {
                        last_validated.as_ref().and_then(|s| {
                            chrono::DateTime::parse_from_rfc3339(s)
                                .ok()
                                .map(|dt| dt.with_timezone(&chrono::Utc))
                        })
                    }
                };
                ProviderStatus {
                    provider: id.clone(),
                    status: AuthStatus::Authenticated,
                    source: AuthSource::AuthJson,
                    last_validated,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Save the current `RUSTAIN_DATA_DIR`, point it at `dir`, return the original.
    fn set_data_dir(dir: &std::path::Path) -> Option<String> {
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", dir.as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
        original
    }

    /// Restore `RUSTAIN_DATA_DIR` from a saved original value.
    fn restore_data_dir(original: Option<String>) {
        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }

    // P0-3a: 0600 permissions (unix only)
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn auth_file_has_0600_permissions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        let store = FileAuthStore::new();
        let cred = Credential::new_api_key("test-key".to_string());
        store.set("test-provider", cred).await.expect("set");

        let auth_path = tmp.path().join("auth.json");
        assert!(auth_path.exists(), "auth.json should exist after set()");

        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&auth_path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "auth.json should have 0600 permissions, got {:o}",
            perms.mode() & 0o777
        );

        // No leftover .tmp file
        let tmp_path = tmp.path().join("auth.json.tmp");
        assert!(!tmp_path.exists(), "stale .tmp file should not exist");

        restore_data_dir(original);
    }

    // P0-3b: Versioned schema on disk
    #[tokio::test]
    #[serial]
    async fn auth_file_uses_versioned_schema() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        let store = FileAuthStore::new();
        let cred = Credential::new_api_key("schema-test-key".to_string());
        store.set("anthropic", cred).await.expect("set");

        let auth_path = tmp.path().join("auth.json");
        let raw = std::fs::read_to_string(&auth_path).expect("read auth.json");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse json");

        // Version tag
        assert_eq!(parsed["version"], 1, "auth.json should have version: 1");

        // Type tag on the provider entry
        let entry = &parsed["providers"]["anthropic"];
        assert_eq!(entry["type"], "api_key", "entry should have type: api_key");

        // last_validated is RFC3339
        let lv = entry["last_validated"]
            .as_str()
            .expect("last_validated should be a string");
        chrono::DateTime::parse_from_rfc3339(lv).expect("last_validated should be RFC3339");

        restore_data_dir(original);
    }

    // P0-3c: Missing file returns empty
    #[tokio::test]
    #[serial]
    async fn missing_auth_file_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        let store = FileAuthStore::new();
        let result = store.get("anthropic").await.expect("get");
        assert!(result.is_none(), "missing auth.json should return None");

        restore_data_dir(original);
    }

    // P0-5: Overwrite (set replaces)
    #[tokio::test]
    #[serial]
    async fn set_replaces_existing_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        let store = FileAuthStore::new();
        store
            .set("test-provider", Credential::new_api_key("key1".to_string()))
            .await
            .expect("set key1");
        store
            .set("test-provider", Credential::new_api_key("key2".to_string()))
            .await
            .expect("set key2");

        let got = store
            .get("test-provider")
            .await
            .expect("get")
            .expect("should be Some");
        assert_eq!(
            got.expose_api_key(),
            Some("key2"),
            "set should replace existing entry"
        );

        restore_data_dir(original);
    }

    // P0-3d: Remove works
    #[tokio::test]
    #[serial]
    async fn remove_deletes_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        let store = FileAuthStore::new();
        store
            .set(
                "ephemeral",
                Credential::new_api_key("gone-soon".to_string()),
            )
            .await
            .expect("set");
        store.remove("ephemeral").await.expect("remove");

        let got = store.get("ephemeral").await.expect("get");
        assert!(got.is_none(), "removed entry should return None");

        restore_data_dir(original);
    }

    // P0-3e: List returns stored providers
    #[tokio::test]
    #[serial]
    async fn list_returns_all_stored_providers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        let store = FileAuthStore::new();
        store
            .set("provider-a", Credential::new_api_key("ka".to_string()))
            .await
            .expect("set a");
        store
            .set("provider-b", Credential::new_api_key("kb".to_string()))
            .await
            .expect("set b");

        let statuses = store.list().await.expect("list");
        assert_eq!(statuses.len(), 2, "should list both providers");

        let ids: std::collections::HashSet<&str> =
            statuses.iter().map(|s| s.provider.as_str()).collect();
        assert!(ids.contains("provider-a"), "should contain provider-a");
        assert!(ids.contains("provider-b"), "should contain provider-b");

        for s in &statuses {
            assert!(
                matches!(s.status, AuthStatus::Authenticated),
                "status should be Authenticated"
            );
            assert!(
                matches!(s.source, AuthSource::AuthJson),
                "source should be AuthJson"
            );
        }

        restore_data_dir(original);
    }

    // get_sync works
    #[test]
    #[serial]
    fn get_sync_returns_stored_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = set_data_dir(tmp.path());

        // Manually write a valid auth.json
        let auth_json = serde_json::json!({
            "version": 1,
            "providers": {
                "test": {
                    "type": "api_key",
                    "api_key": "sync-test-key",
                    "last_validated": null
                }
            }
        });
        let auth_path = tmp.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_string_pretty(&auth_json).unwrap(),
        )
        .expect("write auth.json");

        let got = FileAuthStore::get_sync("test");
        assert_eq!(got, Some("sync-test-key".to_string()));

        // Unknown provider returns None
        let missing = FileAuthStore::get_sync("nonexistent");
        assert!(missing.is_none());

        restore_data_dir(original);
    }
}
