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

/// The canonical root as requests see it (rebased in instrumented builds).
fn canonical_root() -> String {
    headers::derived(DEFAULT_BASE_URL)
}

/// A root re-probed by discovery for the credential scope it was found under.
#[derive(Debug)]
struct Learned {
    context: String,
    scope: String,
    root: String,
}

/// Per-id facts of the last live discovery: its output limit and wire id.
#[derive(Debug, Clone, Default)]
pub(crate) struct LiveRow {
    pub max_output: Option<u32>,
}

/// State every route of one adapter shares: configured base, learned root,
/// served catalogue and the last live discovery.
#[derive(Debug)]
pub(crate) struct Shared {
    pub configured_base: Option<String>,
    learned: ArcSwap<Option<Learned>>,
    served: ArcSwap<Option<Vec<ModelDescriptor>>>,
    live: ArcSwap<HashMap<String, LiveRow>>,
}

impl Shared {
    fn new(configured_base: Option<String>) -> Self {
        Self {
            configured_base: configured_base.filter(|base| !base.trim().is_empty()),
            learned: ArcSwap::from_pointee(None),
            served: ArcSwap::from_pointee(None),
            live: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    /// The re-probed root when it was learned for this credential's scope.
    pub fn learned_root(&self, resolved: &Resolved) -> Option<String> {
        let learned = self.learned.load();
        let learned = learned.as_ref().as_ref()?;
        (crate::adapters::model_catalog_cache::request_scope(&learned.context, resolved)
            == learned.scope)
            .then(|| learned.root.clone())
    }

    fn learn(&self, context: &str, scope: &str, root: Option<String>) {
        self.learned.store(Arc::new(root.map(|root| Learned {
            context: context.to_owned(),
            scope: scope.to_owned(),
            root,
        })));
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

    /// The id was returned by this process's last live discovery.
    pub fn is_live(&self, id: &str) -> bool {
        self.live.load().contains_key(id)
    }

    /// The bundled output limit, else the live one.
    fn max_output(&self, id: &str) -> Option<u32> {
        catalog::seed_row(id)
            .map(|row| row.max_output)
            .or_else(|| self.live.load().get(id).and_then(|row| row.max_output))
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
    warm: Arc<Mutex<LruCache<String, ()>>>,
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
    /// sampling gate, all from the logical id.
    fn prepare(&self, route: Route, options: &mut CompletionOptions) -> Option<ModelDescriptor> {
        let logical = options.model.clone();
        let descriptor = self.inner.shared.descriptor(&logical);
        let wire = descriptor.as_ref().and_then(|d| d.wire_model_id.clone());
        // A synthetic `-1m` sibling is its base row (omp spreads the base):
        // without its own facts it takes the base's limit and sampling gate.
        let base = wire.as_deref().unwrap_or(&logical);
        let limit = self
            .inner
            .shared
            .max_output(&logical)
            .or_else(|| self.inner.shared.max_output(base));
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
        descriptor: Option<ModelDescriptor>,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let resolved = self.inner.handle.current().await?;
        let has = |capability: ModelCapability| {
            descriptor
                .as_ref()
                .is_some_and(|d| d.capabilities.contains(&capability))
        };
        let session = options.session_id.clone().filter(|id| !id.is_empty());
        let warm = session
            .as_ref()
            .is_some_and(|id| self.warm.lock().contains(id));
        let root = self.inner.responses.root(&resolved);
        let mut route_headers = HeaderMap::new();
        self.inner
            .responses
            .route_headers(&messages, session.as_deref(), &mut route_headers);
        let wire = CopilotWire {
            inner: self.inner.clone(),
            cache_key: session
                .as_deref()
                .map(crate::adapters::responses::prompt_cache_key),
            reasoning: has(ModelCapability::Thinking),
            vision: has(ModelCapability::Vision),
            max_output_tokens: (options.max_tokens > 0).then_some(options.max_tokens),
            temperature: options.temperature,
            warm,
            route_headers,
            state: self.inner.responses.state(&resolved, &root),
        };
        let url = format!("{root}/responses");
        let stream =
            transport::stream(self.inner.clone(), &url, resolved, wire, messages, options).await?;
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
        let descriptor = self.prepare(route, &mut options);
        match route {
            Route::Messages => self.messages.stream_completion(messages, options).await,
            Route::Chat => self.chat.stream_completion(messages, options).await,
            Route::Responses => self.stream_responses(descriptor, messages, options).await,
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
