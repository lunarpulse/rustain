//! OpenRouter: Responses by default, Chat Completions under an explicit switch,
//! one credential for both; adapted from oh-my-pi (`ai/src/stream.ts` dispatch,
//! `ai/src/providers/openai-responses.ts`, `openai-completions.ts`,
//! `ai/src/registry/api-key-validation.ts`).
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

mod errors;
mod profile;

use crate::{
    adapters::{
        openai::{ChatCredential, OpenAiAdapter, OpenAiCompatibleVariant},
        responses::transport::{self, AbortSlot, Host},
    },
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, ModelCapability, ModelDescriptor, StreamChunk},
        ports::{ProbeOutcome, StreamingProvider},
    },
    infrastructure::key_resolver::CredentialHandle,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// omp's OpenRouter API base.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The explicit transport switch: exactly `0` sends a turn over Chat
/// Completions; anything else, or unset, uses Responses (omp
/// `PI_OPENROUTER_RESPONSES`). No error ever switches transports.
pub const RESPONSES_SWITCH: &str = "RUSTAIN_OPENROUTER_RESPONSES";

/// One base for requests, discovery, origin guard and cache scope: a configured
/// base (normalised, never rebased), else the default (rebased only by the
/// instrumented test origin). Read at each use.
pub fn base_url(configured: Option<&str>) -> String {
    let base = configured.map_or_else(
        || crate::adapters::auth::policies::endpoint(DEFAULT_BASE_URL),
        str::to_owned,
    );
    crate::infrastructure::utils::normalize_base_url(&base)
}

fn responses_enabled() -> bool {
    crate::infrastructure::utils::env_var_trimmed(RESPONSES_SWITCH).as_deref() != Some("0")
}

/// The served catalogue (live discovery, else the bundled rows), shared with
/// the Chat profile so both transports read one model's traits alike.
struct Served(arc_swap::ArcSwap<Option<Vec<ModelDescriptor>>>);

impl Served {
    fn new() -> Self {
        Self(arc_swap::ArcSwap::from_pointee(None))
    }
    /// `(reasoning, vision)` of the served row of `model`; unknown ids have neither.
    /// A routing suffix (`:nitro`, `:floor`, …) is no catalogue row: an id with
    /// one reads its base model's row, as omp keeps the variant out of the id.
    fn traits(&self, model: &str) -> (bool, bool) {
        let of = |descriptor: &ModelDescriptor| {
            (
                descriptor.capabilities.contains(&ModelCapability::Thinking),
                descriptor.capabilities.contains(&ModelCapability::Vision),
            )
        };
        let served = self.0.load();
        let find = |id: &str| match &**served {
            Some(models) => models.iter().find(|m| m.model_id == id).map(of),
            None => OpenAiCompatibleVariant::OpenRouter
                .known_models("")
                .iter()
                .find(|m| m.model_id == id)
                .map(of),
        };
        find(model)
            .or_else(|| model.rsplit_once(':').and_then(|(base, _)| find(base)))
            .unwrap_or_default()
    }
}

struct Inner {
    client: reqwest::Client,
    handle: CredentialHandle,
    configured_base: Option<String>,
    abort: AbortSlot,
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
    fn base(&self) -> String {
        base_url(self.configured_base.as_deref())
    }
    /// `GET <base>/auth/key` with the bearer only: the one request that tells a
    /// valid key from any bearer (`/models` answers 200 to every bearer). The
    /// body is ignored.
    async fn key_status(&self) -> Result<(u16, Duration), ProviderError> {
        let start = Instant::now();
        let resolved = self.handle.current().await?;
        let bearer = crate::adapters::openai::authorization(resolved.auth.bearer_secret())?
            .ok_or(ProviderError::AuthenticationFailed)?;
        let response = self
            .client
            .get(format!("{}/auth/key", self.base()))
            .header("authorization", bearer)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
        Ok((response.status().as_u16(), start.elapsed()))
    }
}

/// One adapter, one credential, two transports: the Responses route here and
/// the owned Chat adapter, which also serves discovery and the catalogue.
pub struct OpenRouterAdapter {
    inner: Arc<Inner>,
    chat: OpenAiAdapter,
    served: Arc<Served>,
}

impl std::fmt::Debug for OpenRouterAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterAdapter")
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl OpenRouterAdapter {
    pub fn new(
        handle: CredentialHandle,
        model: String,
        configured_base: Option<String>,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::ConnectionFailed("Could not create OpenRouter HTTP client".into())
            })?;
        let served = Arc::new(Served::new());
        let chat = OpenAiAdapter::new(
            OpenAiCompatibleVariant::OpenRouter,
            ChatCredential::Resolved(handle.clone()),
            model,
            profile::OpenRouterChatProfile::new(configured_base.clone(), served.clone()),
        )?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                handle,
                configured_base,
                abort: AbortSlot::default(),
            }),
            chat,
            served,
        })
    }
}

#[async_trait]
impl StreamingProvider for OpenRouterAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        if !responses_enabled() {
            return self.chat.stream_completion(messages, options).await;
        }
        // Armed first: an abort while the credential resolves cancels this request.
        let cancel = self.inner.abort.arm();
        let resolved = cancel.run(self.inner.handle.current()).await?;
        let (reasoning, vision) = self.served.traits(&options.model);
        let wire = profile::Responses::new(&options, reasoning, vision);
        let url = format!("{}/responses", self.inner.base());
        transport::stream(
            self.inner.clone(),
            &url,
            cancel,
            resolved,
            wire,
            messages,
            options,
        )
        .await
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        self.inner.abort.abort().await;
        self.chat.abort().await
    }
    fn provider_id(&self) -> String {
        self.chat.provider_id()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        self.chat.list_models()
    }
    fn set_discovered_models(&self, models: Vec<ModelDescriptor>) {
        self.served.0.store(Arc::new(Some(models.clone())));
        self.chat.set_discovered_models(models);
    }
    async fn discover_models(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Option<Result<crate::domain::models::provider::DiscoveredModels, ProviderError>> {
        self.chat.discover_models(model_filter, scope_context).await
    }
    fn discovery_credential_fingerprint(&self, context: &str) -> Option<String> {
        self.chat.discovery_credential_fingerprint(context)
    }
    async fn discovery_scope(&self, scope_context: &str) -> Option<String> {
        self.chat.discovery_scope(scope_context).await
    }
    fn default_context_window(&self, model_id: &str) -> Option<u32> {
        self.chat.default_context_window(model_id)
    }
    /// A credential check, not a reachability check: an invalid key is
    /// `Unauthenticated` even though `/models` answers 200.
    async fn health_check(&self) -> Result<(), ProviderError> {
        let (status, _) = self.inner.key_status().await?;
        match status {
            200..=299 => Ok(()),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
            _ => Err(ProviderError::Other(format!(
                "Health check failed: HTTP {status}"
            ))),
        }
    }
    /// Key validation (omp: any non-2xx fails), so a base without `/auth/key`
    /// never stores a key unvalidated.
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        let (status, latency) = self.inner.key_status().await?;
        match status {
            200..=299 => Ok(ProbeOutcome { latency }),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            _ => Err(ProviderError::Other(format!("Probe failed: HTTP {status}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_base_is_normalised_and_never_rebased() {
        assert_eq!(
            base_url(Some("https://gateway.example/api/v1/")),
            "https://gateway.example/api/v1"
        );
        assert_eq!(
            base_url(Some("http://127.0.0.1:9/x")),
            "http://127.0.0.1:9/x"
        );
    }

    #[test]
    fn served_traits_fall_back_to_the_bundled_rows_then_to_none() {
        let served = Served::new();
        assert_eq!(served.traits("no/such-model"), (false, false));
        served.0.store(Arc::new(Some(vec![ModelDescriptor {
            model_id: "a/m".into(),
            display_name: "m".into(),
            provider_id: "openrouter".into(),
            context_window: 1,
            capabilities: [ModelCapability::Thinking, ModelCapability::Vision]
                .into_iter()
                .collect(),
            pricing_tier: None,
            stale: false,
            wire_model_id: None,
        }])));
        assert_eq!(served.traits("a/m"), (true, true));
        assert_eq!(served.traits("a/m:nitro"), (true, true));
        assert_eq!(served.traits("openai/gpt-5.5"), (false, false));
    }
}
