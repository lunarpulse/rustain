//! Story 20.1a front-door conformance. Real files, flock and atomic rename;
//! only the OAuth transport and epoch clock are injected. Expected winners are
//! literals, never answers obtained from the cascade under test.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::errors::AuthError;
use rustain::domain::models::{
    AuthSource, CasOutcome, Credential, CredentialId, CredentialIdentity, CredentialOrigin,
    OAuthCredential, ResolvedAuth, SecretString,
};
use rustain::domain::ports::{AuthStorePort, OAuthRefreshPort, RefreshFailure, RefreshedTokens};
use rustain::infrastructure::key_resolver::{KeyResolver, ResolveError};
use rustain::infrastructure::provider_factory::{resolve_api_key, resolve_auth};
use serde_json::{Value, json};
use serial_test::serial;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore};

const NOW: i64 = 1_000_000;
const FRESH: i64 = NOW + 600_000;
const PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "GOOGLE_API_KEY",
    "DEEPSEEK_API_KEY",
    "MOONSHOT_API_KEY",
];

/// Restores even non-UTF8 environment values, including during unwinding.
/// Every caller that changes the parent environment uses the same unkeyed
/// serial lock as the unchanged cli_auth regression oracle.
struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

impl EnvRestore {
    fn capture(names: &[&'static str]) -> Self {
        Self(
            names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        )
    }

    fn set(&self, name: &str, value: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: callers hold #[serial], shared with cli_auth.
        unsafe { std::env::set_var(name, value) };
    }

    fn remove(&self, name: &str) {
        // SAFETY: callers hold #[serial], shared with cli_auth.
        unsafe { std::env::remove_var(name) };
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, original) in &self.0 {
            // SAFETY: the serial guard outlives this RAII guard.
            unsafe {
                match original {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn api(token: &str) -> Credential {
    Credential::ApiKey(SecretString::new(token.to_owned()))
}

fn identity(
    account: Option<&str>,
    email: Option<&str>,
    project: Option<&str>,
) -> CredentialIdentity {
    CredentialIdentity {
        account_id: account.map(str::to_owned),
        email: email.map(str::to_owned),
        project_id: project.map(str::to_owned),
    }
}

fn oauth(token: &str, expires_at_ms: i64, account: Option<&str>) -> Credential {
    Credential::OAuth(OAuthCredential {
        access: token.to_owned().into(),
        refresh: "STORED-REFRESH-CANARY".into(),
        expires_at_ms,
        identity: identity(account, None, None),
    })
}

fn refreshed() -> RefreshedTokens {
    RefreshedTokens {
        access: "REFRESHED-ACCESS-LITERAL".into(),
        refresh: Some("ROTATED-REFRESH-LITERAL".into()),
        expires_at_ms: FRESH,
        identity: CredentialIdentity::default(),
    }
}

fn key_row(id: u64, token: &str, origin: &str, disabled: Option<&str>) -> Value {
    json!({
        "id": id, "rev": 1, "origin": origin, "disabled_cause": disabled,
        "type": "api_key", "api_key": token, "last_validated": null
    })
}

fn oauth_row(id: u64, token: &str, expires: i64, disabled: Option<&str>) -> Value {
    json!({
        "id": id, "rev": 1, "origin": "login", "disabled_cause": disabled,
        "type": "oauth", "access": token, "refresh": "FIXTURE-REFRESH-CANARY",
        "expires_at": expires, "identity": {"account_id": format!("account-{id}")},
        "last_validated": null
    })
}

fn fixture(path: &Path, provider: &str, rows: Vec<Value>) {
    let next_id = rows
        .iter()
        .map(|row| row["id"].as_u64().unwrap())
        .max()
        .unwrap_or(0)
        + 1;
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "version": 2, "next_id": next_id, "providers": {provider: rows}
        }))
        .unwrap(),
    )
    .unwrap();
}

fn resolver(store: Arc<FileAuthStore>, refresh: Arc<dyn OAuthRefreshPort>) -> KeyResolver {
    KeyResolver::new(store, refresh, Arc::new(|| NOW))
}

fn assert_winner(result: Option<(AuthSource, ResolvedAuth)>, source: AuthSource, token: &str) {
    let (actual_source, auth) = result.expect("expected a winning credential");
    assert_eq!(actual_source, source);
    assert_eq!(auth.bearer_secret().expose_secret(), token);
}

struct Probe {
    calls: AtomicUsize,
    outcome: Result<RefreshedTokens, RefreshFailure>,
}

impl Probe {
    fn new(outcome: Result<RefreshedTokens, RefreshFailure>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            outcome,
        }
    }
}

#[async_trait]
impl OAuthRefreshPort for Probe {
    async fn refresh(
        &self,
        _: &str,
        _: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

struct GatedRefresh {
    entered: AtomicUsize,
    gate: Semaphore,
    outcome: Result<RefreshedTokens, RefreshFailure>,
}

impl GatedRefresh {
    fn new(outcome: Result<RefreshedTokens, RefreshFailure>) -> Self {
        Self {
            entered: AtomicUsize::new(0),
            gate: Semaphore::new(0),
            outcome,
        }
    }
}

#[async_trait]
impl OAuthRefreshPort for GatedRefresh {
    async fn refresh(
        &self,
        _: &str,
        _: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        self.outcome.clone()
    }
}

#[tokio::test]
async fn k01_v1_read_is_nonwriting_and_first_upsert_migrates_all_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let original = br#"{"version":1,"providers":{"zeta":{"type":"api_key","api_key":"V1-ZETA-LITERAL","last_validated":null},"alpha":{"type":"api_key","api_key":"V1-ALPHA-LITERAL","last_validated":"2026-01-01T00:00:00Z"}}}"#;
    std::fs::write(&path, original).unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let store = FileAuthStore::at(&path);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.providers.len(), 2);
    assert_eq!(snapshot.rows("alpha")[0].id, CredentialId(1));
    assert_eq!(snapshot.rows("zeta")[0].id, CredentialId(2));
    for row in snapshot.providers.values().flatten() {
        assert_eq!(row.origin, CredentialOrigin::Stored);
        assert_eq!(row.rev, 1);
        assert!(row.disabled_cause.is_none());
    }
    assert_eq!(
        snapshot.rows("alpha")[0].credential.expose_api_key(),
        Some("V1-ALPHA-LITERAL")
    );
    assert!(snapshot.rows("alpha")[0].last_validated.is_some());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );
    let id = store
        .upsert(
            "beta",
            api("V2-BETA-LITERAL"),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    assert_eq!(id, CredentialId(3));
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["version"], 2);
    assert_eq!(raw["next_id"], 4);
    assert_eq!(raw["providers"]["alpha"][0]["api_key"], "V1-ALPHA-LITERAL");
    assert_eq!(raw["providers"]["zeta"][0]["api_key"], "V1-ZETA-LITERAL");
    assert_eq!(raw["providers"]["alpha"][0]["origin"], "stored");
    assert_eq!(raw["providers"]["beta"][0]["api_key"], "V2-BETA-LITERAL");
}

/// M01: migrating a v1 key as login would incorrectly outrank the environment.
#[tokio::test]
#[serial]
async fn k02_m01_env_beats_migrated_v1_through_production_resolver() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvRestore::capture(&["RUSTAIN_DATA_DIR", "STORY20_AUTH_KEY"]);
    env.set("RUSTAIN_DATA_DIR", tmp.path());
    env.set("STORY20_AUTH_KEY", "MIGRATION-ENV-WINNER");
    let path = tmp.path().join("auth.json");
    std::fs::write(&path, br#"{"version":1,"providers":{"migration":{"type":"api_key","api_key":"MIGRATION-STORED-LOSER","last_validated":null}}}"#).unwrap();
    assert_eq!(
        resolve_api_key("STORY20_AUTH_KEY", "migration").as_deref(),
        Some("MIGRATION-ENV-WINNER")
    );
    let store = FileAuthStore::at(path);
    assert_eq!(
        store.snapshot().await.unwrap().rows("migration")[0].origin,
        CredentialOrigin::Stored
    );
}

/// M02: both legacy and v2 unknown rows must survive without JSON canonicalization.
#[tokio::test]
#[serial]
async fn k03_m02_unknown_rows_round_trip_verbatim_and_never_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvRestore::capture(&["RUSTAIN_DATA_DIR", "STORY20_AUTH_KEY"]);
    env.set("RUSTAIN_DATA_DIR", tmp.path());
    env.remove("STORY20_AUTH_KEY");
    let path = tmp.path().join("auth.json");
    let unknown = r#"{  "type" : "future_auth", "number":1.0e3, "label":"\u00e9", "access":"UNKNOWN-NEVER-SERVE"  }"#;
    for version in [1, 2] {
        let body = if version == 1 {
            format!(r#"{{"version":1,"providers":{{"unknown":{unknown}}}}}"#)
        } else {
            format!(r#"{{"version":2,"next_id":2,"providers":{{"unknown":[{unknown}]}}}}"#)
        };
        std::fs::write(&path, body).unwrap();
        let store = FileAuthStore::at(&path);
        assert_eq!(store.snapshot().await.unwrap().unknown_rows, 1);
        assert!(resolve_auth("STORY20_AUTH_KEY", "unknown").is_none());
        store
            .upsert(
                "unrelated",
                api("UNRELATED-KEY-LITERAL"),
                CredentialOrigin::Stored,
                false,
            )
            .await
            .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(unknown),
            "unknown row's exact raw bytes were lost: {raw}"
        );
        let snapshot = store.snapshot().await.unwrap();
        assert_eq!(snapshot.unknown_rows, 1);
        assert!(snapshot.rows("unknown").is_empty());
        assert_eq!(
            snapshot.rows("unrelated")[0].credential.expose_api_key(),
            Some("UNRELATED-KEY-LITERAL")
        );
        assert!(resolve_auth("STORY20_AUTH_KEY", "unknown").is_none());
    }
}

/// M03: the body is otherwise valid v2, so only the version guard protects it.
#[tokio::test]
async fn k04_m03_future_version_is_rejected_without_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let body = br#"{"version":3,"next_id":2,"providers":{"future":[{"id":1,"rev":1,"origin":"stored","disabled_cause":null,"type":"api_key","api_key":"FUTURE-KEY-LITERAL","last_validated":null}]}}"#;
    std::fs::write(&path, body).unwrap();
    let before = Sha256::digest(body);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let store = FileAuthStore::at(&path);
    assert!(matches!(
        store.snapshot().await,
        Err(AuthError::UnsupportedVersion(3))
    ));
    assert!(matches!(
        store
            .upsert("new", api("DO-NOT-WRITE"), CredentialOrigin::Stored, false)
            .await,
        Err(AuthError::UnsupportedVersion(3))
    ));
    assert!(matches!(
        store.remove("future").await,
        Err(AuthError::UnsupportedVersion(3))
    ));
    assert_eq!(Sha256::digest(std::fs::read(&path).unwrap()), before);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
}

/// M04: raw disk data is the positive control for all masked model surfaces.
#[tokio::test]
async fn k05_m04_oauth_models_mask_both_tokens_but_disk_retains_them() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = FileAuthStore::at(&path);
    let credential = oauth("MASKING-ACCESS-CANARY", FRESH, Some("masking"));
    store
        .upsert("masked", credential.clone(), CredentialOrigin::Login, true)
        .await
        .unwrap();
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("masked")[0];
    let resolved = ResolvedAuth::OAuth {
        access: "MASKING-ACCESS-CANARY".into(),
    };
    for output in [
        format!("{credential:?}"),
        format!("{credential}"),
        serde_json::to_string(&credential).unwrap(),
        format!("{row:?}"),
        format!("{row}"),
        serde_json::to_string(row).unwrap(),
        format!("{resolved:?}"),
        format!("{resolved}"),
        serde_json::to_string(&resolved).unwrap(),
    ] {
        assert!(!output.contains("MASKING-ACCESS-CANARY"));
        assert!(!output.contains("STORED-REFRESH-CANARY"));
    }
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(raw.contains("MASKING-ACCESS-CANARY"));
    assert!(raw.contains("STORED-REFRESH-CANARY"));
    assert!(!raw.contains("***"));
}

/// M05: each fixture removes the preceding stage, keeping distinct lower losers.
#[tokio::test]
async fn k06_m05_stage_order_and_anthropic_dual_env_are_literal() {
    let cases = [
        (
            true,
            true,
            true,
            true,
            AuthSource::Runtime,
            "RUNTIME-STAGE-LITERAL",
        ),
        (
            false,
            true,
            true,
            true,
            AuthSource::StoredOAuth,
            "OAUTH-STAGE-LITERAL",
        ),
        (
            false,
            false,
            true,
            true,
            AuthSource::LoginKey,
            "LOGIN-STAGE-LITERAL",
        ),
        (
            false,
            false,
            false,
            true,
            AuthSource::Env {
                var: "ANTHROPIC_AUTH_TOKEN".into(),
            },
            "AUTH-TOKEN-STAGE-LITERAL",
        ),
        (
            false,
            false,
            false,
            false,
            AuthSource::StoredKey,
            "STORED-STAGE-LITERAL",
        ),
    ];
    for (runtime_present, has_oauth, has_login, has_env, source, token) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let mut rows = vec![key_row(3, "STORED-STAGE-LITERAL", "stored", None)];
        if has_login {
            rows.push(key_row(2, "LOGIN-STAGE-LITERAL", "login", None));
        }
        if has_oauth {
            rows.push(oauth_row(1, "OAUTH-STAGE-LITERAL", FRESH, None));
        }
        fixture(&path, "anthropic", rows);
        let port = Arc::new(Probe::new(Ok(refreshed())));
        let resolver = resolver(Arc::new(FileAuthStore::at(path)), port.clone());
        let runtime = SecretString::from("RUNTIME-STAGE-LITERAL");
        let lookup = |name: &str| {
            if has_env {
                match name {
                    "ANTHROPIC_AUTH_TOKEN" => Some("  AUTH-TOKEN-STAGE-LITERAL  ".into()),
                    "ANTHROPIC_API_KEY" => Some("API-KEY-STAGE-LOSER".into()),
                    _ => None,
                }
            } else {
                None
            }
        };
        assert_winner(
            resolver
                .resolve(
                    "anthropic",
                    runtime_present.then_some(&runtime),
                    &["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"],
                    &lookup,
                )
                .await
                .unwrap(),
            source,
            token,
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
    let resolver = resolver(store, Arc::new(Probe::new(Ok(refreshed()))));
    let lookup = |name: &str| {
        Some(
            if name == "ANTHROPIC_AUTH_TOKEN" {
                " \t\n"
            } else {
                "  API-KEY-AFTER-BLANK  "
            }
            .into(),
        )
    };
    assert_winner(
        resolver
            .resolve(
                "anthropic",
                None,
                &["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"],
                &lookup,
            )
            .await
            .unwrap(),
        AuthSource::Env {
            var: "ANTHROPIC_API_KEY".into(),
        },
        "API-KEY-AFTER-BLANK",
    );
}

/// M06: all disabled credential kinds are present, but neither front door serves them.
#[tokio::test]
#[serial]
async fn k07_m06_disabled_rows_fall_through_in_sync_and_async_front_doors() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvRestore::capture(&["RUSTAIN_DATA_DIR", "STORY20_AUTH_KEY"]);
    env.set("RUSTAIN_DATA_DIR", tmp.path());
    env.set("STORY20_AUTH_KEY", "ENABLED-ENV-LITERAL");
    let path = tmp.path().join("auth.json");
    fixture(
        &path,
        "disabled",
        vec![
            // Fresh on the wall clock too, so the sync front door relies on the
            // disabled filter rather than on staleness.
            oauth_row(1, "DISABLED-OAUTH-LOSER", i64::MAX, Some("revoked")),
            key_row(2, "DISABLED-LOGIN-LOSER", "login", Some("revoked")),
            key_row(3, "DISABLED-STORED-LOSER", "stored", Some("revoked")),
        ],
    );
    let store = Arc::new(FileAuthStore::at(&path));
    let port = Arc::new(Probe::new(Ok(refreshed())));
    let resolver = resolver(store.clone(), port.clone());
    let lookup = |_: &str| Some("ENABLED-ENV-LITERAL".into());
    let sync = resolve_auth("STORY20_AUTH_KEY", "disabled").unwrap();
    assert_eq!(sync.bearer_secret().expose_secret(), "ENABLED-ENV-LITERAL");
    assert_winner(
        resolver
            .resolve("disabled", None, &["STORY20_AUTH_KEY"], &lookup)
            .await
            .unwrap(),
        AuthSource::Env {
            var: "STORY20_AUTH_KEY".into(),
        },
        "ENABLED-ENV-LITERAL",
    );
    env.remove("STORY20_AUTH_KEY");
    assert!(resolve_auth("STORY20_AUTH_KEY", "disabled").is_none());
    assert!(
        resolver
            .resolve("disabled", None, &[], &no_env)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.snapshot().await.unwrap().rows("disabled").len(), 3);
}

/// M07: equality at the skew boundary requires refresh, one millisecond beyond does not.
#[tokio::test]
async fn k08_m07_refresh_skew_boundary_is_strict() {
    for (expiry, expected_calls, token) in [
        (NOW + 60_000, 1, "REFRESHED-ACCESS-LITERAL"),
        (NOW + 60_001, 0, "BOUNDARY-ORIGINAL-LITERAL"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
        store
            .upsert(
                "boundary",
                oauth("BOUNDARY-ORIGINAL-LITERAL", expiry, Some("boundary")),
                CredentialOrigin::Login,
                false,
            )
            .await
            .unwrap();
        let port = Arc::new(Probe::new(Ok(refreshed())));
        let resolver = resolver(store, port.clone());
        assert_winner(
            resolver
                .resolve("boundary", None, &[], &no_env)
                .await
                .unwrap(),
            AuthSource::StoredOAuth,
            token,
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), expected_calls);
    }
}

/// M08: the current-thread arrived/yield gate is structural, never time-based.
#[tokio::test(flavor = "current_thread")]
async fn k09_m08_eight_concurrent_resolves_are_one_refresh_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
    store
        .upsert(
            "flight",
            oauth("FLIGHT-OLD-LITERAL", NOW, Some("flight")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(GatedRefresh::new(Ok(refreshed())));
    let resolver = Arc::new(resolver(store.clone(), port.clone()));
    let arrived = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let resolver = resolver.clone();
        let arrived = arrived.clone();
        tasks.push(tokio::spawn(async move {
            arrived.fetch_add(1, Ordering::SeqCst);
            resolver.resolve("flight", None, &[], &no_env).await
        }));
    }
    while arrived.load(Ordering::SeqCst) != 8 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    assert_eq!(
        port.entered.load(Ordering::SeqCst),
        1,
        "all eight callers reached their first pending await"
    );
    // Extra permits let the lock-removal mutant finish rather than hang.
    port.gate.add_permits(8);
    for task in tasks {
        assert_winner(
            task.await.unwrap().unwrap(),
            AuthSource::StoredOAuth,
            "REFRESHED-ACCESS-LITERAL",
        );
    }
    assert_eq!(port.entered.load(Ordering::SeqCst), 1);
    assert_eq!(store.snapshot().await.unwrap().rows("flight")[0].rev, 2);
}

/// M09: definitive disables; transient preserves and remains retryable without fallback.
#[tokio::test]
async fn k10_m09_definitive_and_transient_have_distinct_persistence_and_fallback() {
    for definitive in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
        let id = store
            .upsert(
                "failure",
                oauth("FAILURE-OLD-LITERAL", NOW, Some("failure")),
                CredentialOrigin::Login,
                false,
            )
            .await
            .unwrap();
        let failure = if definitive {
            RefreshFailure::Definitive("invalid_grant".into())
        } else {
            RefreshFailure::Transient("offline".into())
        };
        let port = Arc::new(Probe::new(Err(failure)));
        let resolver = resolver(store.clone(), port.clone());
        let lookup = |_: &str| Some("FAILURE-ENV-FALLBACK".into());
        assert_winner(
            resolver
                .resolve("failure", None, &["FALLBACK"], &lookup)
                .await
                .unwrap(),
            AuthSource::Env {
                var: "FALLBACK".into(),
            },
            "FAILURE-ENV-FALLBACK",
        );
        let snapshot = store.snapshot().await.unwrap();
        let row = &snapshot.rows("failure")[0];
        assert_eq!(row.id, id);
        if definitive {
            assert_eq!(row.rev, 2);
            assert_eq!(
                row.disabled_cause.as_deref(),
                Some("oauth refresh failed: invalid_grant")
            );
            assert!(
                resolver
                    .resolve("failure", None, &[], &no_env)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(port.calls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(row.rev, 1);
            assert!(row.disabled_cause.is_none());
            assert!(
                matches!(resolver.resolve("failure", None, &[], &no_env).await, Err(ResolveError::RefreshUnavailable { provider, cause }) if provider == "failure" && cause.contains("offline"))
            );
            assert_eq!(port.calls.load(Ordering::SeqCst), 2);
        }
        let Credential::OAuth(saved) = &row.credential else {
            panic!("expected OAuth")
        };
        assert_eq!(saved.access.expose_secret(), "FAILURE-OLD-LITERAL");
        assert_eq!(saved.refresh.expose_secret(), "STORED-REFRESH-CANARY");
    }
}

struct PeerRotation {
    peer: FileAuthStore,
    id: CredentialId,
    rev: u64,
    calls: AtomicUsize,
    outcome: Result<RefreshedTokens, RefreshFailure>,
    peer_expiry: i64,
}

#[async_trait]
impl OAuthRefreshPort for PeerRotation {
    async fn refresh(
        &self,
        _: &str,
        _: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            self.peer
                .replace_if_matches(
                    "peer",
                    self.id,
                    self.rev,
                    oauth("PEER-ROTATED-LITERAL", self.peer_expiry, Some("peer"))
                )
                .await
                .unwrap(),
            CasOutcome::Applied
        );
        self.outcome.clone()
    }
}

/// M10: rotation happens through a second real store before a definitive failure.
#[tokio::test]
async fn k11_m10_definitive_failure_adopts_peer_rotation_instead_of_disabling() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "peer",
            oauth("PEER-OLD-LITERAL", NOW, Some("peer")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(PeerRotation {
        peer: FileAuthStore::at(&path),
        id,
        rev: 1,
        calls: AtomicUsize::new(0),
        outcome: Err(RefreshFailure::Definitive("invalid_grant".into())),
        peer_expiry: FRESH,
    });
    let resolver = resolver(store.clone(), port.clone());
    assert_winner(
        resolver.resolve("peer", None, &[], &no_env).await.unwrap(),
        AuthSource::StoredOAuth,
        "PEER-ROTATED-LITERAL",
    );
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("peer")[0];
    assert_eq!(row.id, id);
    assert_eq!(row.rev, 2);
    assert!(row.disabled_cause.is_none());
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

fn auth_command(tmp: &Path, verb: &str) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::cargo_bin("rustain").unwrap();
    command
        .args(["auth", verb, "--json"])
        .current_dir(tmp)
        .env("RUSTAIN_DATA_DIR", tmp)
        .env("RUSTAIN_CONFIG_DIR", tmp.join("config"))
        .env("HOME", tmp)
        .env_remove("RUSTAIN_CONFIG");
    // Deliberately explicit: do not let the invoking shell select any provider.
    for name in PROVIDER_ENV {
        command.env_remove(name);
    }
    command
}

fn binary_json(mut command: assert_cmd::Command) -> Value {
    let output = command.assert().success().get_output().clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    for secret in [
        "BINARY-OAUTH-LITERAL",
        "BINARY-LOGIN-LITERAL",
        "BINARY-ENV-LITERAL",
        "BINARY-STORED-LITERAL",
        "BINARY-DISABLED-LITERAL",
        "BINARY-STALE-LITERAL",
        "FIXTURE-REFRESH-CANARY",
    ] {
        assert!(!stdout.contains(secret), "stdout leaked {secret}");
        assert!(!stderr.contains(secret), "stderr leaked {secret}");
    }
    serde_json::from_str(&stdout).unwrap()
}

fn provider_row<'a>(output: &'a Value, provider: &str) -> Option<&'a Value> {
    output["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == provider)
}

/// M12: six literal fixtures exercise construction, binary provenance and list.
#[tokio::test]
#[serial]
async fn k12_m12_binary_status_list_and_construction_share_six_fixture_cascade() {
    let env = EnvRestore::capture(&[
        "RUSTAIN_DATA_DIR",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ]);
    for (case, expected_token, expected_source, expected_configured) in [
        ("oauth", Some("BINARY-OAUTH-LITERAL"), Some("oauth"), true),
        (
            "login",
            Some("BINARY-LOGIN-LITERAL"),
            Some("login_key"),
            true,
        ),
        ("env", Some("BINARY-ENV-LITERAL"), Some("env"), true),
        (
            "stored",
            Some("BINARY-STORED-LITERAL"),
            Some("stored_key"),
            true,
        ),
        ("disabled", None, Some("disabled"), false),
        ("absent", None, None, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let mut rows = Vec::new();
        if matches!(case, "oauth" | "login" | "env" | "stored") {
            rows.push(key_row(3, "BINARY-STORED-LITERAL", "stored", None));
        }
        if matches!(case, "oauth" | "login") {
            rows.push(key_row(2, "BINARY-LOGIN-LITERAL", "login", None));
        }
        if case == "oauth" {
            rows.push(oauth_row(1, "BINARY-OAUTH-LITERAL", i64::MAX, None));
        }
        if case == "disabled" {
            rows.push(key_row(
                4,
                "BINARY-DISABLED-LITERAL",
                "stored",
                Some("operator disabled"),
            ));
        }
        fixture(&path, "anthropic", rows);
        let store = FileAuthStore::at(&path);
        assert_eq!(store.snapshot().await.unwrap().unknown_rows, 0);
        let has_env = matches!(case, "oauth" | "login" | "env");
        env.set("RUSTAIN_DATA_DIR", tmp.path());
        env.remove("ANTHROPIC_AUTH_TOKEN");
        if has_env {
            env.set("ANTHROPIC_API_KEY", "BINARY-ENV-LITERAL");
        } else {
            env.remove("ANTHROPIC_API_KEY");
        }
        assert_eq!(
            resolve_api_key("ANTHROPIC_API_KEY", "anthropic").as_deref(),
            expected_token,
            "construction fixture {case}"
        );
        let mut status_command = auth_command(tmp.path(), "status");
        let mut list_command = auth_command(tmp.path(), "list");
        if has_env {
            status_command.env("ANTHROPIC_API_KEY", "BINARY-ENV-LITERAL");
            list_command.env("ANTHROPIC_API_KEY", "BINARY-ENV-LITERAL");
        }
        let status = binary_json(status_command);
        assert_eq!(status["schema_version"], "2.0");
        match expected_source {
            Some(source) => {
                let row =
                    provider_row(&status, "anthropic").expect("configured or disabled status row");
                assert_eq!(row["source"], source, "status fixture {case}");
                if case == "disabled" {
                    assert_eq!(row["status"], "invalid");
                    assert_eq!(row["disabled_cause"], "operator disabled");
                }
            }
            None => assert!(provider_row(&status, "anthropic").is_none()),
        }
        let list = binary_json(list_command);
        assert_eq!(
            provider_row(&list, "anthropic").unwrap()["configured"],
            expected_configured,
            "list fixture {case}"
        );
    }
}

#[tokio::test]
#[serial]
async fn k12_stale_only_divergence_is_pinned_at_binary_front_doors() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvRestore::capture(&[
        "RUSTAIN_DATA_DIR",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ]);
    env.set("RUSTAIN_DATA_DIR", tmp.path());
    env.remove("ANTHROPIC_API_KEY");
    env.remove("ANTHROPIC_AUTH_TOKEN");
    let path = tmp.path().join("auth.json");
    fixture(
        &path,
        "anthropic",
        vec![oauth_row(1, "BINARY-STALE-LITERAL", 0, None)],
    );
    let store = FileAuthStore::at(&path);
    assert_eq!(store.snapshot().await.unwrap().rows("anthropic").len(), 1);
    assert!(resolve_api_key("ANTHROPIC_API_KEY", "anthropic").is_none());
    let status = binary_json(auth_command(tmp.path(), "status"));
    let row = provider_row(&status, "anthropic").unwrap();
    assert_eq!(row["source"], "oauth");
    assert_eq!(row["status"], "unknown");
    let list = binary_json(auth_command(tmp.path(), "list"));
    assert_eq!(
        provider_row(&list, "anthropic").unwrap()["configured"],
        true
    );
    let mut human = assert_cmd::Command::cargo_bin("rustain").unwrap();
    human
        .args(["auth", "status"])
        .current_dir(tmp.path())
        .env("RUSTAIN_DATA_DIR", tmp.path())
        .env("RUSTAIN_CONFIG_DIR", tmp.path().join("config"));
    for name in PROVIDER_ENV {
        human.env_remove(name);
    }
    let output = human.assert().success().get_output().clone();
    let text = String::from_utf8(output.stdout).unwrap();
    let line = text
        .lines()
        .find(|line| line.contains("Anthropic"))
        .unwrap();
    assert!(line.contains("oauth"));
    assert!(line.contains('⚠'));
    assert!(!text.contains("BINARY-STALE-LITERAL"));
    assert!(!text.contains("FIXTURE-REFRESH-CANARY"));
}

/// M11: omitted rotation retains the old refresh, identity fallback is per-field.
#[tokio::test]
async fn k13_m11_missing_refresh_is_retained_and_identity_merges_per_field() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
    let credential = Credential::OAuth(OAuthCredential {
        access: "MERGE-OLD-ACCESS".into(),
        refresh: "MERGE-OLD-REFRESH".into(),
        expires_at_ms: NOW,
        identity: identity(Some("old-account"), Some("old@email"), Some("old-project")),
    });
    let id = store
        .upsert("merge", credential, CredentialOrigin::Login, false)
        .await
        .unwrap();
    let response = RefreshedTokens {
        access: "MERGE-NEW-ACCESS".into(),
        refresh: None,
        expires_at_ms: FRESH,
        identity: identity(None, Some("new@email"), None),
    };
    let resolver = resolver(store.clone(), Arc::new(Probe::new(Ok(response))));
    assert_winner(
        resolver.resolve("merge", None, &[], &no_env).await.unwrap(),
        AuthSource::StoredOAuth,
        "MERGE-NEW-ACCESS",
    );
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("merge")[0];
    assert_eq!(row.id, id);
    assert_eq!(row.rev, 2);
    let Credential::OAuth(saved) = &row.credential else {
        panic!("expected OAuth")
    };
    assert_eq!(saved.access.expose_secret(), "MERGE-NEW-ACCESS");
    assert_eq!(saved.refresh.expose_secret(), "MERGE-OLD-REFRESH");
    assert_eq!(saved.expires_at_ms, FRESH);
    assert_eq!(saved.identity.account_id.as_deref(), Some("old-account"));
    assert_eq!(saved.identity.email.as_deref(), Some("new@email"));
    assert_eq!(saved.identity.project_id.as_deref(), Some("old-project"));
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_write_is_0600_replaces_inode_and_cleans_stale_tmp() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = FileAuthStore::at(&path);
    store
        .upsert(
            "atomic",
            api("ATOMIC-BEFORE"),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let stale = path.with_extension("json.tmp");
    std::fs::write(&stale, "interrupted-write").unwrap();
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();
    store
        .upsert(
            "atomic",
            api("ATOMIC-AFTER"),
            CredentialOrigin::Stored,
            true,
        )
        .await
        .unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_ne!(
        metadata.ino(),
        inode,
        "atomic replacement must rename a new inode"
    );
    assert!(!stale.exists());
    assert!(path.with_extension("json.lock").exists());
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot.rows("atomic")[0].credential.expose_api_key(),
        Some("ATOMIC-AFTER")
    );
    assert!(snapshot.rows("atomic")[0].last_validated.is_some());
}

/// Separate instances have separate in-process mutexes. The sibling flock must
/// serialize the entire read/modify/rename, not merely the final write.
#[cfg(unix)]
#[test]
fn flock_two_independent_writers_preserve_every_read_modify_write() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let start = Arc::new(std::sync::Barrier::new(2));
    let mut writers = Vec::new();
    for prefix in ["left", "right"] {
        let path = path.clone();
        let start = start.clone();
        writers.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let store = FileAuthStore::at(path);
            for index in 0..16 {
                start.wait();
                runtime
                    .block_on(store.upsert(
                        &format!("{prefix}-{index}"),
                        api(if prefix == "left" {
                            "LEFT-WRITER-LITERAL"
                        } else {
                            "RIGHT-WRITER-LITERAL"
                        }),
                        CredentialOrigin::Stored,
                        false,
                    ))
                    .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.join().unwrap();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let store = FileAuthStore::at(&path);
    let snapshot = runtime.block_on(store.snapshot()).unwrap();
    assert_eq!(snapshot.providers.len(), 32);
    let mut ids = std::collections::BTreeSet::new();
    for index in 0..16 {
        for (prefix, token) in [
            ("left", "LEFT-WRITER-LITERAL"),
            ("right", "RIGHT-WRITER-LITERAL"),
        ] {
            let rows = snapshot.rows(&format!("{prefix}-{index}"));
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].credential.expose_api_key(), Some(token));
            assert_eq!(rows[0].rev, 1);
            assert!(
                ids.insert(rows[0].id),
                "ids are globally unique, not provider-local"
            );
        }
    }
    assert!(!path.with_extension("json.tmp").exists());
    // This is the actual flock artifact, not a test double's synchronization.
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("json.lock"))
        .unwrap();
    // SAFETY: lock owns this valid descriptor until after unlock.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
}

#[tokio::test]
async fn oauth_upsert_matches_first_present_identity_and_never_matches_none() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileAuthStore::at(tmp.path().join("auth.json"));
    assert_eq!(
        identity(Some("a"), Some("e"), Some("p")).key().as_deref(),
        Some("account:a")
    );
    assert_eq!(
        identity(None, Some("e"), Some("p")).key().as_deref(),
        Some("email:e")
    );
    assert_eq!(
        identity(None, None, Some("p")).key().as_deref(),
        Some("project:p")
    );
    assert_eq!(CredentialIdentity::default().key(), None);
    let first = store
        .upsert(
            "identity",
            oauth("IDENTITY-FIRST", FRESH, Some("same")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let same = store
        .upsert(
            "identity",
            oauth("IDENTITY-REPLACED", FRESH, Some("same")),
            CredentialOrigin::Stored,
            true,
        )
        .await
        .unwrap();
    let different = store
        .upsert(
            "identity",
            oauth("IDENTITY-DIFFERENT", FRESH, Some("other")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let none_a = store
        .upsert(
            "identity",
            oauth("IDENTITY-NONE-A", FRESH, None),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let none_b = store
        .upsert(
            "identity",
            oauth("IDENTITY-NONE-B", FRESH, None),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    assert_eq!(same, first);
    assert_ne!(different, first);
    assert_ne!(none_a, none_b);
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("identity");
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [first, different, none_a, none_b]
    );
    assert_eq!(rows[0].rev, 2);
    assert_eq!(rows[0].origin, CredentialOrigin::Stored);
    assert!(rows[0].last_validated.is_some());
    let Credential::OAuth(saved) = &rows[0].credential else {
        panic!("expected OAuth")
    };
    assert_eq!(saved.access.expose_secret(), "IDENTITY-REPLACED");

    // Email and project are real upsert keys, not only formatting helpers.
    for (provider, ident) in [
        (
            "email",
            identity(None, Some("same@email"), Some("ignored-project")),
        ),
        ("project", identity(None, None, Some("same-project"))),
    ] {
        let credential = |access: &str| {
            Credential::OAuth(OAuthCredential {
                access: access.to_owned().into(),
                refresh: "IDENTITY-REFRESH".into(),
                expires_at_ms: FRESH,
                identity: ident.clone(),
            })
        };
        let id = store
            .upsert(
                provider,
                credential("IDENTITY-BEFORE"),
                CredentialOrigin::Login,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .upsert(
                    provider,
                    credential("IDENTITY-AFTER"),
                    CredentialOrigin::Login,
                    false
                )
                .await
                .unwrap(),
            id
        );
        let snapshot = store.snapshot().await.unwrap();
        assert_eq!(snapshot.rows(provider).len(), 1);
        assert_eq!(snapshot.rows(provider)[0].rev, 2);
    }
    let account_email = |account: &str| {
        Credential::OAuth(OAuthCredential {
            access: "PRIORITY-ACCESS".into(),
            refresh: "PRIORITY-REFRESH".into(),
            expires_at_ms: FRESH,
            identity: identity(Some(account), Some("shared@email"), None),
        })
    };
    let a = store
        .upsert(
            "priority",
            account_email("a"),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let b = store
        .upsert(
            "priority",
            account_email("b"),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    assert_ne!(a, b, "different accounts sharing email must not collapse");
}

#[tokio::test]
async fn api_key_upsert_replaces_enabled_row_across_origins_and_purges_tombstones() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    fixture(
        &path,
        "key",
        vec![
            key_row(1, "KEY-TOMBSTONE", "login", Some("old")),
            key_row(2, "KEY-ENABLED", "stored", None),
            oauth_row(3, "OAUTH-UNRELATED", FRESH, Some("keep oauth")),
        ],
    );
    let store = FileAuthStore::at(&path);
    let id = store
        .upsert(
            "key",
            api("KEY-LOGIN-REPLACEMENT"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    assert_eq!(id, CredentialId(2));
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("key");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, CredentialId(2));
    assert_eq!(rows[0].rev, 2);
    assert_eq!(rows[0].origin, CredentialOrigin::Login);
    assert_eq!(
        rows[0].credential.expose_api_key(),
        Some("KEY-LOGIN-REPLACEMENT")
    );
    assert!(rows[0].last_validated.is_some());
    assert_eq!(rows[1].id, CredentialId(3));
    assert_eq!(
        store
            .upsert(
                "key",
                api("KEY-STORED-REPLACEMENT"),
                CredentialOrigin::Stored,
                false
            )
            .await
            .unwrap(),
        id
    );
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("key")[0].rev, 3);
    assert_eq!(snapshot.rows("key")[0].origin, CredentialOrigin::Stored);
    assert!(snapshot.rows("key")[0].last_validated.is_none());
    store
        .disable_if_matches("key", id, 3, "disabled")
        .await
        .unwrap();
    let new_id = store
        .upsert(
            "key",
            api("KEY-AFTER-TOMBSTONE"),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    assert_ne!(new_id, id);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("key").len(), 2);
    assert!(!snapshot.rows("key").iter().any(|row| row.id == id));
    assert_eq!(
        snapshot
            .rows("key")
            .iter()
            .filter(|row| matches!(row.credential, Credential::ApiKey(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn oauth_upsert_purges_only_matching_nonnull_identity_tombstones() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileAuthStore::at(tmp.path().join("auth.json"));
    let same = store
        .upsert(
            "purge",
            oauth("PURGE-SAME", FRESH, Some("same")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let other = store
        .upsert(
            "purge",
            oauth("PURGE-OTHER", FRESH, Some("other")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let anonymous = store
        .upsert(
            "purge",
            oauth("PURGE-NONE", FRESH, None),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    for id in [same, other, anonymous] {
        assert_eq!(
            store
                .disable_if_matches("purge", id, 1, "disabled")
                .await
                .unwrap(),
            CasOutcome::Applied
        );
    }
    let replacement = store
        .upsert(
            "purge",
            oauth("PURGE-REPLACEMENT", FRESH, Some("same")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    assert_ne!(same, replacement);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("purge").len(), 3);
    assert!(!snapshot.rows("purge").iter().any(|row| row.id == same));
    assert!(
        snapshot
            .rows("purge")
            .iter()
            .any(|row| row.id == other && row.disabled_cause.is_some())
    );
    assert!(
        snapshot
            .rows("purge")
            .iter()
            .any(|row| row.id == anonymous && row.disabled_cause.is_some())
    );
    store
        .upsert(
            "purge",
            oauth("PURGE-NEW-NONE", FRESH, None),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().rows("purge").len(),
        4,
        "null identities must not purge one another"
    );
}

fn disk_state(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (
        std::fs::read(path).unwrap(),
        std::fs::metadata(path).unwrap().modified().unwrap(),
    )
}

#[tokio::test]
async fn cas_stale_is_nonwriting_and_applied_increments_rev_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = FileAuthStore::at(&path);
    let id = store
        .upsert("cas", api("CAS-ORIGINAL"), CredentialOrigin::Stored, false)
        .await
        .unwrap();
    let before = disk_state(&path);
    for (provider, candidate, revision) in [
        ("wrong-provider", id, 1),
        ("cas", CredentialId(99), 1),
        ("cas", id, 0),
        ("cas", id, 2),
    ] {
        assert_eq!(
            store
                .replace_if_matches(provider, candidate, revision, api("CAS-STALE-LOSER"))
                .await
                .unwrap(),
            CasOutcome::Stale
        );
        assert_eq!(disk_state(&path), before);
        assert_eq!(
            store
                .disable_if_matches(provider, candidate, revision, "wrong")
                .await
                .unwrap(),
            CasOutcome::Stale
        );
        assert_eq!(disk_state(&path), before);
    }
    assert_eq!(
        store
            .replace_if_matches("cas", id, 1, api("CAS-REPLACED"))
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("cas")[0].id, id);
    assert_eq!(snapshot.rows("cas")[0].rev, 2);
    assert_eq!(
        snapshot.rows("cas")[0].credential.expose_api_key(),
        Some("CAS-REPLACED")
    );
    assert_eq!(
        store
            .disable_if_matches("cas", id, 2, "operator disabled")
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    let disabled = disk_state(&path);
    assert_eq!(
        store
            .replace_if_matches("cas", id, 3, api("CANNOT-RESURRECT"))
            .await
            .unwrap(),
        CasOutcome::Stale
    );
    assert_eq!(
        store
            .disable_if_matches("cas", id, 3, "cannot overwrite")
            .await
            .unwrap(),
        CasOutcome::Stale
    );
    assert_eq!(disk_state(&path), disabled);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("cas")[0].rev, 3);
    assert_eq!(
        snapshot.rows("cas")[0].disabled_cause.as_deref(),
        Some("operator disabled")
    );
}

#[tokio::test]
async fn remove_absent_skips_write_and_remove_provider_deletes_all_its_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = FileAuthStore::at(&path);
    store.remove("missing").await.unwrap();
    assert!(!path.exists());
    let id = store
        .upsert(
            "remove",
            oauth("REMOVE-FIRST", FRESH, Some("first")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    store
        .upsert(
            "remove",
            oauth("REMOVE-SECOND", FRESH, Some("second")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    store
        .disable_if_matches("remove", id, 1, "disabled")
        .await
        .unwrap();
    store
        .upsert(
            "keep",
            api("REMOVE-KEEP-LITERAL"),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    let before = disk_state(&path);
    store.remove("missing").await.unwrap();
    assert_eq!(disk_state(&path), before);
    store.remove("remove").await.unwrap();
    let snapshot = store.snapshot().await.unwrap();
    assert!(snapshot.rows("remove").is_empty());
    assert_eq!(
        snapshot.rows("keep")[0].credential.expose_api_key(),
        Some("REMOVE-KEEP-LITERAL")
    );
    let after = disk_state(&path);
    store.remove("remove").await.unwrap();
    assert_eq!(disk_state(&path), after);
}

struct NeverRefresh {
    entered: AtomicUsize,
}

#[async_trait]
impl OAuthRefreshPort for NeverRefresh {
    async fn refresh(
        &self,
        _: &str,
        _: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn refresh_ten_second_timeout_is_transient_and_keeps_row() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
    store
        .upsert(
            "timeout",
            oauth("TIMEOUT-OLD-LITERAL", NOW, Some("timeout")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(NeverRefresh {
        entered: AtomicUsize::new(0),
    });
    let resolver = Arc::new(resolver(store.clone(), port.clone()));
    let task = tokio::spawn(async move { resolver.resolve("timeout", None, &[], &no_env).await });
    while port.entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(9_999)).await;
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "deadline is ten seconds, not an earlier timeout"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(
        matches!(task.await.unwrap(), Err(ResolveError::RefreshUnavailable { provider, .. }) if provider == "timeout")
    );
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("timeout")[0];
    assert_eq!(row.rev, 1);
    assert!(row.disabled_cause.is_none());
    let Credential::OAuth(saved) = &row.credential else {
        panic!("expected OAuth")
    };
    assert_eq!(saved.access.expose_secret(), "TIMEOUT-OLD-LITERAL");
    assert_eq!(saved.refresh.expose_secret(), "STORED-REFRESH-CANARY");
    assert_eq!(port.entered.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_loop_row_gone_after_waiting_for_lock_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    store
        .upsert(
            "gone",
            oauth("GONE-OLD-LITERAL", NOW, Some("gone")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(GatedRefresh::new(Err(RefreshFailure::Transient(
        "first flight offline".into(),
    ))));
    let resolver = Arc::new(resolver(store.clone(), port.clone()));
    let first_resolver = resolver.clone();
    let first =
        tokio::spawn(async move { first_resolver.resolve("gone", None, &[], &no_env).await });
    while port.entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let arrived = Arc::new(AtomicUsize::new(0));
    let arrived_task = arrived.clone();
    let second = tokio::spawn(async move {
        arrived_task.fetch_add(1, Ordering::SeqCst);
        resolver.resolve("gone", None, &[], &no_env).await
    });
    while arrived.load(Ordering::SeqCst) != 1 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    FileAuthStore::at(&path).remove("gone").await.unwrap();
    port.gate.add_permits(2);
    assert!(matches!(
        first.await.unwrap(),
        Err(ResolveError::RefreshUnavailable { .. })
    ));
    assert!(second.await.unwrap().unwrap().is_none());
    assert_eq!(
        port.entered.load(Ordering::SeqCst),
        1,
        "the waiter must re-read before calling the port"
    );
    assert!(store.snapshot().await.unwrap().rows("gone").is_empty());
}

struct StalePeerRefresh {
    gate: Semaphore,
    calls: AtomicUsize,
    seen_refresh: Mutex<Vec<String>>,
}

#[async_trait]
impl OAuthRefreshPort for StalePeerRefresh {
    async fn refresh(
        &self,
        _: &str,
        credential: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        self.seen_refresh
            .lock()
            .await
            .push(credential.refresh.expose_secret().to_owned());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.gate.acquire().await.unwrap().forget();
            Err(RefreshFailure::Transient("old revision failed".into()))
        } else {
            Ok(refreshed())
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_loop_waiter_refreshes_peer_still_stale_revision_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "stale-peer",
            oauth("STALE-PEER-OLD", NOW, Some("stale-peer")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(StalePeerRefresh {
        gate: Semaphore::new(0),
        calls: AtomicUsize::new(0),
        seen_refresh: Mutex::new(Vec::new()),
    });
    let resolver = Arc::new(resolver(store.clone(), port.clone()));
    let first_resolver = resolver.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve("stale-peer", None, &[], &no_env)
            .await
    });
    while port.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let arrived = Arc::new(AtomicUsize::new(0));
    let task_arrived = arrived.clone();
    let second = tokio::spawn(async move {
        task_arrived.fetch_add(1, Ordering::SeqCst);
        resolver.resolve("stale-peer", None, &[], &no_env).await
    });
    while arrived.load(Ordering::SeqCst) != 1 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    let peer_credential = Credential::OAuth(OAuthCredential {
        access: "STALE-PEER-UPDATED".into(),
        refresh: "STALE-PEER-UPDATED-REFRESH".into(),
        expires_at_ms: NOW,
        identity: identity(Some("stale-peer"), None, None),
    });
    assert_eq!(
        FileAuthStore::at(&path)
            .replace_if_matches("stale-peer", id, 1, peer_credential)
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    port.gate.add_permits(1);
    assert!(matches!(
        first.await.unwrap(),
        Err(ResolveError::RefreshUnavailable { .. })
    ));
    assert_winner(
        second.await.unwrap().unwrap(),
        AuthSource::StoredOAuth,
        "REFRESHED-ACCESS-LITERAL",
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *port.seen_refresh.lock().await,
        ["STORED-REFRESH-CANARY", "STALE-PEER-UPDATED-REFRESH"]
    );
    assert_eq!(store.snapshot().await.unwrap().rows("stale-peer")[0].rev, 3);
}

#[tokio::test]
async fn refresh_loop_attempts_each_stale_row_at_most_once_then_uses_next_stage() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
    store
        .upsert(
            "loop",
            oauth("LOOP-FIRST", NOW, Some("first")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    store
        .upsert(
            "loop",
            oauth("LOOP-SECOND", NOW, Some("second")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    store
        .upsert(
            "loop",
            api("LOOP-STORED-FALLBACK"),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(Probe::new(Err(RefreshFailure::Transient("offline".into()))));
    let resolver = resolver(store.clone(), port.clone());
    assert_winner(
        resolver.resolve("loop", None, &[], &no_env).await.unwrap(),
        AuthSource::StoredKey,
        "LOOP-STORED-FALLBACK",
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 2);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("loop").len(), 3);
    for row in snapshot.rows("loop") {
        assert_eq!(row.rev, 1);
        assert!(row.disabled_cause.is_none());
    }
}

#[tokio::test]
async fn refresh_success_losing_cas_adopts_fresh_peer_instead_of_overwriting() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "peer",
            oauth("CAS-REFRESH-OLD", NOW, Some("peer")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(PeerRotation {
        peer: FileAuthStore::at(&path),
        id,
        rev: 1,
        calls: AtomicUsize::new(0),
        outcome: Ok(refreshed()),
        peer_expiry: FRESH,
    });
    let resolver = resolver(store.clone(), port);
    assert_winner(
        resolver.resolve("peer", None, &[], &no_env).await.unwrap(),
        AuthSource::StoredOAuth,
        "PEER-ROTATED-LITERAL",
    );
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("peer")[0].rev, 2);
    let Credential::OAuth(saved) = &snapshot.rows("peer")[0].credential else {
        panic!("expected OAuth")
    };
    assert_eq!(saved.access.expose_secret(), "PEER-ROTATED-LITERAL");
}

#[tokio::test]
async fn refresh_success_losing_cas_to_stale_peer_is_retryable_not_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "peer",
            oauth("CAS-STALE-OLD", NOW, Some("peer")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(PeerRotation {
        peer: FileAuthStore::at(&path),
        id,
        rev: 1,
        calls: AtomicUsize::new(0),
        outcome: Ok(refreshed()),
        peer_expiry: NOW,
    });
    let resolver = resolver(store.clone(), port.clone());
    assert!(
        matches!(resolver.resolve("peer", None, &[], &no_env).await, Err(ResolveError::RefreshUnavailable { provider, cause }) if provider == "peer" && cause.contains("concurrent update"))
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("peer")[0].rev, 2);
    assert!(snapshot.rows("peer")[0].disabled_cause.is_none());
    let Credential::OAuth(saved) = &snapshot.rows("peer")[0].credential else {
        panic!("expected OAuth")
    };
    assert_eq!(saved.access.expose_secret(), "PEER-ROTATED-LITERAL");
}

#[tokio::test]
#[serial]
async fn sync_construction_skips_stale_rows_individually_before_env_and_stored_key() {
    let env = EnvRestore::capture(&["RUSTAIN_DATA_DIR", "STORY20_AUTH_KEY"]);
    for (case, expected) in [
        ("fresh-second", Some("SYNC-FRESH-SECOND")),
        ("env", Some("SYNC-ENV-FALLBACK")),
        ("stored", Some("SYNC-STORED-FALLBACK")),
        ("stale-only", None),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        env.set("RUSTAIN_DATA_DIR", tmp.path());
        env.remove("STORY20_AUTH_KEY");
        let path = tmp.path().join("auth.json");
        let mut rows = vec![oauth_row(1, "SYNC-STALE-FIRST", 0, None)];
        if case == "fresh-second" {
            rows.push(oauth_row(2, "SYNC-FRESH-SECOND", i64::MAX, None));
            rows.push(oauth_row(4, "SYNC-FRESH-THIRD-LOSER", i64::MAX, None));
        }
        if case == "stored" {
            rows.push(key_row(3, "SYNC-STORED-FALLBACK", "stored", None));
        }
        if case == "env" {
            env.set("STORY20_AUTH_KEY", "  SYNC-ENV-FALLBACK  ");
        }
        fixture(&path, "sync", rows);
        let store = FileAuthStore::at(&path);
        let before = disk_state(&path);
        assert_eq!(
            resolve_api_key("STORY20_AUTH_KEY", "sync").as_deref(),
            expected,
            "fixture {case}"
        );
        assert_eq!(
            disk_state(&path),
            before,
            "construction must never refresh or write"
        );
        assert_eq!(store.snapshot().await.unwrap().rows("sync")[0].rev, 1);
    }
}

#[tokio::test]
#[serial]
async fn w1_empty_data_dir_uses_default_home_not_current_directory() {
    let home = tempfile::tempdir().unwrap();
    let env = EnvRestore::capture(&["RUSTAIN_DATA_DIR", "HOME"]);
    env.set("RUSTAIN_DATA_DIR", "");
    env.set("HOME", home.path());
    let cwd_file = std::env::current_dir().unwrap().join("auth.json");
    let cwd_before = std::fs::read(&cwd_file).ok();
    assert_eq!(
        rustain::infrastructure::paths::data_dir().unwrap(),
        home.path().join(".rustain")
    );
    // Whitespace is as empty as "", for this process and its daemon child alike.
    env.set("RUSTAIN_DATA_DIR", "   ");
    assert_eq!(
        rustain::infrastructure::paths::data_dir().unwrap(),
        home.path().join(".rustain")
    );
    env.set("RUSTAIN_DATA_DIR", "");
    let store = FileAuthStore::new();
    store
        .upsert(
            "empty-dir",
            api("EMPTY-DIR-LITERAL"),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    let default_path = home.path().join(".rustain/auth.json");
    assert!(default_path.is_file());
    assert_eq!(std::fs::read(&cwd_file).ok(), cwd_before);
    let explicit = FileAuthStore::at(default_path);
    assert_eq!(
        explicit.snapshot().await.unwrap().rows("empty-dir")[0]
            .credential
            .expose_api_key(),
        Some("EMPTY-DIR-LITERAL")
    );
}

#[tokio::test]
async fn oauth_upsert_replaces_first_enabled_match_in_file_order() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let mut rows = vec![
        oauth_row(1, "FIRST-MATCH-TOMBSTONE", FRESH, Some("disabled")),
        oauth_row(2, "FIRST-ENABLED-MATCH", FRESH, None),
        oauth_row(3, "SECOND-ENABLED-MATCH", FRESH, None),
        oauth_row(4, "UNRELATED-TOMBSTONE", FRESH, Some("unrelated")),
    ];
    for row in &mut rows[..3] {
        row["identity"] = json!({"account_id": "same"});
    }
    fixture(&path, "first-match", rows);
    let store = FileAuthStore::at(path);
    assert_eq!(
        store
            .upsert(
                "first-match",
                oauth("FIRST-MATCH-REPLACEMENT", FRESH, Some("same")),
                CredentialOrigin::Stored,
                false
            )
            .await
            .unwrap(),
        CredentialId(2)
    );
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("first-match");
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [CredentialId(2), CredentialId(3), CredentialId(4)]
    );
    assert_eq!(rows[0].rev, 2);
    assert_eq!(rows[0].origin, CredentialOrigin::Stored);
    let Credential::OAuth(first) = &rows[0].credential else {
        panic!("expected OAuth")
    };
    let Credential::OAuth(second) = &rows[1].credential else {
        panic!("expected OAuth")
    };
    assert_eq!(first.access.expose_secret(), "FIRST-MATCH-REPLACEMENT");
    assert_eq!(second.access.expose_secret(), "SECOND-ENABLED-MATCH");
    assert_eq!(rows[1].rev, 1);
    assert_eq!(rows[2].disabled_cause.as_deref(), Some("unrelated"));
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_loop_row_disabled_while_waiting_for_lock_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "disabled-waiter",
            oauth("DISABLED-WAITER-OLD", NOW, Some("disabled-waiter")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(GatedRefresh::new(Err(RefreshFailure::Transient(
        "offline".into(),
    ))));
    let resolver = Arc::new(resolver(store.clone(), port.clone()));
    let first_resolver = resolver.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve("disabled-waiter", None, &[], &no_env)
            .await
    });
    while port.entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let arrived = Arc::new(AtomicUsize::new(0));
    let task_arrived = arrived.clone();
    let second = tokio::spawn(async move {
        task_arrived.fetch_add(1, Ordering::SeqCst);
        resolver
            .resolve("disabled-waiter", None, &[], &no_env)
            .await
    });
    while arrived.load(Ordering::SeqCst) != 1 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    assert_eq!(
        FileAuthStore::at(path)
            .disable_if_matches("disabled-waiter", id, 1, "peer disabled")
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    port.gate.add_permits(2);
    assert!(matches!(
        first.await.unwrap(),
        Err(ResolveError::RefreshUnavailable { .. })
    ));
    assert!(second.await.unwrap().unwrap().is_none());
    assert_eq!(port.entered.load(Ordering::SeqCst), 1);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("disabled-waiter")[0].rev, 2);
    assert_eq!(
        snapshot.rows("disabled-waiter")[0]
            .disabled_cause
            .as_deref(),
        Some("peer disabled")
    );
}

#[tokio::test]
async fn definitive_failure_does_not_disable_peer_still_stale_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "peer",
            oauth("DEFINITIVE-STALE-OLD", NOW, Some("peer")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(PeerRotation {
        peer: FileAuthStore::at(&path),
        id,
        rev: 1,
        calls: AtomicUsize::new(0),
        outcome: Err(RefreshFailure::Definitive("invalid_grant".into())),
        peer_expiry: NOW,
    });
    let resolver = resolver(store.clone(), port.clone());
    let lookup = |_: &str| Some("PEER-STALE-FALLBACK".into());
    assert_winner(
        resolver
            .resolve("peer", None, &["FALLBACK"], &lookup)
            .await
            .unwrap(),
        AuthSource::Env {
            var: "FALLBACK".into(),
        },
        "PEER-STALE-FALLBACK",
    );
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("peer")[0].rev, 2);
    assert!(snapshot.rows("peer")[0].disabled_cause.is_none());
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

/// ADR-19-1-03 no-daemon election: while another store on the same file holds
/// the refresh lease, this process must not spend the refresh token; once the
/// holder's rotation lands and the lease drops, the rotation is adopted.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn refresh_lease_held_by_peer_store_defers_refresh_and_adopts_its_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    let store = Arc::new(FileAuthStore::at(&path));
    let id = store
        .upsert(
            "lease",
            oauth("LEASE-OLD-LITERAL", NOW, Some("lease")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let peer = FileAuthStore::at(&path);
    let lease = peer.refresh_lease().await.unwrap();
    let port = Arc::new(Probe::new(Ok(refreshed())));
    let resolver = resolver(store.clone(), port.clone());
    let task = tokio::spawn(async move { resolver.resolve("lease", None, &[], &no_env).await });
    // One scheduler turn: without an election the task refreshes to completion here.
    tokio::task::yield_now().await;
    assert_eq!(
        port.calls.load(Ordering::SeqCst),
        0,
        "refreshed while a peer process held the refresh lease"
    );
    assert_eq!(
        peer.replace_if_matches(
            "lease",
            id,
            1,
            oauth("LEASE-PEER-LITERAL", FRESH, Some("lease"))
        )
        .await
        .unwrap(),
        CasOutcome::Applied
    );
    drop(lease);
    assert_winner(
        task.await.unwrap().unwrap(),
        AuthSource::StoredOAuth,
        "LEASE-PEER-LITERAL",
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.snapshot().await.unwrap().rows("lease")[0].rev, 2);
}

/// Callers queued behind a transient exchange share its failure: one exchange,
/// not one bounded attempt per waiter in turn. A later call is a new flight.
#[tokio::test(flavor = "current_thread")]
async fn queued_callers_share_a_transient_exchange_instead_of_repeating_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FileAuthStore::at(tmp.path().join("auth.json")));
    store
        .upsert(
            "shared",
            oauth("SHARED-OLD-LITERAL", NOW, Some("shared")),
            CredentialOrigin::Login,
            false,
        )
        .await
        .unwrap();
    let port = Arc::new(GatedRefresh::new(Err(RefreshFailure::Transient(
        "offline".into(),
    ))));
    let resolver = Arc::new(resolver(store.clone(), port.clone()));
    let arrived = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let resolver = resolver.clone();
        let arrived = arrived.clone();
        tasks.push(tokio::spawn(async move {
            arrived.fetch_add(1, Ordering::SeqCst);
            resolver.resolve("shared", None, &[], &no_env).await
        }));
    }
    while arrived.load(Ordering::SeqCst) != 4 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    assert_eq!(port.entered.load(Ordering::SeqCst), 1);
    port.gate.add_permits(5);
    for task in tasks {
        assert!(matches!(
            task.await.unwrap(),
            Err(ResolveError::RefreshUnavailable { provider, cause })
                if provider == "shared" && cause == "offline"
        ));
    }
    assert_eq!(port.entered.load(Ordering::SeqCst), 1);
    assert!(matches!(
        resolver.resolve("shared", None, &[], &no_env).await,
        Err(ResolveError::RefreshUnavailable { .. })
    ));
    assert_eq!(port.entered.load(Ordering::SeqCst), 2);
}

/// A v2 counter that is missing or behind the file never reissues an id,
/// including an id held by an unknown-type row.
#[tokio::test]
async fn next_id_never_reissues_an_id_already_in_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    for (counter, expected) in [(None, 10), (Some(0), 10), (Some(3), 10), (Some(12), 12)] {
        let mut body = json!({
            "version": 2,
            "providers": {
                "known": [key_row(5, "KNOWN-ID-FIVE", "stored", None)],
                "future": [{"type": "future_auth", "id": 9}]
            }
        });
        if let Some(counter) = counter {
            body["next_id"] = json!(counter);
        }
        std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        let id = FileAuthStore::at(&path)
            .upsert(
                "fresh",
                api("FRESH-ID-LITERAL"),
                CredentialOrigin::Stored,
                false,
            )
            .await
            .unwrap();
        assert_eq!(id, CredentialId(expected), "next_id {counter:?}");
    }
}

/// v1 kept `last_validated` opaque: an unparseable stamp drops only the stamp.
#[tokio::test]
async fn v1_unparseable_last_validated_keeps_the_key() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    std::fs::write(
        &path,
        br#"{"version":1,"providers":{"legacy":{"type":"api_key","api_key":"LEGACY-STAMP-LITERAL","last_validated":"2026-06-15"}}}"#,
    )
    .unwrap();
    let snapshot = FileAuthStore::at(&path).snapshot().await.unwrap();
    let row = &snapshot.rows("legacy")[0];
    assert_eq!(
        row.credential.expose_api_key(),
        Some("LEGACY-STAMP-LITERAL")
    );
    assert!(row.last_validated.is_none());
}

/// Parse errors carry a position, never the offending value (it may be a key),
/// and a future version is judged before its body shape.
#[tokio::test]
async fn parse_errors_omit_values_and_future_versions_are_judged_first() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth.json");
    for body in [
        r#"{"version":2,"next_id":1,"providers":{"anthropic":"sk-PARSE-LEAK-CANARY"}}"#,
        r#"{"version":1,"providers":{"anthropic":"sk-PARSE-LEAK-CANARY"}}"#,
    ] {
        std::fs::write(&path, body).unwrap();
        match FileAuthStore::at(&path).snapshot().await {
            Err(AuthError::Parse(message)) => assert!(
                !message.contains("PARSE-LEAK-CANARY"),
                "parse error echoed the stored value"
            ),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }
    let future = br#"{"version":3,"accounts":[]}"#;
    std::fs::write(&path, future).unwrap();
    let store = FileAuthStore::at(&path);
    assert!(matches!(
        store.snapshot().await,
        Err(AuthError::UnsupportedVersion(3))
    ));
    assert!(matches!(
        store
            .upsert("new", api("DO-NOT-WRITE"), CredentialOrigin::Stored, false)
            .await,
        Err(AuthError::UnsupportedVersion(3))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), future);
}
