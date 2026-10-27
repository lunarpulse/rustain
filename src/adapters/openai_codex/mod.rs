//! ChatGPT subscription Responses adapter, adapted from oh-my-pi.
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

mod discovery;
mod errors;
pub mod profile;
mod session;

use crate::{
    adapters::responses::transport::{self, AbortSlot, Host},
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, ModelDescriptor, StreamChunk},
        ports::{ProbeOutcome, StreamingProvider},
    },
    infrastructure::key_resolver::{CredentialHandle, Resolved},
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// The adapter kind: replay items are stamped with it and replayed only to it.
const PROVIDER_KIND: &str = "openai-codex";

/// Normalise a configured API base or explicit Codex response URL once.
pub fn responses_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/codex/responses") {
        base.to_owned()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}
struct Inner {
    client: reqwest::Client,
    handle: CredentialHandle,
    url: String,
    sessions: Arc<session::Sessions>,
    abort: AbortSlot,
    discovered_models: arc_swap::ArcSwap<Option<Vec<ModelDescriptor>>>,
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
pub struct CodexAdapter {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for CodexAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAdapter")
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}
impl CodexAdapter {
    pub fn new(
        handle: CredentialHandle,
        _model: String,
        base: String,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::ConnectionFailed("Could not create Codex HTTP client".into())
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                handle,
                url: responses_url(&base),
                sessions: Arc::default(),
                abort: AbortSlot::default(),
                discovered_models: arc_swap::ArcSwap::from_pointee(None),
            }),
        })
    }
}
impl Inner {
    async fn recover(
        &self,
        resolved: &mut Resolved,
        used: &mut bool,
    ) -> Result<bool, ProviderError> {
        transport::recover_once(&self.handle, resolved, used).await
    }
    async fn probe_status(&self) -> Result<(u16, Duration), ProviderError> {
        let start = Instant::now();
        let mut resolved = self.handle.current().await?;
        let mut auth_retried = false;
        let url = format!(
            "{}/models?client_version={}",
            self.url.strip_suffix("/responses").unwrap_or(&self.url),
            profile::CODEX_VERSION
        );
        loop {
            let mut headers = profile::auth_headers(&resolved.auth, false)?;
            headers.insert(
                "accept",
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            let response = self
                .client
                .get(&url)
                .headers(headers)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
            let status = response.status().as_u16();
            drop(response);
            if status == 401 && self.recover(&mut resolved, &mut auth_retried).await? {
                continue;
            }
            return Ok((status, start.elapsed()));
        }
    }
}

#[async_trait]
impl StreamingProvider for CodexAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let resolved = self.inner.handle.current().await?;
        let session = self.inner.sessions.begin(&messages, &options).await;
        let profile = profile::Profile::new(session, self.inner.sessions.clone())?;
        transport::stream(
            self.inner.clone(),
            &self.inner.url,
            resolved,
            profile,
            messages,
            options,
        )
        .await
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        self.inner.abort.abort().await;
        Ok(())
    }
    fn provider_id(&self) -> String {
        PROVIDER_KIND.into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        if let Some(models) = &**self.inner.discovered_models.load() {
            return models.clone();
        }
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|catalog| catalog.providers.get(PROVIDER_KIND))
            .map(|entry| entry.models.iter().map(|m| m.descriptor.clone()).collect())
            .unwrap_or_default()
    }
    fn set_discovered_models(&self, models: Vec<ModelDescriptor>) {
        self.inner.discovered_models.store(Arc::new(Some(models)));
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
    fn default_context_window(&self, model_id: &str) -> Option<u32> {
        Some(discovery::default_context_window(model_id))
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        let (status, _) = tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
            .await
            .map_err(|_| {
                ProviderError::ConnectionFailed("Codex health check timed out".into())
            })??;
        match status {
            200..=299 => Ok(()),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
            _ => Err(ProviderError::Other(format!("Codex models HTTP {status}"))),
        }
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        let (status, latency) =
            tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
                .await
                .map_err(|_| ProviderError::Offline("Codex probe timed out".into()))?
                .map_err(|e| {
                    if matches!(
                        e,
                        ProviderError::ConnectionFailed(_) | ProviderError::Offline(_)
                    ) {
                        ProviderError::Offline("Codex probe transport failed".into())
                    } else {
                        e
                    }
                })?;
        match status {
            200..=299 => Ok(ProbeOutcome { latency }),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            _ => Err(ProviderError::Other(format!("Codex models HTTP {status}"))),
        }
    }
}
