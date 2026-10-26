//! Lazy OAuth refresh: one flight per row within a resolver instance, one
//! refresher per store across processes (`AuthStorePort::refresh_lease`),
//! revision-fenced writes.
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::domain::errors::{AuthError, ProviderError};
use crate::domain::models::{
    AuthSource, CasOutcome, Credential, CredentialId, CredentialIdentity, OAuthCredential,
    ResolvedAuth, SecretString, StoredCredential,
};
use crate::domain::ports::{AuthStorePort, OAuthRefreshPort, RefreshFailure};
use crate::domain::services::key_cascade::{self, CascadeInputs, SelectionMaterial};

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    // Store failures fall back at selection or become transient during refresh.
    #[error("OAuth refresh unavailable for {provider}: {cause}")]
    RefreshUnavailable { provider: String, cause: String },
}

/// One refresh exchange (omp `refresh.ts:43-44`).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// A peer process holds the lease for one bounded exchange plus its write.
const LEASE_WAIT: Duration = Duration::from_secs(15);

pub struct KeyResolver {
    store: Arc<dyn AuthStorePort>,
    refresh: Arc<dyn OAuthRefreshPort>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    flights: Mutex<HashMap<CredentialId, Arc<RowFlight>>>,
}

/// Per-row flight. Callers queued behind an exchange share its transient
/// failure instead of repeating the exchange one after another.
#[derive(Default)]
struct RowFlight {
    /// Finished attempts; compared before and after queueing.
    finished: AtomicU64,
    /// The row lock. Holds `(rev, cause)` when the latest attempt was transient;
    /// `rev` is `None` when the lease wait itself timed out (any revision).
    last_transient: Mutex<Option<(Option<u64>, String)>>,
}

enum Attempt {
    Ready(ResolvedAuth),
    Skip,
    Transient(String),
}

impl KeyResolver {
    pub fn new(
        store: Arc<dyn AuthStorePort>,
        refresh: Arc<dyn OAuthRefreshPort>,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            store,
            refresh,
            clock,
            flights: Mutex::new(HashMap::new()),
        }
    }

    pub async fn resolve(
        &self,
        provider: &str,
        runtime: Option<&SecretString>,
        env_vars: &[&str],
        env_lookup: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> Result<Option<(AuthSource, ResolvedAuth)>, ResolveError> {
        self.resolve_with_policy(provider, runtime, env_vars, env_lookup, true)
            .await
    }

    pub async fn resolve_with_policy(
        &self,
        provider: &str,
        runtime: Option<&SecretString>,
        env_vars: &[&str],
        env_lookup: &(dyn Fn(&str) -> Option<String> + Send + Sync),
        oauth_allowed: bool,
    ) -> Result<Option<(AuthSource, ResolvedAuth)>, ResolveError> {
        let mut skip = BTreeSet::new();
        let mut transient = None;
        loop {
            // Runtime candidates do not depend on the disk store being readable.
            let snapshot = if runtime.is_some() {
                Default::default()
            } else {
                match self.store.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        tracing::warn!(
                            "auth.json unreadable; ignoring stored credentials: {error}"
                        );
                        Default::default()
                    }
                }
            };
            let selected = {
                let inputs = CascadeInputs {
                    runtime,
                    rows: snapshot.rows(provider),
                    env_vars,
                    env_lookup,
                    now_ms: (self.clock)(),
                    oauth_allowed,
                };
                key_cascade::select(&inputs, &skip).map(|s| (s.source, s.material))
            };
            match selected {
                Some((source, SelectionMaterial::Ready(auth))) => return Ok(Some((source, auth))),
                Some((_, SelectionMaterial::NeedsRefresh { id, .. })) => {
                    // Row-scoped, including CAS loss and transient errors. This bounds
                    // each call even when the disk changes while refreshing.
                    skip.insert(id);
                    let attempt = match self.refresh_row(provider, id).await {
                        Ok(attempt) => attempt,
                        Err(ResolveError::RefreshUnavailable { cause, .. }) => {
                            Attempt::Transient(cause)
                        }
                    };
                    match attempt {
                        Attempt::Ready(auth) => return Ok(Some((AuthSource::StoredOAuth, auth))),
                        Attempt::Skip => {}
                        Attempt::Transient(cause) => transient = Some(cause),
                    }
                }
                None => {
                    return match transient {
                        Some(cause) => Err(ResolveError::RefreshUnavailable {
                            provider: provider.into(),
                            cause,
                        }),
                        None => Ok(None),
                    };
                }
            }
        }
    }

    async fn refresh_row(&self, provider: &str, id: CredentialId) -> Result<Attempt, ResolveError> {
        let flight = self.flights.lock().await.entry(id).or_default().clone();
        let attempt = self.fly(provider, id, &flight).await;
        // Forget the row once no other caller holds or awaits its flight.
        let mut flights = self.flights.lock().await;
        if flights.get(&id).is_some_and(|f| Arc::ptr_eq(f, &flight))
            && Arc::strong_count(&flight) == 2
        {
            flights.remove(&id);
        }
        attempt
    }

    async fn fly(
        &self,
        provider: &str,
        id: CredentialId,
        flight: &RowFlight,
    ) -> Result<Attempt, ResolveError> {
        let queued_at = flight.finished.load(Ordering::SeqCst);
        let mut last_transient = flight.last_transient.lock().await;
        let joined = flight.finished.load(Ordering::SeqCst) != queued_at;
        // The attempt this caller queued behind found the lease held past its bound.
        if let Some((None, cause)) = last_transient.as_ref().filter(|_| joined) {
            return Ok(Attempt::Transient(cause.clone()));
        }
        // Elect before re-reading, so a row a peer process refreshed meanwhile is
        // adopted below rather than its refresh token being spent a second time.
        let _lease = match tokio::time::timeout(LEASE_WAIT, self.store.refresh_lease()).await {
            Ok(Ok(lease)) => lease,
            Ok(Err(_)) => return Ok(Attempt::Transient("refresh lease unavailable".into())),
            Err(_) => {
                let cause = String::from("refresh lease busy");
                *last_transient = Some((None, cause.clone()));
                flight.finished.fetch_add(1, Ordering::SeqCst);
                return Ok(Attempt::Transient(cause));
            }
        };
        let snapshot = match self.store.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(Attempt::Transient("refresh store unavailable".into())),
        };
        let Some(row) = snapshot
            .rows(provider)
            .iter()
            .find(|row| row.id == id && row.disabled_cause.is_none())
        else {
            return Ok(Attempt::Skip);
        };
        let Credential::OAuth(oauth) = &row.credential else {
            return Ok(Attempt::Skip);
        };
        if let Some(auth) = self.fresh(row) {
            return Ok(Attempt::Ready(auth));
        }
        // The exchange this caller queued behind failed transiently at this rev.
        if let Some((_, cause)) = last_transient
            .as_ref()
            .filter(|(rev, _)| joined && *rev == Some(row.rev))
        {
            return Ok(Attempt::Transient(cause.clone()));
        }
        // A stale new revision is refreshed with its current refresh token.
        let attempt = self.exchange(provider, id, row, oauth).await;
        *last_transient = match &attempt {
            Ok(Attempt::Transient(cause)) => Some((Some(row.rev), cause.clone())),
            _ => None,
        };
        flight.finished.fetch_add(1, Ordering::SeqCst);
        attempt
    }

    async fn exchange(
        &self,
        provider: &str,
        id: CredentialId,
        row: &StoredCredential,
        oauth: &OAuthCredential,
    ) -> Result<Attempt, ResolveError> {
        let result = match tokio::time::timeout(
            REFRESH_TIMEOUT,
            self.refresh.refresh(provider, oauth),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(RefreshFailure::Transient("refresh timed out".into())),
        };
        match result {
            Ok(tokens) => {
                let access = tokens.access;
                let account_id = tokens
                    .identity
                    .account_id
                    .as_ref()
                    .or(oauth.identity.account_id.as_ref())
                    .cloned();
                let credential = || {
                    Credential::OAuth(OAuthCredential {
                        access: access.clone(),
                        refresh: tokens.refresh.as_ref().unwrap_or(&oauth.refresh).clone(),
                        expires_at_ms: tokens.expires_at_ms,
                        authorized_at_ms: oauth.authorized_at_ms,
                        identity: CredentialIdentity {
                            account_id: account_id.clone(),
                            email: tokens
                                .identity
                                .email
                                .as_ref()
                                .or(oauth.identity.email.as_ref())
                                .cloned(),
                            project_id: tokens
                                .identity
                                .project_id
                                .as_ref()
                                .or(oauth.identity.project_id.as_ref())
                                .cloned(),
                            org_id: oauth.identity.org_id.clone(),
                            org_name: oauth.identity.org_name.clone(),
                        },
                    })
                };
                let mut persisted = self
                    .store
                    .replace_if_matches(provider, id, row.rev, credential())
                    .await;
                if matches!(persisted, Err(AuthError::Io(_) | AuthError::Locked(_))) {
                    persisted = self
                        .store
                        .replace_if_matches(provider, id, row.rev, credential())
                        .await;
                    if matches!(persisted, Err(AuthError::Io(_) | AuthError::Locked(_))) {
                        tracing::warn!(
                            provider,
                            row_id = id.0,
                            "rotated refresh token not persisted; re-login may be required"
                        );
                        return Ok(Attempt::Ready(ResolvedAuth::OAuth { access, account_id }));
                    }
                }
                match persisted {
                    Ok(CasOutcome::Applied) => {
                        Ok(Attempt::Ready(ResolvedAuth::OAuth { access, account_id }))
                    }
                    Ok(CasOutcome::Stale) => Ok(match self.peer_fresh(provider, id).await? {
                        Some(auth) => Attempt::Ready(auth),
                        None => Attempt::Transient("concurrent update".into()),
                    }),
                    Err(_) => Ok(Attempt::Transient("refresh write unavailable".into())),
                }
            }
            Err(RefreshFailure::Transient(cause)) => Ok(Attempt::Transient(cause)),
            Err(RefreshFailure::Definitive(cause)) => {
                let current = match self.store.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => return Ok(Attempt::Transient("refresh store unavailable".into())),
                };
                let Some(current) = current
                    .rows(provider)
                    .iter()
                    .find(|row| row.id == id && row.disabled_cause.is_none())
                else {
                    return Ok(Attempt::Skip);
                };
                if current.rev != row.rev {
                    return Ok(match self.fresh(current) {
                        Some(auth) => Attempt::Ready(auth),
                        None => Attempt::Skip,
                    });
                }
                let disabled_cause = format!("oauth refresh failed: {cause}");
                match self
                    .store
                    .disable_if_matches(provider, id, row.rev, &disabled_cause)
                    .await
                {
                    Ok(CasOutcome::Applied) => {
                        tracing::warn!(provider, row_id = id.0, cause, "OAuth credential disabled");
                    }
                    Ok(CasOutcome::Stale) => {}
                    Err(_) => return Ok(Attempt::Transient("refresh write unavailable".into())),
                }
                Ok(Attempt::Skip)
            }
        }
    }

    fn fresh(&self, row: &StoredCredential) -> Option<ResolvedAuth> {
        if row.disabled_cause.is_some() {
            return None;
        }
        match &row.credential {
            Credential::OAuth(oauth)
                if key_cascade::oauth_is_fresh(oauth.expires_at_ms, (self.clock)()) =>
            {
                Some(ResolvedAuth::OAuth {
                    access: oauth.access.clone(),
                    account_id: oauth.identity.account_id.clone(),
                })
            }
            _ => None,
        }
    }

    async fn peer_fresh(
        &self,
        provider: &str,
        id: CredentialId,
    ) -> Result<Option<ResolvedAuth>, ResolveError> {
        Ok(self
            .store
            .snapshot()
            .await
            .map_err(|_| ResolveError::RefreshUnavailable {
                provider: provider.into(),
                cause: "refresh store unavailable".into(),
            })?
            .rows(provider)
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| self.fresh(row)))
    }
}

pub fn compose(
    store: Arc<dyn AuthStorePort>,
    refresher: Arc<dyn OAuthRefreshPort>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> Arc<KeyResolver> {
    Arc::new(KeyResolver::new(store, refresher, clock))
}

/// The process composition root; the store resolves its location on every use.
pub fn shared() -> Arc<KeyResolver> {
    static RESOLVER: LazyLock<Arc<KeyResolver>> = LazyLock::new(|| {
        #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
        let refresher: Arc<dyn OAuthRefreshPort> =
            Arc::new(crate::adapters::auth::flows::refresh::HttpOAuthRefresher::new());
        #[cfg(not(any(feature = "anthropic", feature = "openai", feature = "ollama")))]
        let refresher: Arc<dyn OAuthRefreshPort> = Arc::new(NoHttpRefresher);
        compose(
            Arc::new(crate::adapters::auth_store::FileAuthStore::new()),
            refresher,
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
    });
    RESOLVER.clone()
}

#[cfg(not(any(feature = "anthropic", feature = "openai", feature = "ollama")))]
struct NoHttpRefresher;

#[cfg(not(any(feature = "anthropic", feature = "openai", feature = "ollama")))]
#[async_trait::async_trait]
impl OAuthRefreshPort for NoHttpRefresher {
    async fn refresh(
        &self,
        _: &str,
        _: &OAuthCredential,
    ) -> Result<crate::domain::ports::RefreshedTokens, RefreshFailure> {
        Err(RefreshFailure::Transient(
            "this build has no HTTP client; rebuild with default features".into(),
        ))
    }
}

/// Stateless request handle; no credential is captured when an adapter is built.
#[derive(Clone)]
pub struct CredentialHandle {
    resolver: Arc<KeyResolver>,
    provider: String,
    env_vars: Vec<String>,
    oauth_allowed: bool,
    pub auth_header: Option<crate::domain::models::AuthHeaderScheme>,
}

impl CredentialHandle {
    pub fn new(
        resolver: Arc<KeyResolver>,
        provider: String,
        env_vars: Vec<String>,
        oauth_allowed: bool,
        auth_header: Option<crate::domain::models::AuthHeaderScheme>,
    ) -> Self {
        Self {
            resolver,
            provider,
            env_vars,
            oauth_allowed,
            auth_header,
        }
    }

    pub async fn current(&self) -> Result<(AuthSource, ResolvedAuth), ProviderError> {
        // A dropped request must not drop a refresh between the token grant and its
        // write: the rotated refresh token would be lost and the row later disabled.
        // omp likewise lets the leased exchange finish and persist when its caller stops.
        tokio::spawn(self.clone().resolve())
            .await
            .map_err(|_| ProviderError::ConnectionFailed("credential resolution stopped".into()))?
    }

    async fn resolve(self) -> Result<(AuthSource, ResolvedAuth), ProviderError> {
        // Current providers use at most two names; borrow those without allocating.
        let names = [
            self.env_vars.first().map_or("", String::as_str),
            self.env_vars.get(1).map_or("", String::as_str),
        ];
        let extra;
        let names = if self.env_vars.len() <= names.len() {
            &names[..self.env_vars.len()]
        } else {
            extra = self.env_vars.iter().map(String::as_str).collect::<Vec<_>>();
            &extra
        };
        self.resolver
            .resolve_with_policy(
                &self.provider,
                None,
                names,
                &crate::infrastructure::utils::env_var_trimmed,
                self.oauth_allowed,
            )
            .await
            .map_err(|error| ProviderError::ConnectionFailed(error.to_string()))?
            .ok_or(ProviderError::AuthenticationFailed)
    }
}
