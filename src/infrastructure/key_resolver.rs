//! Lazy OAuth refresh: one flight per grant within a resolver instance, one
//! refresher per store across processes (`AuthStorePort::refresh_lease`),
//! generation- and revision-fenced writes.
//!
//! Recovery policy adapted from oh-my-pi.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::domain::errors::{AuthError, ProviderError};
use crate::domain::models::{
    AuthSource, CasOutcome, Credential, CredentialGeneration, CredentialId, CredentialIdentity,
    CredentialStamp, OAuthCredential, ResolvedAuth, SecretString, StoredCredential,
};
use crate::domain::ports::{AuthStorePort, OAuthRefreshPort, RefreshFailure};
use crate::domain::services::key_cascade::{self, CascadeInputs, SelectionMaterial};

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    // Store failures fall back at selection or become transient during refresh.
    #[error("OAuth refresh unavailable for {provider}: {cause}")]
    RefreshUnavailable { provider: String, cause: String },
}

/// Credential and provenance served to one request; the handle retains no attempt state.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub source: AuthSource,
    pub auth: ResolvedAuth,
    /// Exact grant epoch and revision, not a reusable row id.
    pub row: Option<CredentialStamp>,
    /// Account/org/project identity key of the grant that actually produced `auth`.
    pub identity: Option<String>,
}

fn oauth_identity(provider: &str, row: &StoredCredential) -> Option<String> {
    match &row.credential {
        Credential::OAuth(oauth) => {
            oauth
                .identity
                .key(crate::adapters::auth::policies::org_scoped_identity(
                    provider,
                ))
        }
        _ => None,
    }
}

fn resolved_oauth(
    provider: &str,
    row: CredentialStamp,
    access: SecretString,
    identity: &CredentialIdentity,
) -> Resolved {
    Resolved {
        source: AuthSource::StoredOAuth,
        auth: ResolvedAuth::OAuth {
            access,
            account_id: identity.account_id.clone(),
        },
        row: Some(row),
        identity: identity.key(crate::adapters::auth::policies::org_scoped_identity(
            provider,
        )),
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RowKey {
    provider: String,
    id: CredentialId,
    generation: CredentialGeneration,
}

/// One refresh exchange (omp `refresh.ts:43-44`).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// A peer process holds the lease for one bounded exchange plus its write.
const LEASE_WAIT: Duration = Duration::from_secs(15);
/// A token this process minted is not re-minted on rejection for five minutes
/// (omp `OAUTH_REMINT_COOLDOWN_MS`).
const REMINT_COOLDOWN_MS: i64 = 300_000;

pub struct KeyResolver {
    store: Arc<dyn AuthStorePort>,
    refresh: Arc<dyn OAuthRefreshPort>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    flights: Mutex<HashMap<RowKey, Arc<RowFlight>>>,
    recent_mints: Mutex<HashMap<RowKey, (u64, i64)>>,
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
    Ready(Resolved),
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
            recent_mints: Mutex::new(HashMap::new()),
        }
    }

    pub async fn resolve(
        &self,
        provider: &str,
        runtime: Option<&SecretString>,
        env_vars: &[&str],
        env_lookup: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> Result<Option<(AuthSource, ResolvedAuth)>, ResolveError> {
        Ok(self
            .resolve_tracked(provider, runtime, env_vars, env_lookup)
            .await?
            .map(|resolved| (resolved.source, resolved.auth)))
    }

    pub async fn resolve_tracked(
        &self,
        provider: &str,
        runtime: Option<&SecretString>,
        env_vars: &[&str],
        env_lookup: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> Result<Option<Resolved>, ResolveError> {
        self.resolve_tracked_with_policy(provider, runtime, env_vars, env_lookup, true)
            .await
    }

    async fn resolve_tracked_with_policy(
        &self,
        provider: &str,
        runtime: Option<&SecretString>,
        env_vars: &[&str],
        env_lookup: &(dyn Fn(&str) -> Option<String> + Send + Sync),
        oauth_allowed: bool,
    ) -> Result<Option<Resolved>, ResolveError> {
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
                key_cascade::select(&inputs, &skip).map(|s| {
                    let identity = s.row.and_then(|row| oauth_identity(provider, row));
                    (
                        s.source,
                        s.material,
                        s.row.map(StoredCredential::stamp),
                        identity,
                    )
                })
            };
            match selected {
                Some((source, SelectionMaterial::Ready(auth), row, identity)) => {
                    return Ok(Some(Resolved {
                        source,
                        auth,
                        row,
                        identity,
                    }));
                }
                Some((_, SelectionMaterial::NeedsRefresh { id, .. }, Some(row), _)) => {
                    // Row-scoped, including CAS loss and transient errors. This bounds
                    // each call even when the disk changes while refreshing.
                    skip.insert(id);
                    let attempt = match self.refresh_row(provider, row, false).await {
                        Ok(attempt) => attempt,
                        Err(ResolveError::RefreshUnavailable { cause, .. }) => {
                            Attempt::Transient(cause)
                        }
                    };
                    match attempt {
                        Attempt::Ready(resolved) => return Ok(Some(resolved)),
                        Attempt::Skip => {}
                        Attempt::Transient(cause) => transient = Some(cause),
                    }
                }
                Some((_, SelectionMaterial::NeedsRefresh { .. }, None, _)) => {
                    unreachable!("a refresh candidate always has a stored row");
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

    /// Force the rejected row, never another account. The detached owned operation
    /// persists a rotated refresh token even if its requesting stream is cancelled.
    pub async fn force_refresh(
        self: &Arc<Self>,
        provider: &str,
        expected: CredentialStamp,
    ) -> Result<Option<Resolved>, ResolveError> {
        let resolver = self.clone();
        let name = provider.to_owned();
        let provider = name.clone();
        tokio::spawn(async move {
            match resolver.refresh_row(&provider, expected, true).await? {
                Attempt::Ready(resolved) => Ok(Some(resolved)),
                Attempt::Skip => Ok(None),
                Attempt::Transient(cause) => {
                    Err(ResolveError::RefreshUnavailable { provider, cause })
                }
            }
        })
        .await
        .map_err(|_| ResolveError::RefreshUnavailable {
            provider: name,
            cause: "refresh operation stopped".into(),
        })?
    }

    async fn refresh_row(
        &self,
        provider: &str,
        expected: CredentialStamp,
        forced: bool,
    ) -> Result<Attempt, ResolveError> {
        let key = RowKey {
            provider: provider.to_owned(),
            id: expected.id,
            generation: expected.generation,
        };
        let flight = self
            .flights
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .clone();
        let attempt = self.fly(&key, expected, &flight, forced).await;
        // Forget the row once no other caller holds or awaits its flight.
        let mut flights = self.flights.lock().await;
        if flights.get(&key).is_some_and(|f| Arc::ptr_eq(f, &flight))
            && Arc::strong_count(&flight) == 2
        {
            flights.remove(&key);
        }
        attempt
    }

    async fn fly(
        &self,
        key: &RowKey,
        expected: CredentialStamp,
        flight: &RowFlight,
        forced: bool,
    ) -> Result<Attempt, ResolveError> {
        let provider = key.provider.as_str();
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
        let Some(row) = snapshot.rows(provider).iter().find(|row| {
            row.id == expected.id
                && row.generation == expected.generation
                && row.disabled_cause.is_none()
        }) else {
            self.recent_mints.lock().await.remove(key);
            return Ok(Attempt::Skip);
        };
        let Credential::OAuth(oauth) = &row.credential else {
            self.recent_mints.lock().await.remove(key);
            return Ok(Attempt::Skip);
        };
        let fresh = self.fresh(provider, row);
        let mut mints = self.recent_mints.lock().await;
        let recent = mints.get(key).is_some_and(|(rev, at)| {
            *rev == row.rev
                && fresh.is_some()
                && (self.clock)().saturating_sub(*at) < REMINT_COOLDOWN_MS
        });
        if !recent {
            mints.remove(key);
        }
        drop(mints);
        if !forced || expected.rev != row.rev {
            if let Some(resolved) = fresh {
                return Ok(Attempt::Ready(resolved));
            }
        } else if recent {
            return Ok(Attempt::Skip);
        }
        // The exchange this caller queued behind failed transiently at this rev.
        if let Some((_, cause)) = last_transient
            .as_ref()
            .filter(|(rev, _)| joined && *rev == Some(row.rev))
        {
            return Ok(Attempt::Transient(cause.clone()));
        }
        // A stale new revision is refreshed with its current refresh token.
        let attempt = self.exchange(provider, key, row, oauth).await;
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
        key: &RowKey,
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
                let identity = CredentialIdentity {
                    account_id: tokens
                        .identity
                        .account_id
                        .as_ref()
                        .or(oauth.identity.account_id.as_ref())
                        .cloned(),
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
                };
                let credential = || {
                    Credential::OAuth(OAuthCredential {
                        access: access.clone(),
                        refresh: tokens.refresh.as_ref().unwrap_or(&oauth.refresh).clone(),
                        expires_at_ms: tokens.expires_at_ms,
                        authorized_at_ms: oauth.authorized_at_ms,
                        identity: identity.clone(),
                    })
                };
                let mut persisted = self
                    .store
                    .replace_if_matches(provider, row.stamp(), credential())
                    .await;
                if matches!(persisted, Err(AuthError::Io(_) | AuthError::Locked(_))) {
                    persisted = self
                        .store
                        .replace_if_matches(provider, row.stamp(), credential())
                        .await;
                    if matches!(persisted, Err(AuthError::Io(_) | AuthError::Locked(_))) {
                        tracing::warn!(
                            provider,
                            row_id = row.id.0,
                            "rotated refresh token not persisted; re-login may be required"
                        );
                        // No revision holds the minted token, so no cooldown is recorded;
                        // the served revision stays the observed one.
                        return Ok(Attempt::Ready(resolved_oauth(
                            provider,
                            row.stamp(),
                            access,
                            &identity,
                        )));
                    }
                }
                match persisted {
                    Ok(CasOutcome::Applied) => {
                        self.remember_mint(key, row.rev + 1).await;
                        Ok(Attempt::Ready(resolved_oauth(
                            provider,
                            CredentialStamp {
                                rev: row.rev + 1,
                                ..row.stamp()
                            },
                            access,
                            &identity,
                        )))
                    }
                    Ok(CasOutcome::Stale) => self.peer_fresh(provider, row.stamp()).await,
                    Err(_) => Ok(Attempt::Transient("refresh write unavailable".into())),
                }
            }
            Err(RefreshFailure::Transient(cause)) => Ok(Attempt::Transient(cause)),
            Err(RefreshFailure::Definitive(cause)) => {
                let current = match self.store.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => return Ok(Attempt::Transient("refresh store unavailable".into())),
                };
                let Some(current) = current.rows(provider).iter().find(|current| {
                    current.id == row.id
                        && current.generation == row.generation
                        && current.disabled_cause.is_none()
                }) else {
                    return Ok(Attempt::Skip);
                };
                if current.rev != row.rev {
                    return Ok(match self.fresh(provider, current) {
                        Some(resolved) => Attempt::Ready(resolved),
                        None => Attempt::Skip,
                    });
                }
                let disabled_cause = format!("oauth refresh failed: {cause}");
                match self
                    .store
                    .disable_if_matches(provider, row.stamp(), &disabled_cause)
                    .await
                {
                    Ok(CasOutcome::Applied) => {
                        tracing::warn!(
                            provider,
                            row_id = row.id.0,
                            cause,
                            "OAuth credential disabled"
                        );
                    }
                    Ok(CasOutcome::Stale) => {}
                    Err(_) => return Ok(Attempt::Transient("refresh write unavailable".into())),
                }
                Ok(Attempt::Skip)
            }
        }
    }

    /// omp `#rememberMint`: expired entries are pruned whenever a mint is recorded.
    async fn remember_mint(&self, key: &RowKey, rev: u64) {
        let now = (self.clock)();
        let mut mints = self.recent_mints.lock().await;
        mints.retain(|_, (_, at)| now.saturating_sub(*at) < REMINT_COOLDOWN_MS);
        mints.insert(key.clone(), (rev, now));
    }

    fn fresh(&self, provider: &str, row: &StoredCredential) -> Option<Resolved> {
        if row.disabled_cause.is_some() {
            return None;
        }
        match &row.credential {
            Credential::OAuth(oauth)
                if key_cascade::oauth_is_fresh(oauth.expires_at_ms, (self.clock)()) =>
            {
                Some(resolved_oauth(
                    provider,
                    row.stamp(),
                    oauth.access.clone(),
                    &oauth.identity,
                ))
            }
            _ => None,
        }
    }

    async fn peer_fresh(
        &self,
        provider: &str,
        expected: CredentialStamp,
    ) -> Result<Attempt, ResolveError> {
        let snapshot =
            self.store
                .snapshot()
                .await
                .map_err(|_| ResolveError::RefreshUnavailable {
                    provider: provider.into(),
                    cause: "refresh store unavailable".into(),
                })?;
        // A concurrent login or recreated file is not a peer refresh of this grant.
        let Some(row) = snapshot.rows(provider).iter().find(|row| {
            row.id == expected.id
                && row.generation == expected.generation
                && row.disabled_cause.is_none()
        }) else {
            return Ok(Attempt::Skip);
        };
        Ok(match self.fresh(provider, row) {
            Some(resolved) => Attempt::Ready(resolved),
            None => Attempt::Transient("concurrent update".into()),
        })
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
            Arc::new(crate::infrastructure::utils::auth_now_ms),
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
    /// A candidate key under validation; it wins the cascade as `Runtime`.
    runtime: Option<SecretString>,
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
            runtime: None,
        }
    }

    /// A handle over one explicit key, read neither from env nor the store. It
    /// resolves as `AuthSource::Runtime` without a row, so `recover` is a no-op.
    pub fn runtime(resolver: Arc<KeyResolver>, provider: String, key: SecretString) -> Self {
        Self {
            resolver,
            provider,
            env_vars: Vec::new(),
            oauth_allowed: false,
            auth_header: None,
            runtime: Some(key),
        }
    }

    pub async fn current(&self) -> Result<Resolved, ProviderError> {
        // A dropped request must not drop a refresh between the token grant and its
        // write: the rotated refresh token would be lost and the row later disabled.
        // omp likewise lets the leased exchange finish and persist when its caller stops.
        tokio::spawn(self.clone().resolve())
            .await
            .map_err(|_| ProviderError::ConnectionFailed("credential resolution stopped".into()))?
    }

    pub async fn recover(&self, rejected: &Resolved) -> Result<Option<Resolved>, ProviderError> {
        if rejected.source != AuthSource::StoredOAuth {
            return Ok(None);
        }
        let Some(expected) = rejected.row else {
            return Ok(None);
        };
        let auth = self
            .resolver
            .force_refresh(&self.provider, expected)
            .await
            .map_err(|error| ProviderError::ConnectionFailed(error.to_string()))?;
        // One consolidated exposure boundary: do not retry the bearer already rejected.
        fn bearer(auth: &ResolvedAuth) -> &str {
            auth.bearer_secret().expose_secret()
        }
        Ok(auth.filter(|resolved| bearer(&resolved.auth) != bearer(&rejected.auth)))
    }

    async fn resolve(self) -> Result<Resolved, ProviderError> {
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
            .resolve_tracked_with_policy(
                &self.provider,
                self.runtime.as_ref(),
                names,
                &crate::infrastructure::utils::env_var_trimmed,
                self.oauth_allowed,
            )
            .await
            .map_err(|error| ProviderError::ConnectionFailed(error.to_string()))?
            .ok_or(ProviderError::AuthenticationFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoRefresh;
    #[async_trait::async_trait]
    impl OAuthRefreshPort for NoRefresh {
        async fn refresh(
            &self,
            _: &str,
            _: &OAuthCredential,
        ) -> Result<crate::domain::ports::RefreshedTokens, RefreshFailure> {
            Err(RefreshFailure::Transient("never called".into()))
        }
    }

    #[tokio::test]
    async fn runtime_handle_serves_its_key_without_a_row_or_recovery() {
        let resolver = compose(
            Arc::new(crate::adapters::noop::NoOpAuthStore),
            Arc::new(NoRefresh),
            Arc::new(|| 0),
        );
        let handle = CredentialHandle::runtime(
            resolver,
            "xai".into(),
            SecretString::new("candidate".into()),
        );
        let resolved = handle.current().await.unwrap();
        assert_eq!(resolved.source, AuthSource::Runtime);
        assert!(resolved.row.is_none());
        assert!(matches!(resolved.auth, ResolvedAuth::ApiKey(_)));
        assert!(handle.recover(&resolved).await.unwrap().is_none());
    }
}
