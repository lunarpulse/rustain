//! Credential-scoped disk cache for raw model-discovery records.
//!
//! The embedded seed retains its independent v2 schema. Discovery uses v3 at
//! `~/.rustain/models_cache.json`; older files are discarded and fetched again.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::domain::models::provider::{ModelDescriptor, ModelRecord};
use crate::domain::ports::StreamingProvider;
use crate::infrastructure::paths;

/// Embedded seed schema version, independent from the discovery cache.
const CURRENT_VERSION: u32 = 2;
const DISCOVERY_CACHE_VERSION: u32 = 3;
/// Bound for another process's cache write, as the refresh lease bounds its peer.
const CACHE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The credential a discovery request sent, reduced to cache identity.
pub enum ScopeCredential<'a> {
    /// A stored OAuth login's account/org/project identity key, read from the
    /// same row as the token: stable across access rotation, and unlike row
    /// ids never reissued to another account when `auth.json` is recreated.
    OAuthIdentity(&'a str),
    /// The bearer/API key actually sent (also for an OAuth row with no identity).
    Secret(&'a crate::domain::models::SecretString),
    /// A resolved grant: its bearer, plus the routing of the row that produced it.
    Grant(&'a crate::domain::models::ResolvedAuth),
    Keyless,
}

/// Opaque 16-hex cache scope over the endpoint `context` and the credential.
/// Routing (enterprise domain, API endpoint) is part of the credential: two
/// grants that differ only by where they are served never share a roster.
/// Grants without routing keep their pre-routing digests.
pub fn credential_scope(context: &str, credential: ScopeCredential<'_>) -> String {
    use crate::domain::models::secret::scope_digest;
    match credential {
        ScopeCredential::OAuthIdentity(identity) => {
            scope_digest(&[context.as_bytes(), b"oauth".as_slice(), identity.as_bytes()])
        }
        ScopeCredential::Secret(secret) => secret.fingerprint(context),
        ScopeCredential::Grant(auth) => match auth.routing().filter(|r| !r.is_empty()) {
            None => auth.bearer_secret().fingerprint(context),
            Some(routing) => auth.bearer_secret().fingerprint(&format!(
                "{context}\0enterprise={}\0endpoint={}",
                routing.enterprise_domain.as_deref().unwrap_or_default(),
                routing.api_endpoint.as_deref().unwrap_or_default()
            )),
        },
        ScopeCredential::Keyless => scope_digest(&[context.as_bytes(), b"keyless".as_slice()]),
    }
}

/// Cache scope of the credential a request resolved: a stored OAuth login by
/// the account identity of that same row (stable across access rotation),
/// otherwise — including an identity-less OAuth row — the exact bearer sent and
/// the row's routing. Every request-resolving adapter stamps its discovery with
/// this one function.
pub fn request_scope(
    context: &str,
    resolved: &crate::infrastructure::key_resolver::Resolved,
) -> String {
    match (&resolved.source, resolved.identity.as_deref()) {
        (crate::domain::models::AuthSource::StoredOAuth, Some(identity)) => {
            credential_scope(context, ScopeCredential::OAuthIdentity(identity))
        }
        _ => credential_scope(context, ScopeCredential::Grant(&resolved.auth)),
    }
}

/// Replay scope of the credential a request resolved: stamped on the native
/// items a response produces and required of every item a request replays.
/// [`request_scope`], except that a stored OAuth row without an account
/// identity is scoped by its login epoch (the row `generation`, minted per
/// login and kept across refresh) instead of its rotating access token, so a
/// routine refresh never drops that same account's items mid-session.
pub fn replay_scope(
    context: &str,
    resolved: &crate::infrastructure::key_resolver::Resolved,
) -> String {
    match (&resolved.source, resolved.identity.as_deref(), resolved.row) {
        (crate::domain::models::AuthSource::StoredOAuth, None, Some(row)) => {
            crate::domain::models::secret::scope_digest(&[
                context.as_bytes(),
                b"login".as_slice(),
                row.generation.0.as_slice(),
            ])
        }
        _ => request_scope(context, resolved),
    }
}

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

/// Credential-scoped discovery data; never used to deserialize the bundled seed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryCacheFile {
    pub version: u32,
    pub providers: HashMap<String, CachedDiscoveryEntry>,
}

impl Default for DiscoveryCacheFile {
    fn default() -> Self {
        Self {
            version: DISCOVERY_CACHE_VERSION,
            providers: HashMap::new(),
        }
    }
}

/// Raw records preserve absent fields for later metadata-layer merges.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedDiscoveryEntry {
    pub fetched_at_unix: i64,
    pub credential_fingerprint: String,
    pub authoritative: bool,
    pub models: Vec<ModelRecord>,
}

fn parse_discovery_cache(text: &str) -> DiscoveryCacheFile {
    match serde_json::from_str::<DiscoveryCacheFile>(text) {
        Ok(catalog) if catalog.version == DISCOVERY_CACHE_VERSION => catalog,
        Ok(catalog) => {
            tracing::info!(
                "models_cache.json version {} differs from current {}; rebuilding",
                catalog.version,
                DISCOVERY_CACHE_VERSION
            );
            DiscoveryCacheFile::default()
        }
        Err(e) => {
            tracing::warn!("models_cache.json parse error: {e}; rebuilding");
            DiscoveryCacheFile::default()
        }
    }
}

fn load_discovery_file(path: &Path) -> DiscoveryCacheFile {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_discovery_cache(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiscoveryCacheFile::default(),
        Err(e) => {
            tracing::warn!("models_cache.json read error: {e}; using empty catalog");
            DiscoveryCacheFile::default()
        }
    }
}

/// The sidecar survives atomic replacements of the data file. Closing the
/// descriptor releases the advisory lock, including error and process-exit paths.
fn lock_discovery_file(path: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `file` owns a valid descriptor throughout the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    file.lock()?;
    Ok(file)
}

fn write_discovery_file(path: &Path, json: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!("models_cache.json.{}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Cache manager with process-wide async and cross-process advisory write locks.
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
        static WRITE_LOCK: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));
        Self {
            write_lock: Arc::clone(&WRITE_LOCK),
        }
    }

    /// Load discovery records; missing, incompatible or corrupt files are empty.
    pub async fn load(&self) -> DiscoveryCacheFile {
        let path = match paths::models_cache_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("models_cache_path error: {e}; using empty catalog");
                return DiscoveryCacheFile::default();
            }
        };

        match tokio::fs::read_to_string(&path).await {
            Ok(text) => parse_discovery_cache(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiscoveryCacheFile::default(),
            Err(e) => {
                tracing::warn!("models_cache.json read error: {e}; using empty catalog");
                DiscoveryCacheFile::default()
            }
        }
    }

    /// Synchronous startup counterpart, with the same compatibility policy.
    pub fn load_sync(&self) -> DiscoveryCacheFile {
        let path = match paths::models_cache_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("models_cache_path error: {e}; using empty catalog");
                return DiscoveryCacheFile::default();
            }
        };

        load_discovery_file(&path)
    }

    /// Replace a complete snapshot. Production discovery uses `upsert_provider`
    /// instead, so a stale snapshot cannot overwrite another provider's update.
    pub async fn save(&self, catalog: &DiscoveryCacheFile) -> anyhow::Result<()> {
        let path = paths::models_cache_path()?;
        let json = serde_json::to_vec_pretty(catalog)?;
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _guard = guard;
            let _file_lock = lock_discovery_file(&path)?;
            write_discovery_file(&path, &json)?;
            Ok(())
        })
        .await?
    }

    /// Atomically update one provider without dropping another process's entries.
    /// The sidecar wait is bounded; once held, the blocking critical section owns
    /// both locks until the read-modify-write completes, even if the caller is
    /// cancelled.
    pub async fn upsert_provider(
        &self,
        provider_id: &str,
        entry: CachedDiscoveryEntry,
    ) -> anyhow::Result<()> {
        let path = paths::models_cache_path()?;
        let provider_id = provider_id.to_string();
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        let lock_path = path.clone();
        // A stalled peer holding the sidecar lock yields a typed failure (the
        // caller keeps its live result in memory), never an unbounded wait.
        let acquired = tokio::time::timeout(
            CACHE_LOCK_WAIT,
            tokio::task::spawn_blocking(move || lock_discovery_file(&lock_path)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("models_cache.json is locked by another process"))?;
        let file_lock = acquired??;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _guard = guard;
            let _file_lock = file_lock;
            let mut catalog = load_discovery_file(&path);
            #[cfg(all(test, unix))]
            tests::pause_writer_after_read()?;
            catalog.providers.insert(provider_id, entry);
            let json = serde_json::to_vec_pretty(&catalog)?;
            write_discovery_file(&path, &json)?;
            Ok(())
        })
        .await?
    }

    /// Check whether a provider entry is still fresh given its TTL.
    pub fn is_fresh(&self, entry: &CachedDiscoveryEntry, ttl_seconds: u64, now_unix: i64) -> bool {
        let elapsed = now_unix.saturating_sub(entry.fetched_at_unix) as u64;
        elapsed < ttl_seconds
    }
}

/// Target for background catalog discovery.
#[derive(Clone)]
pub struct DiscoveryTarget {
    pub provider_id: String,
    /// Transport/catalogue kind, independent of the configured runtime map key.
    pub kind: String,
    pub provider: Arc<dyn StreamingProvider>,
    pub cache_ttl_seconds: u64,
    pub model_filter: Vec<String>,
    /// Startup scope (store read at construction); refreshes re-read it through
    /// the adapter's request path and stamp the scope a request actually used.
    pub credential_fingerprint: String,
    /// Endpoint part of every scope for this target (provider id + base URL).
    pub scope_context: String,
    /// The newest live roster this process discovered, even if persisting failed.
    pub last_live: Arc<arc_swap::ArcSwapOption<CachedDiscoveryEntry>>,
    /// Publication order of overlapping refreshes, shared by every clone.
    pub refresh_order: Arc<RefreshOrder>,
    pub bundled: Vec<ModelDescriptor>,
    pub sign_in_hint: String,
    pub display_name: String,
}

/// Orders overlapping refreshes of one target (Story 20.1d1b): a refresh that
/// started earlier never publishes, persists or sets status over one that
/// started later, e.g. a delayed response for an account that was replaced.
#[derive(Default)]
pub struct RefreshOrder {
    started: std::sync::atomic::AtomicU64,
    published: Mutex<u64>,
}

impl RefreshOrder {
    /// Ticket taken when a refresh starts; later starts get larger tickets.
    pub fn ticket(&self) -> u64 {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// The publication slot for `ticket`, held across persist and publish, or
    /// `None` once a later-started refresh has published.
    pub async fn publish(&self, ticket: u64) -> Option<tokio::sync::MutexGuard<'_, u64>> {
        let mut published = self.published.lock().await;
        if *published > ticket {
            return None;
        }
        *published = ticket;
        Some(published)
    }
}

// ── Embedded seed catalog (Story 7.7 AC1) ────────────────────────────────

/// Shipped `models_variants.json` content baked into the binary at compile time.
/// Zero I/O — lives in `.rodata`. Used by all adapters as the Tier-0 seed.
const EMBEDDED_SEED_JSON: &str = include_str!("../../models_variants.json");

/// Accessor for the raw embedded JSON text (used by `update-catalog` CLI, startup).
pub fn embedded_seed_json() -> &'static str {
    EMBEDDED_SEED_JSON
}

/// Lazily deserialized embedded seed catalog. Returns a static reference —
/// zero allocations after first call.
pub fn load_embedded_seed() -> Option<&'static CachedCatalog> {
    static SEED: OnceLock<Option<CachedCatalog>> = OnceLock::new();
    SEED.get_or_init(|| serde_json::from_str(EMBEDDED_SEED_JSON).ok())
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::provider::ModelCapability;
    use serial_test::serial;

    fn temp_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", dir.path());
        }
        dir
    }

    /// A refresh rotates an identity-less login's access token and bumps its
    /// revision; its replay scope stays the login's, while another login (a new
    /// generation) is another scope. Identified rows and keys keep the request scope.
    #[test]
    fn replay_scope_keeps_an_identityless_login_across_access_rotation() {
        use crate::domain::models::{
            AuthSource, CredentialGeneration, CredentialId, CredentialStamp, ResolvedAuth,
        };
        use crate::infrastructure::key_resolver::Resolved;
        let oauth = |access: &str, rev: u64, generation: u8, identity: Option<&str>| Resolved {
            source: AuthSource::StoredOAuth,
            auth: ResolvedAuth::OAuth {
                access: access.into(),
                account_id: None,
                project_id: None,
                routing: Default::default(),
            },
            row: Some(CredentialStamp {
                id: CredentialId(1),
                rev,
                generation: CredentialGeneration([generation; 16]),
            }),
            identity: identity.map(str::to_owned),
        };
        let before = oauth("access-1", 1, 7, None);
        let after = oauth("access-2", 2, 7, None);
        assert_ne!(
            request_scope("xai-oauth", &before),
            request_scope("xai-oauth", &after)
        );
        assert_eq!(
            replay_scope("xai-oauth", &before),
            replay_scope("xai-oauth", &after)
        );
        assert_ne!(
            replay_scope("xai-oauth", &before),
            replay_scope("xai-oauth", &oauth("access-1", 1, 8, None))
        );
        let identified = oauth("access-1", 1, 7, Some("account:a"));
        assert_eq!(
            replay_scope("openai-codex", &identified),
            request_scope("openai-codex", &identified)
        );
        let key = Resolved {
            source: AuthSource::StoredKey,
            auth: ResolvedAuth::ApiKey("sk-or-v1-key".into()),
            row: None,
            identity: None,
        };
        assert_eq!(
            replay_scope("openrouter", &key),
            request_scope("openrouter", &key)
        );
    }

    fn discovery_entry() -> CachedDiscoveryEntry {
        CachedDiscoveryEntry {
            fetched_at_unix: 1000,
            credential_fingerprint: "0123456789abcdef".to_string(),
            authoritative: true,
            models: vec![
                ModelRecord {
                    model_id: "m1".to_string(),
                    display_name: Some("Model One".to_string()),
                    context_window: Some(128_000),
                    capabilities: Some([ModelCapability::ToolUse].into_iter().collect()),
                    pricing_tier: Some("flagship".to_string()),
                    wire_model_id: None,
                },
                ModelRecord {
                    model_id: "m2".to_string(),
                    ..Default::default()
                },
            ],
        }
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn v3_async_and_sync_roundtrip_preserves_sparse_records() {
        let dir = temp_dir();
        let cache = ModelCatalogCache::new();
        let mut catalog = cache.load().await;
        catalog
            .providers
            .insert("openrouter".to_string(), discovery_entry());
        cache.save(&catalog).await.unwrap();

        for loaded in [cache.load().await, cache.load_sync()] {
            assert_eq!(loaded.version, 3);
            assert_eq!(loaded.providers.len(), 1);
            let entry = &loaded.providers["openrouter"];
            assert_eq!(entry.fetched_at_unix, 1000);
            assert_eq!(entry.credential_fingerprint, "0123456789abcdef");
            assert!(entry.authoritative);
            assert_eq!(entry.models.len(), 2);
            assert_eq!(entry.models[0].model_id, "m1");
            assert_eq!(entry.models[0].display_name.as_deref(), Some("Model One"));
            assert_eq!(entry.models[0].context_window, Some(128_000));
            let capabilities = entry.models[0].capabilities.as_ref().unwrap();
            assert_eq!(capabilities.len(), 1);
            assert!(capabilities.contains(&ModelCapability::ToolUse));
            assert_eq!(entry.models[0].pricing_tier.as_deref(), Some("flagship"));
            assert_eq!(entry.models[1].model_id, "m2");
            assert!(entry.models[1].display_name.is_none());
            assert!(entry.models[1].context_window.is_none());
            assert!(entry.models[1].capabilities.is_none());
            assert!(entry.models[1].pricing_tier.is_none());
        }
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("models_cache.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(json["version"], 3);
        assert_eq!(json["providers"]["openrouter"]["fetchedAtUnix"], 1000);
        assert_eq!(
            json["providers"]["openrouter"]["credentialFingerprint"],
            "0123456789abcdef"
        );
        assert!(
            !dir.path()
                .join(format!("models_cache.json.{}.tmp", std::process::id()))
                .exists()
        );
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn load_missing_file_returns_default() {
        let _dir = temp_dir();
        let cache = ModelCatalogCache::new();
        for loaded in [cache.load().await, cache.load_sync()] {
            assert_eq!(loaded.version, 3);
            assert!(loaded.providers.is_empty());
        }
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn load_corrupt_file_returns_default() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("models_cache.json"), "{garbage}").unwrap();
        let cache = ModelCatalogCache::new();
        for loaded in [cache.load().await, cache.load_sync()] {
            assert_eq!(loaded.version, 3);
            assert!(loaded.providers.is_empty());
        }
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn v2_discovery_file_is_discarded_without_changing_seed_schema() {
        let dir = temp_dir();
        let mut seed = CachedCatalog::default();
        seed.providers.insert(
            "openrouter".to_string(),
            CachedProviderEntry {
                fetched_at_unix: 1000,
                models: vec![CachedModelEntry {
                    descriptor: ModelDescriptor {
                        model_id: "old-account-model".to_string(),
                        display_name: "Old Account".to_string(),
                        provider_id: "openrouter".to_string(),
                        context_window: 128_000,
                        capabilities: Default::default(),
                        pricing_tier: None,
                        stale: false,
                        wire_model_id: None,
                    },
                }],
            },
        );
        assert_eq!(seed.version, 2);
        std::fs::write(
            dir.path().join("models_cache.json"),
            serde_json::to_vec(&seed).unwrap(),
        )
        .unwrap();
        let cache = ModelCatalogCache::new();
        for loaded in [cache.load().await, cache.load_sync()] {
            assert_eq!(loaded.version, 3);
            assert!(loaded.providers.is_empty());
        }
        assert!(!load_embedded_seed().unwrap().providers.is_empty());
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn incompatible_discovery_versions_are_discarded() {
        let dir = temp_dir();
        let cache = ModelCatalogCache::new();
        for version in [2, 4] {
            let mut catalog = DiscoveryCacheFile {
                version,
                ..Default::default()
            };
            catalog
                .providers
                .insert("openrouter".to_string(), discovery_entry());
            std::fs::write(
                dir.path().join("models_cache.json"),
                serde_json::to_vec(&catalog).unwrap(),
            )
            .unwrap();
            assert!(cache.load().await.providers.is_empty());
            assert!(cache.load_sync().providers.is_empty());
        }
    }

    #[test]
    fn is_fresh_boundaries() {
        let cache = ModelCatalogCache::new();
        let entry = discovery_entry();
        assert!(cache.is_fresh(&entry, 600, 1500));
        assert!(!cache.is_fresh(&entry, 600, 1600));
        assert!(!cache.is_fresh(&entry, 600, 1601));
        assert!(!cache.is_fresh(&entry, 0, 1000));
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn independent_cache_instances_share_the_write_lock() {
        let first = ModelCatalogCache::new();
        let second = ModelCatalogCache::new();
        let guard = first.write_lock.lock().await;
        assert!(second.write_lock.try_lock().is_err());
        drop(guard);
        assert!(second.write_lock.try_lock().is_ok());
    }

    #[tokio::test]
    #[serial(rustain_data_dir)]
    async fn provider_upsert_retains_unrelated_entries_and_replaces_its_own() {
        let _dir = temp_dir();
        let first = ModelCatalogCache::new();
        let second = ModelCatalogCache::new();
        assert!(second.load().await.providers.is_empty());
        first
            .upsert_provider("openai", discovery_entry())
            .await
            .unwrap();
        second
            .upsert_provider("openrouter", discovery_entry())
            .await
            .unwrap();
        let mut replacement = discovery_entry();
        replacement.credential_fingerprint = "fedcba9876543210".to_string();
        replacement.models.truncate(1);
        first.upsert_provider("openai", replacement).await.unwrap();

        let loaded = second.load_sync();
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.providers["openai"].models.len(), 1);
        assert_eq!(
            loaded.providers["openai"].credential_fingerprint,
            "fedcba9876543210"
        );
        assert_eq!(loaded.providers["openrouter"].models.len(), 2);
        assert_eq!(
            loaded.providers["openrouter"].credential_fingerprint,
            "0123456789abcdef"
        );
    }

    /// Pause only the designated subprocess at the real persistence boundary.
    #[cfg(unix)]
    pub(super) fn pause_writer_after_read() -> std::io::Result<()> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let Some(socket) = std::env::var_os("RUSTAIN_CACHE_TEST_AFTER_READ_SOCKET") else {
            return Ok(());
        };
        let mut control = UnixStream::connect(socket)?;
        control.write_all(b"held")?;
        let mut release = [0];
        control.read_exact(&mut release)?;
        if &release != b"g" {
            return Err(std::io::Error::other("invalid cache-writer release"));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discovery_cache_subprocess_writer() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let Some(provider) =
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_CACHE_TEST_WRITER")
        else {
            return;
        };
        let cache = ModelCatalogCache::new();
        // The parent starts both consumers before the first locked write
        // completes. Each observes the old snapshot before its upsert.
        assert!(cache.load_sync().providers.is_empty());
        let mut control =
            UnixStream::connect(std::env::var_os("RUSTAIN_CACHE_TEST_SOCKET").unwrap()).unwrap();
        control.write_all(b"ready").unwrap();
        let mut go = [0];
        control.read_exact(&mut go).unwrap();
        assert_eq!(&go, b"g");
        if std::env::var_os("RUSTAIN_CACHE_TEST_PROBE_LOCK").is_some() {
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .open(
                    paths::models_cache_path()
                        .unwrap()
                        .with_extension("json.lock"),
                )
                .unwrap();
            // SAFETY: `lock` owns a valid descriptor throughout the probe.
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            let blocked = result != 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
            // Report instead of asserting here: the parent must release the
            // paused writer even when the production lock has been removed.
            control
                .write_all(if blocked { b"b" } else { b"a" })
                .unwrap();
            drop(lock);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(cache.upsert_provider(&provider, discovery_entry()))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[serial(rustain_data_dir)]
    fn concurrent_process_provider_writes_retain_both_entries() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::process::{Command, Stdio};

        let dir = temp_dir();
        let socket = dir.path().join("writers.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let helper = format!(
            "{}::discovery_cache_subprocess_writer",
            module_path!().split_once("::").unwrap().1
        );
        let spawn = |provider: &str, hold_after_read: bool| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &helper])
                .env("RUSTAIN_CACHE_TEST_WRITER", provider)
                .env("RUSTAIN_CACHE_TEST_SOCKET", &socket)
                .env("RUSTAIN_DATA_DIR", dir.path())
                .env_remove("RUSTAIN_CACHE_TEST_AFTER_READ_SOCKET")
                .env_remove("RUSTAIN_CACHE_TEST_PROBE_LOCK")
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            if hold_after_read {
                command.env("RUSTAIN_CACHE_TEST_AFTER_READ_SOCKET", &socket);
            } else {
                command.env("RUSTAIN_CACHE_TEST_PROBE_LOCK", "1");
            }
            command.spawn().unwrap()
        };
        let first = spawn("openai", true);
        let (mut first_control, _) = listener.accept().unwrap();
        let mut ready = [0; 5];
        first_control.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        first_control.write_all(b"g").unwrap();

        // This connection is made after the real locked reread, before rename.
        let (mut held, _) = listener.accept().unwrap();
        let mut boundary = [0; 4];
        held.read_exact(&mut boundary).unwrap();
        assert_eq!(&boundary, b"held");
        let second = spawn("openrouter", false);
        let (mut second_control, _) = listener.accept().unwrap();
        second_control.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        second_control.write_all(b"g").unwrap();
        let mut probe = [0];
        let probe_result = second_control.read_exact(&mut probe);
        // Always unblock the first writer before checking the probe's result.
        held.write_all(b"g").unwrap();
        for child in [first, second] {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        probe_result.unwrap();
        assert_eq!(
            &probe, b"b",
            "the independent writer must be excluded while the first holds the persistence lock"
        );
        use std::os::fd::AsRawFd;
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("models_cache.json.lock"))
            .unwrap();
        // SAFETY: `lock` owns the descriptor; both writer processes have exited.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "the advisory lock must be released after persistence"
        );
        drop(lock);
        let loaded = ModelCatalogCache::new().load_sync();
        assert_eq!(loaded.providers.len(), 2);
        for provider in ["openai", "openrouter"] {
            let entry = &loaded.providers[provider];
            assert_eq!(entry.models.len(), 2);
            assert_eq!(entry.models[0].model_id, "m1");
            assert_eq!(entry.credential_fingerprint, "0123456789abcdef");
            assert!(entry.authoritative);
        }
    }
}
