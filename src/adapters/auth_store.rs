//! `FileAuthStore` — persists provider credentials to `~/.rustain/auth.json`.
//!
//! Story 13.4a.  Atomic writes via sibling `.tmp` + `rename(2)`, advisory
//! locking via `flock(2)` on unix, in-process serialisation via `tokio::sync::Mutex`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use crate::domain::errors::AuthError;
use crate::domain::models::credential::{
    AuthSnapshot, CasOutcome, Credential, CredentialGeneration, CredentialId, CredentialIdentity,
    CredentialOrigin, CredentialStamp, StoredCredential,
};
use crate::domain::models::secret::{SecretString, expose_secret_string};
use crate::domain::ports::{AuthStorePort, RefreshLease};
use crate::infrastructure::paths;

#[derive(Serialize)]
struct AuthFile {
    version: u32,
    next_id: u64,
    providers: BTreeMap<String, Vec<AuthRow>>,
}

/// Epic 20 rows retain unrecognised credential types verbatim for downgrade safety.
enum AuthRow {
    Known(StoredCredential),
    Unknown(Box<RawValue>),
}

/// Private disk projection: the only unmasked serializer uses the audited
/// `expose_secret_string` chokepoint, never the public masking serializer.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DiskCredential<'a> {
    ApiKey {
        #[serde(serialize_with = "expose_secret_string")]
        api_key: &'a SecretString,
    },
    Oauth {
        #[serde(serialize_with = "expose_secret_string")]
        access: &'a SecretString,
        #[serde(serialize_with = "expose_secret_string")]
        refresh: &'a SecretString,
        expires_at: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        authorized_at: Option<i64>,
        identity: &'a CredentialIdentity,
    },
}

impl Serialize for AuthRow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct DiskRow<'a> {
            id: CredentialId,
            rev: u64,
            generation: CredentialGeneration,
            origin: CredentialOrigin,
            disabled_cause: &'a Option<String>,
            last_validated: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(flatten)]
            credential: DiskCredential<'a>,
        }
        match self {
            Self::Unknown(raw) => raw.serialize(serializer),
            Self::Known(row) => DiskRow {
                id: row.id,
                rev: row.rev,
                generation: row.generation,
                origin: row.origin,
                disabled_cause: &row.disabled_cause,
                last_validated: row.last_validated,
                credential: match &row.credential {
                    Credential::ApiKey(api_key) => DiskCredential::ApiKey { api_key },
                    Credential::OAuth(oauth) => DiskCredential::Oauth {
                        access: &oauth.access,
                        refresh: &oauth.refresh,
                        expires_at: oauth.expires_at_ms,
                        authorized_at: oauth.authorized_at_ms,
                        identity: &oauth.identity,
                    },
                },
            }
            .serialize(serializer),
        }
    }
}

impl AuthFile {
    fn snapshot(self) -> AuthSnapshot {
        let mut snapshot = AuthSnapshot::default();
        for (provider, rows) in self.providers {
            let known = snapshot.providers.entry(provider).or_default();
            for row in rows {
                match row {
                    AuthRow::Known(row) => known.push(row),
                    AuthRow::Unknown(_) => snapshot.unknown_rows += 1,
                }
            }
        }
        snapshot
    }
}

fn parse_error(error: impl std::fmt::Display) -> AuthError {
    AuthError::Parse(error.to_string())
}

/// serde_json quotes offending values (`invalid type: string "…"`), which in a
/// malformed store can be credential bytes. Keep only category and position.
fn json_error(error: serde_json::Error) -> AuthError {
    AuthError::Parse(format!(
        "malformed auth.json ({:?} error at line {} column {})",
        error.classify(),
        error.line(),
        error.column()
    ))
}

/// Row type tag, plus the row id when present (unknown rows included), so
/// `next_id` can never hand out an id the file already uses.
fn row_header(raw: &RawValue) -> Result<(bool, Option<u64>), AuthError> {
    #[derive(Deserialize)]
    struct Header {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        id: Option<serde_json::Value>,
    }
    let header: Header = serde_json::from_str(raw.get()).map_err(json_error)?;
    Ok((
        matches!(header.kind.as_str(), "api_key" | "oauth"),
        header.id.as_ref().and_then(serde_json::Value::as_u64),
    ))
}

/// Hash the original row bytes, not a masking or normalizing reserialization.
/// Length prefixes keep provider and row boundaries unambiguous.
fn legacy_generation(provider: &str, raw: &RawValue) -> CredentialGeneration {
    let mut hash = Sha256::new();
    hash.update(b"rustain.auth.legacy-generation.v1\0");
    hash.update((provider.len() as u64).to_be_bytes());
    hash.update(provider.as_bytes());
    hash.update((raw.get().len() as u64).to_be_bytes());
    hash.update(raw.get().as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    if bytes == [0; 16] {
        bytes[15] = 1;
    }
    CredentialGeneration(bytes)
}

fn mint_generation() -> Result<CredentialGeneration, AuthError> {
    loop {
        let mut bytes = [0; 16];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|error| AuthError::Io(format!("credential generation entropy: {error}")))?;
        if bytes != [0; 16] {
            return Ok(CredentialGeneration(bytes));
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
    path: Option<PathBuf>,
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
            path: None,
        }
    }

    /// Explicit path for composition and isolated stores.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            path: Some(path.into()),
        }
    }

    /// Read-only startup bridge. Failures warn, never look like valid credentials.
    /// Secret wrappers remain intact through the synchronous selection path.
    pub fn snapshot_sync() -> AuthSnapshot {
        match Self::try_snapshot_sync() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!("auth.json unreadable; ignoring stored credentials: {error}");
                AuthSnapshot::default()
            }
        }
    }

    pub fn try_snapshot_sync() -> Result<AuthSnapshot, AuthError> {
        Self::auth_file_path()
            .and_then(|path| Self::read_auth_file(&path))
            .map(|data| data.snapshot())
    }

    fn path(&self) -> Result<PathBuf, AuthError> {
        self.path
            .clone()
            .map(Ok)
            .unwrap_or_else(Self::auth_file_path)
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
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthFile {
                    version: 2,
                    next_id: 1,
                    providers: BTreeMap::new(),
                });
            }
            Err(e) => return Err(AuthError::Io(e.to_string())),
        };
        #[derive(Deserialize)]
        struct VersionProbe {
            version: u32,
        }
        // The version decides the body shape, so it is checked before the body parses.
        let version = serde_json::from_str::<VersionProbe>(&contents)
            .map_err(json_error)?
            .version;
        if version > 2 {
            return Err(AuthError::UnsupportedVersion(version));
        }
        if version == 0 {
            return Err(parse_error("unsupported auth.json version 0"));
        }
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(default)]
            next_id: u64,
            providers: BTreeMap<String, Box<RawValue>>,
        }
        let envelope: Envelope = serde_json::from_str(&contents).map_err(json_error)?;
        let mut data = AuthFile {
            version: 2,
            next_id: if version == 1 { 1 } else { envelope.next_id },
            providers: BTreeMap::new(),
        };
        let mut max_id: u64 = 0;
        for (provider, raw) in envelope.providers {
            let rows = if version == 1 {
                vec![raw]
            } else {
                serde_json::from_str::<Vec<Box<RawValue>>>(raw.get()).map_err(json_error)?
            };
            let mut parsed = Vec::with_capacity(rows.len());
            for raw in rows {
                let (known, raw_id) = row_header(&raw)?;
                if !known {
                    max_id = max_id.max(raw_id.unwrap_or(0));
                    parsed.push(AuthRow::Unknown(raw));
                    continue;
                }
                let mut row = if version == 1 {
                    #[derive(Deserialize)]
                    struct LegacyRow {
                        #[serde(flatten)]
                        credential: Credential,
                        // v1 kept this opaque; an unparseable stamp must not hide the key.
                        #[serde(default)]
                        last_validated: Option<String>,
                    }
                    let legacy: LegacyRow = serde_json::from_str(raw.get()).map_err(json_error)?;
                    let id = CredentialId(data.next_id);
                    data.next_id = data
                        .next_id
                        .checked_add(1)
                        .ok_or_else(|| parse_error("credential id exhausted"))?;
                    StoredCredential {
                        id,
                        rev: 1,
                        generation: CredentialGeneration::default(),
                        origin: CredentialOrigin::Stored,
                        credential: legacy.credential,
                        last_validated: legacy
                            .last_validated
                            .as_deref()
                            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
                            .map(|stamp| stamp.with_timezone(&chrono::Utc)),
                        disabled_cause: None,
                    }
                } else {
                    serde_json::from_str::<StoredCredential>(raw.get()).map_err(json_error)?
                };
                if row.generation == CredentialGeneration::default() {
                    row.generation = legacy_generation(&provider, &raw);
                }
                max_id = max_id.max(row.id.0);
                parsed.push(AuthRow::Known(row));
            }
            data.providers.insert(provider, parsed);
        }
        // A missing or lagging counter (hand edit, older writer) must not reuse ids.
        let floor = max_id
            .checked_add(1)
            .ok_or_else(|| parse_error("credential id exhausted"))?;
        data.next_id = data.next_id.max(floor);
        Ok(data)
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

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Created owner-only: the tmp file holds refresh tokens before the rename.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut tmp_file = options
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
        #[cfg(unix)]
        {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| AuthError::Io(format!("fsync auth directory: {error}")))?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AuthStorePort implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl AuthStorePort for FileAuthStore {
    async fn snapshot(&self) -> Result<AuthSnapshot, AuthError> {
        Self::read_auth_file(&self.path()?).map(AuthFile::snapshot)
    }

    async fn upsert(
        &self,
        provider: &str,
        cred: Credential,
        origin: CredentialOrigin,
        validated: bool,
    ) -> Result<CredentialId, AuthError> {
        let _guard = self.lock.lock().await;
        let path = self.path()?;
        let mut result = CredentialId(0);
        Self::modify_locked(&path, |data| {
            let generation = mint_generation()?;
            let identity = match &cred {
                Credential::OAuth(oauth) => {
                    oauth
                        .identity
                        .key(crate::adapters::auth::policies::org_scoped_identity(
                            provider,
                        ))
                }
                Credential::ApiKey(_) => None,
            };
            let matches = |row: &StoredCredential| match (&cred, &row.credential) {
                (Credential::ApiKey(_), Credential::ApiKey(_)) => true,
                (Credential::OAuth(_), Credential::OAuth(old)) => {
                    identity.is_some()
                        && identity
                            == old.identity.key(
                                crate::adapters::auth::policies::org_scoped_identity(provider),
                            )
                }
                _ => false,
            };
            let rows = data.providers.entry(provider.to_owned()).or_default();
            // Superseded tombstones must not accumulate across re-login.
            rows.retain(|row| {
                !matches!(row, AuthRow::Known(row)
                if row.disabled_cause.is_some() && matches(row))
            });
            let last_validated = validated.then(chrono::Utc::now);
            if let Some(row) = rows.iter_mut().find_map(|row| match row {
                AuthRow::Known(row) if row.disabled_cause.is_none() && matches(row) => Some(row),
                _ => None,
            }) {
                row.rev = row
                    .rev
                    .checked_add(1)
                    .ok_or_else(|| parse_error("credential revision exhausted"))?;
                row.credential = cred;
                row.generation = generation;
                row.origin = origin;
                row.last_validated = last_validated;
                result = row.id;
            } else {
                result = CredentialId(data.next_id);
                data.next_id = data
                    .next_id
                    .checked_add(1)
                    .ok_or_else(|| parse_error("credential id exhausted"))?;
                rows.push(AuthRow::Known(StoredCredential {
                    id: result,
                    rev: 1,
                    generation,
                    origin,
                    credential: cred,
                    last_validated,
                    disabled_cause: None,
                }));
            }
            Ok(true)
        })?;
        Ok(result)
    }

    async fn replace_if_matches(
        &self,
        provider: &str,
        expected: CredentialStamp,
        cred: Credential,
    ) -> Result<CasOutcome, AuthError> {
        self.change_if_matches(provider, expected, |row| {
            row.credential = cred;
        })
        .await
    }

    async fn disable_if_matches(
        &self,
        provider: &str,
        expected: CredentialStamp,
        cause: &str,
    ) -> Result<CasOutcome, AuthError> {
        self.change_if_matches(provider, expected, |row| {
            row.disabled_cause = Some(cause.to_owned());
        })
        .await
    }

    async fn remove(&self, provider: &str) -> Result<(), AuthError> {
        let _guard = self.lock.lock().await;
        Self::modify_locked(&self.path()?, |data| {
            Ok(data.providers.remove(provider).is_some())
        })
    }

    /// ADR-19-1-03 no-daemon election: an flock on a sibling file, separate from
    /// the write lock so the holder can still persist through `modify_locked`.
    /// The kernel releases it when the holder exits or crashes, so there is no
    /// stale-lease reclaim; a holder keeps it for one bounded refresh.
    async fn refresh_lease(&self) -> Result<RefreshLease, AuthError> {
        let lock_path = self.path()?.with_extension("json.refresh.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| AuthError::Io(format!("opening refresh lock file: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd is valid and owned by `file` for the duration of the call.
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if ret != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                    return Err(AuthError::Locked(format!(
                        "flock(LOCK_EX|LOCK_NB): {error}"
                    )));
                }
                // Contended: a peer process is refreshing. Wait off the async runtime.
                return tokio::task::spawn_blocking(move || {
                    // SAFETY: fd is valid and owned by `file`, which moved into this closure.
                    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                        return Err(AuthError::Locked(format!(
                            "flock(LOCK_EX): {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                    Ok(RefreshLease::held(file))
                })
                .await
                .map_err(|e| AuthError::Locked(format!("refresh lease wait: {e}")))?;
            }
        }
        Ok(RefreshLease::held(file))
    }
}

impl FileAuthStore {
    async fn change_if_matches(
        &self,
        provider: &str,
        expected: CredentialStamp,
        change: impl FnOnce(&mut StoredCredential) + Send,
    ) -> Result<CasOutcome, AuthError> {
        let _guard = self.lock.lock().await;
        let mut outcome = CasOutcome::Stale;
        Self::modify_locked(&self.path()?, |data| {
            let row = data
                .providers
                .get_mut(provider)
                .into_iter()
                .flatten()
                .find_map(|row| match row {
                    AuthRow::Known(row)
                        if row.stamp() == expected && row.disabled_cause.is_none() =>
                    {
                        Some(row)
                    }
                    _ => None,
                });
            let Some(row) = row else { return Ok(false) };
            row.rev = row
                .rev
                .checked_add(1)
                .ok_or_else(|| parse_error("credential revision exhausted"))?;
            change(row);
            outcome = CasOutcome::Applied;
            Ok(true)
        })?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the child gets the environment override; the parent owns its lifetime.
    fn run_data_dir_scenario(scenario: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let helper = format!(
            "{}::auth_store_subprocess_scenario",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &helper, "--nocapture"])
            .env("RUSTAIN_AUTH_STORE_TEST_SCENARIO", scenario)
            .env("RUSTAIN_AUTH_STORE_TEST_DIR", tmp.path())
            .env("RUSTAIN_DATA_DIR", tmp.path())
            .output()
            .expect("run auth store subprocess");
        assert!(
            output.status.success(),
            "auth store subprocess ({scenario}) failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            tmp.path().join("scenario.completed").is_file(),
            "auth store subprocess ({scenario}) did not complete its assertions"
        );
    }

    async fn put(store: &FileAuthStore, provider: &str, key: &str) -> CredentialId {
        store
            .upsert(
                provider,
                Credential::new_api_key(key.into()),
                CredentialOrigin::Stored,
                true,
            )
            .await
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auth_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let store = FileAuthStore::at(&path);
        put(&store, "test-provider", "test-key").await;
        assert!(path.exists());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[tokio::test]
    async fn auth_file_uses_versioned_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let store = FileAuthStore::at(&path);
        put(&store, "anthropic", "schema-test-key").await;
        let raw = std::fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["version"], 2);
        let entry = &parsed["providers"]["anthropic"][0];
        assert_eq!(entry["type"], "api_key");
        assert_eq!(entry["origin"], "stored");
        chrono::DateTime::parse_from_rfc3339(entry["last_validated"].as_str().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn missing_auth_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileAuthStore::at(tmp.path().join("auth.json"));
        assert!(store.snapshot().await.unwrap().rows("anthropic").is_empty());
    }

    #[tokio::test]
    async fn set_replaces_existing_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileAuthStore::at(tmp.path().join("auth.json"));
        let first = put(&store, "test-provider", "key1").await;
        let first_generation = store.snapshot().await.unwrap().rows("test-provider")[0].generation;
        assert_ne!(first_generation, CredentialGeneration::default());
        let second = put(&store, "test-provider", "key2").await;
        assert_eq!(first, second);
        let snapshot = store.snapshot().await.unwrap();
        let rows = snapshot.rows("test-provider");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rev, 2);
        assert_ne!(rows[0].generation, CredentialGeneration::default());
        assert_ne!(rows[0].generation, first_generation);
        assert_eq!(rows[0].credential.expose_api_key(), Some("key2"));
    }

    #[tokio::test]
    async fn legacy_generation_is_read_only_provider_scoped_and_persisted_on_unrelated_write() {
        for (version, raw) in [
            (
                1,
                r#"{"type":"api_key","api_key":"LEGACY-KEY","last_validated":null}"#,
            ),
            (
                2,
                r#"{"id":1,"rev":7,"origin":"stored","type":"api_key","api_key":"LEGACY-KEY","last_validated":null,"disabled_cause":null}"#,
            ),
            (
                2,
                r#"{"id":1,"rev":7,"generation":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"origin":"stored","type":"api_key","api_key":"LEGACY-KEY","last_validated":null,"disabled_cause":null}"#,
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("auth.json");
            let rows = if version == 1 {
                raw.to_owned()
            } else {
                format!("[{raw}]")
            };
            let document =
                format!(r#"{{"version":{version},"providers":{{"a":{rows},"ab":{rows}}}}}"#);
            std::fs::write(&path, document.as_bytes()).unwrap();
            let before_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let store = FileAuthStore::at(&path);
            let first = store.snapshot().await.unwrap();
            let stamp = first.rows("a")[0].stamp();
            assert_ne!(stamp.generation, CredentialGeneration::default());
            assert_ne!(stamp.generation, first.rows("ab")[0].generation);
            assert_eq!(
                FileAuthStore::at(&path).snapshot().await.unwrap().rows("a")[0].stamp(),
                stamp
            );
            assert_eq!(std::fs::read(&path).unwrap(), document.as_bytes());
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                before_modified
            );
            assert!(!path.with_extension("json.lock").exists());

            let changed_path = tmp.path().join("changed.json");
            std::fs::write(&changed_path, document.replace("LEGACY-KEY", "OTHER-KEY")).unwrap();
            assert_ne!(
                FileAuthStore::at(changed_path)
                    .snapshot()
                    .await
                    .unwrap()
                    .rows("a")[0]
                    .generation,
                stamp.generation
            );

            put(&store, "unrelated", "UNRELATED-KEY").await;
            let migrated = FileAuthStore::at(&path).snapshot().await.unwrap();
            assert_eq!(migrated.rows("a")[0].stamp(), stamp);
            assert_eq!(
                migrated.rows("ab")[0].generation,
                first.rows("ab")[0].generation
            );
            let disk: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(disk["version"], 2);
            assert_eq!(
                disk["providers"]["a"][0]["generation"],
                serde_json::to_value(stamp.generation).unwrap()
            );
            assert_eq!(
                store
                    .replace_if_matches(
                        "a",
                        stamp,
                        Credential::new_api_key("LEGACY-REFRESHED".into()),
                    )
                    .await
                    .unwrap(),
                CasOutcome::Applied
            );
            let refreshed = store.snapshot().await.unwrap().rows("a")[0].stamp();
            assert_eq!(refreshed.generation, stamp.generation);
            assert_eq!(refreshed.rev, stamp.rev + 1);
        }
    }

    #[tokio::test]
    async fn recreated_auth_file_rejects_old_generation_even_when_id_and_rev_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let store = FileAuthStore::at(&path);
        put(&store, "aba", "ACCOUNT-A").await;
        let original = store.snapshot().await.unwrap().rows("aba")[0].stamp();
        std::fs::remove_file(&path).unwrap();
        put(&store, "aba", "ACCOUNT-B").await;
        let replacement = store.snapshot().await.unwrap().rows("aba")[0].stamp();
        assert_eq!(original.id, replacement.id);
        assert_eq!(original.rev, replacement.rev);
        assert_ne!(original.generation, CredentialGeneration::default());
        assert_ne!(replacement.generation, CredentialGeneration::default());
        assert_ne!(original.generation, replacement.generation);
        let before = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let peer = FileAuthStore::at(&path);
        assert_eq!(
            peer.replace_if_matches(
                "aba",
                original,
                Credential::new_api_key("STALE-A-REFRESH".into()),
            )
            .await
            .unwrap(),
            CasOutcome::Stale
        );
        assert_eq!(
            peer.disable_if_matches("aba", original, "stale account A failure")
                .await
                .unwrap(),
            CasOutcome::Stale
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let snapshot = store.snapshot().await.unwrap();
        assert_eq!(snapshot.rows("aba")[0].stamp(), replacement);
        assert_eq!(
            snapshot.rows("aba")[0].credential.expose_api_key(),
            Some("ACCOUNT-B")
        );
        assert!(snapshot.rows("aba")[0].disabled_cause.is_none());
        assert_eq!(
            peer.disable_if_matches("aba", replacement, "current account B failure")
                .await
                .unwrap(),
            CasOutcome::Applied
        );
        let disabled = store.snapshot().await.unwrap().rows("aba")[0].stamp();
        assert_eq!(disabled.generation, replacement.generation);
        assert_eq!(disabled.rev, replacement.rev + 1);
    }

    #[tokio::test]
    async fn remove_deletes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileAuthStore::at(tmp.path().join("auth.json"));
        put(&store, "ephemeral", "gone-soon").await;
        store.remove("ephemeral").await.unwrap();
        assert!(store.snapshot().await.unwrap().rows("ephemeral").is_empty());
    }

    #[tokio::test]
    async fn list_returns_all_stored_providers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileAuthStore::at(tmp.path().join("auth.json"));
        put(&store, "provider-a", "ka").await;
        put(&store, "provider-b", "kb").await;
        let snapshot = store.snapshot().await.unwrap();
        assert_eq!(snapshot.providers.len(), 2);
        assert!(snapshot.providers.contains_key("provider-a"));
        assert!(snapshot.providers.contains_key("provider-b"));
        for rows in snapshot.providers.values() {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].origin, CredentialOrigin::Stored);
            assert!(rows[0].last_validated.is_some());
            assert!(rows[0].disabled_cause.is_none());
        }
    }

    #[test]
    fn get_sync_returns_stored_key() {
        run_data_dir_scenario("get_sync_returns_stored_key");
    }

    #[test]
    fn auth_json_round_trip_persists_real_key() {
        run_data_dir_scenario("auth_json_round_trip_persists_real_key");
    }

    #[tokio::test]
    async fn auth_store_subprocess_scenario() {
        let Some(scenario) =
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_AUTH_STORE_TEST_SCENARIO")
        else {
            return;
        };
        let root = PathBuf::from(
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_AUTH_STORE_TEST_DIR")
                .expect("parent-owned test directory"),
        );
        // Fail before touching a store if the child could fall back to user data.
        assert!(root.is_absolute() && root.is_dir());
        assert_eq!(
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_DATA_DIR")
                .expect("child data directory"),
            root.to_str().expect("UTF-8 test directory")
        );
        let path = root.join("auth.json");
        assert_eq!(FileAuthStore::auth_file_path().unwrap(), path);
        assert!(!path.exists());

        match scenario.as_str() {
            "get_sync_returns_stored_key" => {
                std::fs::write(
                    &path,
                    r#"{"version":1,"providers":{"test":{"type":"api_key","api_key":"sync-test-key","last_validated":null}}}"#,
                )
                .unwrap();
                let got = FileAuthStore::snapshot_sync();
                assert_eq!(
                    got.rows("test")[0].credential.expose_api_key(),
                    Some("sync-test-key")
                );
                assert!(got.rows("nonexistent").is_empty());
            }
            "auth_json_round_trip_persists_real_key" => {
                let real_key = "sk-REAL-KEY-FOR-ROUND-TRIP-TEST";
                let store = FileAuthStore::new();
                put(&store, "rt-provider", real_key).await;
                let raw = std::fs::read_to_string(&path).unwrap();
                assert!(raw.contains(real_key));
                assert!(!raw.contains("***"));
                assert_eq!(
                    FileAuthStore::snapshot_sync().rows("rt-provider")[0]
                        .credential
                        .expose_api_key(),
                    Some(real_key)
                );
                assert_eq!(
                    store.snapshot().await.unwrap().rows("rt-provider")[0]
                        .credential
                        .expose_api_key(),
                    Some(real_key)
                );
            }
            _ => panic!("unknown auth store subprocess scenario: {scenario}"),
        }
        // Written only after the selected scenario has passed every assertion.
        std::fs::write(root.join("scenario.completed"), b"complete")
            .expect("record completed auth scenario");
    }
}
