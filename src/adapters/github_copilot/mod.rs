//! GitHub Copilot provider: device login with Enterprise domains, routing
//! probe and model-policy enablement, Copilot header identity, and
//! model-dependent Messages / Responses / Chat Completions inference, adapted
//! from oh-my-pi (ai/src/registry/oauth/github-copilot.ts,
//! ai/src/providers/github-copilot-headers.ts, catalog/src/wire/github-copilot.ts,
//! catalog/src/compat/rules/runtime/behavior.kdl).
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

pub mod catalog;
mod discovery;
pub mod domain;
pub mod headers;
pub mod identity;
pub mod login;
pub mod policy;
pub mod routing;

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use lru::LruCache;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderValue};

use self::catalog::Route;
use self::headers::{CopilotGrant, CopilotProfile};
use self::identity::CopilotRequestState;
use crate::adapters::anthropic::AnthropicAdapter;
use crate::adapters::openai::{ChatCredential, OpenAiAdapter, OpenAiCompatibleVariant};
use crate::adapters::provider::request_profile::{Alternate, RequestState};
use crate::adapters::responses::{
    Dialect, SystemRole, WireProfile,
    transport::{self, AbortSlot, Host, Wire},
};
use crate::domain::{
    errors::ProviderError,
    models::{
        CompletionOptions, Message, ModelCapability, ModelDescriptor, ResolvedAuth, StopReason,
        StreamChunk,
    },
    ports::{ProbeOutcome, StreamingProvider},
};
use crate::infrastructure::key_resolver::{CredentialHandle, Resolved};

/// The canonical Copilot API root (omp `getGitHubCopilotBaseUrl()`).
pub const DEFAULT_BASE_URL: &str = "https://api.githubcopilot.com";
pub const PROVIDER_ID: &str = "github-copilot";
/// omp's window for an id no catalogue layer describes.
const DEFAULT_CONTEXT: u32 = 128_000;
/// omp `OPENAI_MAX_OUTPUT_TOKENS`, the clamp of the OpenAI-wire routes.
const OPENAI_OUTPUT_CLAMP: u32 = 64_000;
/// Sessions whose native Responses history is warm in this adapter.
const WARM_SESSIONS: usize = 256;
/// Credential scopes whose discovery facts one adapter keeps.
const SCOPES: usize = 16;

/// The canonical root as requests see it (rebased in instrumented builds).
fn canonical_root() -> String {
    headers::derived(DEFAULT_BASE_URL)
}

/// Per-id facts of the last live discovery: its output limit and wire id.
#[derive(Debug, Clone, Default)]
pub(crate) struct LiveRow {
    pub max_output: Option<u32>,
}

/// What discovery learned for one credential scope: the re-probed account root
/// and the ids of its last live listing. One account's discovery never answers,
/// clears or overwrites another account's requests.
#[derive(Debug, Clone, Default)]
struct ScopeFacts {
    root: Option<String>,
    live: Arc<HashMap<String, LiveRow>>,
}

/// State every route of one adapter shares: configured base, the per-scope
/// discovery facts and the served catalogue.
#[derive(Debug)]
pub(crate) struct Shared {
    pub configured_base: Option<String>,
    /// The discovery scope context, learned with the first discovery.
    context: ArcSwap<Option<String>>,
    facts: Mutex<LruCache<String, ScopeFacts>>,
    served: ArcSwap<Option<Vec<ModelDescriptor>>>,
}

impl Shared {
    fn new(configured_base: Option<String>) -> Self {
        Self {
            configured_base: configured_base.filter(|base| !base.trim().is_empty()),
            context: ArcSwap::from_pointee(None),
            facts: Mutex::new(LruCache::new(
                NonZeroUsize::new(SCOPES).expect("non-zero capacity"),
            )),
            served: ArcSwap::from_pointee(None),
        }
    }

    /// The facts discovery learned for this credential's scope.
    fn facts(&self, resolved: &Resolved) -> Option<ScopeFacts> {
        let context = self.context.load();
        let scope = crate::adapters::model_catalog_cache::request_scope(
            context.as_ref().as_ref()?,
            resolved,
        );
        self.facts.lock().get(&scope).cloned()
    }

    /// The re-probed root when it was learned for this credential's scope.
    pub fn learned_root(&self, resolved: &Resolved) -> Option<String> {
        self.facts(resolved)?.root
    }

    /// A re-probe result for one scope; a failed re-probe forgets only its root.
    fn learn(&self, context: &str, scope: &str, root: Option<String>) {
        self.update(context, scope, |facts| facts.root = root);
    }

    /// The live listing of one scope replaces only that scope's listing.
    fn listed(&self, context: &str, scope: &str, live: HashMap<String, LiveRow>) {
        self.update(context, scope, |facts| facts.live = Arc::new(live));
    }

    fn update(&self, context: &str, scope: &str, change: impl FnOnce(&mut ScopeFacts)) {
        if self.context.load().as_ref().as_deref() != Some(context) {
            self.context.store(Arc::new(Some(context.to_owned())));
        }
        let mut facts = self.facts.lock();
        change(facts.get_or_insert_mut(scope.to_owned(), ScopeFacts::default));
    }

    /// The bundled rows.
    fn seed() -> &'static [crate::adapters::model_catalog_cache::CachedModelEntry] {
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|catalog| catalog.providers.get(PROVIDER_ID))
            .map(|entry| entry.models.as_slice())
            .unwrap_or_default()
    }

    /// The served catalogue row of `id`, else its seed row.
    pub fn descriptor(&self, id: &str) -> Option<ModelDescriptor> {
        if let Some(models) = &**self.served.load()
            && let Some(model) = models.iter().find(|m| m.model_id == id)
        {
            return Some(model.clone());
        }
        Self::seed()
            .iter()
            .map(|entry| &entry.descriptor)
            .find(|m| m.model_id == id)
            .cloned()
    }

    /// The id was returned by the last live discovery of this credential's scope.
    pub fn is_live(&self, resolved: &Resolved, id: &str) -> bool {
        self.facts(resolved)
            .is_some_and(|facts| facts.live.contains_key(id))
    }

    /// The bundled output limit, else the live one of this credential's scope.
    fn max_output(&self, facts: Option<&ScopeFacts>, id: &str) -> Option<u32> {
        catalog::seed_row(id).map(|row| row.max_output).or_else(|| {
            facts
                .and_then(|facts| facts.live.get(id))
                .and_then(|row| row.max_output)
        })
    }
}

struct Inner {
    client: reqwest::Client,
    handle: CredentialHandle,
    abort: AbortSlot,
    shared: Arc<Shared>,
    responses: CopilotProfile,
}

impl Host for Inner {
    fn client(&self) -> &reqwest::Client {
        &self.client
    }
    fn credentials(&self) -> &CredentialHandle {
        &self.handle
    }
    fn abort_slot(&self) -> &AbortSlot {
        &self.abort
    }
}

impl Inner {
    /// `GET <root>/models` with the discovery headers, status only; a stored
    /// row's 401 recovers once.
    async fn probe_status(&self) -> Result<(u16, Duration), ProviderError> {
        let start = Instant::now();
        let mut resolved = self.handle.current().await?;
        let mut recovered = false;
        loop {
            let url = format!("{}/models", self.responses.root(&resolved));
            let response = self
                .client
                .get(&url)
                .headers(listing_headers(&resolved.auth)?)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
            let status = response.status().as_u16();
            drop(response);
            if status == 401
                && transport::recover_once(&self.handle, &mut resolved, &mut recovered).await?
            {
                continue;
            }
            return Ok((status, start.elapsed()));
        }
    }
}

/// Model-listing headers: `Accept: application/json`, the discovery identity
/// and the bearer.
fn listing_headers(auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
    let mut out = HeaderMap::new();
    out.insert("accept", HeaderValue::from_static("application/json"));
    headers::discovery_headers(&mut out);
    out.insert("authorization", CopilotGrant::from_auth(auth).bearer()?);
    Ok(out)
}

fn listing_status(status: u16) -> Result<(), ProviderError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(ProviderError::AuthenticationFailed),
        403 => Err(ProviderError::Other(
            "GitHub Copilot denied model listing (HTTP 403)".into(),
        )),
        404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
        _ => Err(ProviderError::Other(format!(
            "GitHub Copilot models HTTP {status}"
        ))),
    }
}

/// The request-scoped Responses policy of one Copilot request.
struct CopilotWire {
    inner: Arc<Inner>,
    cache_key: Option<String>,
    reasoning: bool,
    vision: bool,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    warm: bool,
    /// Message-derived headers, fixed for the logical request.
    route_headers: HeaderMap,
    state: CopilotRequestState,
}

impl WireProfile for CopilotWire {
    fn dialect(&self) -> Dialect {
        Dialect::Generic
    }
    fn prompt_cache_key(&self) -> Option<&str> {
        self.cache_key.as_deref()
    }
    fn client_metadata(&self) -> Option<&serde_json::Value> {
        None
    }
    /// A cold session rebuilds its history from message content only.
    fn replay_provider(&self) -> Option<&str> {
        self.warm.then_some(PROVIDER_ID)
    }
    fn capture_provider(&self) -> Option<&str> {
        Some(PROVIDER_ID)
    }
    fn reasoning(&self) -> bool {
        self.reasoning
    }
    fn vision(&self) -> bool {
        self.vision
    }
    fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
    fn temperature(&self) -> Option<f32> {
        self.temperature
    }
    /// Every bundled reasoning Responses row supports the developer role.
    fn system_role(&self) -> SystemRole {
        if self.reasoning {
            SystemRole::Developer
        } else {
            SystemRole::Instructions
        }
    }
}

#[async_trait]
impl Wire for CopilotWire {
    fn headers(&self, _model: &str, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut out = HeaderMap::new();
        out.insert("content-type", HeaderValue::from_static("application/json"));
        out.insert("authorization", CopilotGrant::from_auth(auth).bearer()?);
        for (name, value) in &self.route_headers {
            out.insert(name, value.clone());
        }
        self.state.headers(&mut out);
        Ok(out)
    }
    fn http_error(
        &self,
        status: u16,
        headers: &HeaderMap,
        body: &str,
        request: &HeaderMap,
    ) -> ProviderError {
        headers::http_error(status, headers, body, request)
    }
    fn redact(&self, message: &str, request: &HeaderMap) -> String {
        headers::redact(message, request)
    }
    /// The identity state is keyed by the token: a recovered credential starts
    /// a new one, and its root may differ.
    fn recovered(&mut self, resolved: &Resolved) -> Result<Option<String>, ProviderError> {
        let root = self.inner.responses.root(resolved);
        self.state = self.inner.responses.state(resolved, &root);
        Ok(Some(format!("{root}/responses")))
    }
    fn alternate(&mut self, status: u16, body: &str, sent: &HeaderMap) -> Option<Alternate> {
        self.state.alternate(status, body.as_bytes(), sent)
    }
}

/// The GitHub Copilot provider: one credential handle, three wire routes.
pub struct CopilotAdapter {
    inner: Arc<Inner>,
    messages: AnthropicAdapter,
    chat: OpenAiAdapter,
    /// Warm native Responses histories, keyed by (session, credential scope).
    warm: Arc<Mutex<LruCache<(String, String), ()>>>,
}

impl std::fmt::Debug for CopilotAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotAdapter")
            .field("configured_base", &self.inner.shared.configured_base)
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl CopilotAdapter {
    /// `configured_base` is the entry's raw `base_url`, sent verbatim.
    pub fn new(
        handle: CredentialHandle,
        model: String,
        configured_base: Option<String>,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::ConnectionFailed(
                    "Could not create GitHub Copilot HTTP client".into(),
                )
            })?;
        let shared = Arc::new(Shared::new(configured_base));
        let messages = AnthropicAdapter::with_profile(
            handle.clone(),
            model.clone(),
            Arc::new(CopilotProfile::new(Route::Messages, shared.clone())),
        )?;
        let chat = OpenAiAdapter::new(
            OpenAiCompatibleVariant::Custom {
                provider_id: PROVIDER_ID.into(),
                display_name: "GitHub Copilot".into(),
                context_window: None,
                supports_tools: None,
            },
            ChatCredential::Resolved(handle.clone()),
            model,
            Arc::new(CopilotProfile::new(Route::Chat, shared.clone())),
        )?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                handle,
                abort: AbortSlot::default(),
                responses: CopilotProfile::new(Route::Responses, shared.clone()),
                shared,
            }),
            messages,
            chat,
            warm: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(WARM_SESSIONS).expect("non-zero capacity"),
            ))),
        })
    }

    /// omp's per-model request shaping: the wire id, the output clamp and the
    /// sampling gate, all from the logical id; live limits are those of the
    /// request credential's own discovery.
    fn prepare(
        &self,
        route: Route,
        resolved: &Resolved,
        options: &mut CompletionOptions,
    ) -> Option<ModelDescriptor> {
        let logical = options.model.clone();
        let descriptor = self.inner.shared.descriptor(&logical);
        let wire = descriptor.as_ref().and_then(|d| d.wire_model_id.clone());
        // A synthetic `-1m` sibling is its base row (omp spreads the base):
        // without its own facts it takes the base's limit and sampling gate.
        let base = wire.as_deref().unwrap_or(&logical);
        let facts = self.inner.shared.facts(resolved);
        let limit = self
            .inner
            .shared
            .max_output(facts.as_ref(), &logical)
            .or_else(|| self.inner.shared.max_output(facts.as_ref(), base));
        if options.max_tokens > 0
            && let Some(limit) = limit
        {
            options.max_tokens = options.max_tokens.min(limit);
        }
        if route != Route::Messages && options.max_tokens > 0 {
            options.max_tokens = options.max_tokens.min(OPENAI_OUTPUT_CLAMP);
        }
        let sampling = match catalog::seed_row(base) {
            Some(row) => row.sampling,
            None => route != Route::Responses || !base.to_ascii_lowercase().starts_with("gpt-"),
        };
        if !sampling {
            options.temperature = None;
        }
        if let Some(wire) = wire {
            options.model = wire;
        }
        descriptor
    }

    async fn stream_responses(
        &self,
        cancel: transport::Armed,
        resolved: Resolved,
        descriptor: Option<ModelDescriptor>,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let has = |capability: ModelCapability| {
            descriptor
                .as_ref()
                .is_some_and(|d| d.capabilities.contains(&capability))
        };
        // A session is warm per account: another account never replays its items.
        let session = options
            .session_id
            .clone()
            .filter(|id| !id.is_empty())
            .map(|id| {
                (
                    id,
                    crate::adapters::model_catalog_cache::request_scope(PROVIDER_ID, &resolved),
                )
            });
        let warm = session
            .as_ref()
            .is_some_and(|key| self.warm.lock().contains(key));
        // The root is computed once for the logical request.
        let root = self.inner.responses.root(&resolved);
        let mut route_headers = HeaderMap::new();
        self.inner.responses.route_headers(
            &messages,
            session.as_ref().map(|(id, _)| id.as_str()),
            &mut route_headers,
        );
        let wire = CopilotWire {
            inner: self.inner.clone(),
            cache_key: session
                .as_ref()
                .map(|(id, _)| crate::adapters::responses::prompt_cache_key(id)),
            reasoning: has(ModelCapability::Thinking),
            vision: has(ModelCapability::Vision),
            max_output_tokens: (options.max_tokens > 0).then_some(options.max_tokens),
            temperature: options.temperature,
            warm,
            route_headers,
            state: self.inner.responses.state(&resolved, &root),
        };
        let url = format!("{root}/responses");
        let stream = transport::stream(
            self.inner.clone(),
            &url,
            cancel,
            resolved,
            wire,
            messages,
            options,
        )
        .await?;
        let Some(session) = session else {
            return Ok(stream);
        };
        // The session turns warm at its first successfully completed response.
        let warm_set = self.warm.clone();
        Ok(Box::pin(stream.inspect(move |chunk| {
            if matches!(
                chunk,
                StreamChunk::TurnComplete { stop_reason } if *stop_reason != StopReason::Cancelled
            ) {
                warm_set.lock().put(session.clone(), ());
            }
        })))
    }
}

#[async_trait]
impl StreamingProvider for CopilotAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        mut options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let route = catalog::route(&options.model);
        // One credential read per logical request: the live limits `prepare`
        // applies and the grant every route sends belong to the same account.
        // Armed first: an abort while the credential resolves cancels this
        // request. Past the read, Messages and Chat keep their own abort state.
        let cancel = self.inner.abort.arm();
        let resolved = cancel.run(self.inner.handle.current()).await?;
        let descriptor = self.prepare(route, &resolved, &mut options);
        match route {
            Route::Messages => {
                drop(cancel);
                self.messages
                    .stream_resolved(resolved, messages, options)
                    .await
            }
            Route::Chat => {
                drop(cancel);
                self.chat
                    .stream_with(Some(resolved), messages, options)
                    .await
            }
            Route::Responses => {
                self.stream_responses(cancel, resolved, descriptor, messages, options)
                    .await
            }
        }
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        self.inner.abort.abort().await;
        self.messages.abort().await?;
        self.chat.abort().await
    }
    fn provider_id(&self) -> String {
        PROVIDER_ID.into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        if let Some(models) = &**self.inner.shared.served.load() {
            return models.clone();
        }
        Shared::seed()
            .iter()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }
    fn set_discovered_models(&self, models: Vec<ModelDescriptor>) {
        self.inner.shared.served.store(Arc::new(Some(models)));
    }
    async fn discover_models(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Option<Result<crate::domain::models::provider::DiscoveredModels, ProviderError>> {
        Some(self.inner.discover(model_filter, scope_context).await)
    }
    /// Credentials resolve per request, so the scope is read through that same path.
    async fn discovery_scope(&self, scope_context: &str) -> Option<String> {
        let resolved = self.inner.handle.current().await.ok()?;
        Some(crate::adapters::model_catalog_cache::request_scope(
            scope_context,
            &resolved,
        ))
    }
    fn default_context_window(&self, _model_id: &str) -> Option<u32> {
        Some(DEFAULT_CONTEXT)
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        let (status, _) = tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
            .await
            .map_err(|_| {
                ProviderError::ConnectionFailed("GitHub Copilot health check timed out".into())
            })??;
        listing_status(status)
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        let (status, latency) =
            tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
                .await
                .map_err(|_| ProviderError::Offline("GitHub Copilot probe timed out".into()))?
                .map_err(|e| {
                    if matches!(
                        e,
                        ProviderError::ConnectionFailed(_) | ProviderError::Offline(_)
                    ) {
                        ProviderError::Offline("GitHub Copilot probe transport failed".into())
                    } else {
                        e
                    }
                })?;
        listing_status(status).map(|()| ProbeOutcome { latency })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keystone (DF-20-1D-RESPONSES-ABORT-WINDOW): on the Responses route, an
    /// abort while the stored grant refreshes cancels the request before
    /// anything is sent.
    #[tokio::test]
    async fn abort_while_the_credential_resolves_cancels_the_responses_stream() {
        use crate::adapters::responses::transport::test_support::{
            loopback, options, stored_oauth, user, within,
        };
        use futures::FutureExt;

        let (handle, refresh, _dir) = stored_oauth(
            PROVIDER_ID,
            true,
            crate::domain::models::CredentialIdentity::default(),
        )
        .await;
        let (listener, origin) = loopback().await;
        assert_eq!(catalog::route("gpt-5"), Route::Responses);
        let adapter = Arc::new(CopilotAdapter::new(handle, "gpt-5".into(), Some(origin)).unwrap());
        let task = tokio::spawn({
            let adapter = adapter.clone();
            async move {
                adapter
                    .stream_completion(vec![user("hi")], options("gpt-5"))
                    .await
                    .map(drop)
            }
        });
        within(refresh.entered.notified()).await;
        adapter.abort().await.unwrap();
        let result = within(task).await.unwrap();
        assert!(
            matches!(result, Err(ProviderError::Cancelled)),
            "{result:?}"
        );
        assert!(
            listener.accept().now_or_never().is_none(),
            "nothing is sent after the abort"
        );
        assert_eq!(adapter.inner.abort.live(), 0);
    }
}
