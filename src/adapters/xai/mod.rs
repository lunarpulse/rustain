//! xAI Responses adapter (`xai` API key, `xai-oauth` subscription), adapted from
//! oh-my-pi (ai/src/providers/openai-responses.ts, xai-base-url.ts).
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

use crate::{
    adapters::responses::transport::{self, AbortSlot, Host},
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, ModelCapability, ModelDescriptor, StreamChunk},
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

/// omp `XAI_DEFAULT_BASE_URL`, shared by both kinds.
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// The two xAI provider kinds; they share the wire and differ in credentials,
/// base-URL rule, catalogue and output limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XaiKind {
    /// `xai`: API key (`XAI_API_KEY` or a stored key).
    Xai,
    /// `xai-oauth`: SuperGrok / X Premium+ device login (`XAI_OAUTH_TOKEN` or a stored row).
    XaiOauth,
}

impl XaiKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Xai => "xai",
            Self::XaiOauth => "xai-oauth",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "xai" => Some(Self::Xai),
            "xai-oauth" => Some(Self::XaiOauth),
            _ => None,
        }
    }
}

/// The API base of one request, without a trailing `/`: a configured base that
/// differs from the default wins; otherwise `xai` follows `XAI_BASE_URL`;
/// otherwise the default. `xai-oauth` never follows `XAI_BASE_URL`, so the
/// request origin, the stored-OAuth origin check and the cache scope agree.
pub fn base_url(kind: XaiKind, configured: Option<&str>) -> String {
    if let Some(base) = configured
        .map(|base| base.trim_end_matches('/'))
        .filter(|base| !base.is_empty() && *base != DEFAULT_BASE_URL)
    {
        return base.to_owned();
    }
    if kind == XaiKind::Xai
        && let Some(base) = crate::infrastructure::utils::env_var_trimmed("XAI_BASE_URL")
    {
        let base = base.trim_end_matches('/');
        if !base.is_empty() {
            return base.to_owned();
        }
    }
    crate::adapters::auth::policies::endpoint(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_owned()
}

struct Inner {
    client: reqwest::Client,
    handle: CredentialHandle,
    kind: XaiKind,
    configured_base: Option<String>,
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

impl Inner {
    /// Resolved per request: `XAI_BASE_URL` and the test origin are read at each use.
    fn base(&self) -> String {
        base_url(self.kind, self.configured_base.as_deref())
    }
    async fn recover(
        &self,
        resolved: &mut Resolved,
        used: &mut bool,
    ) -> Result<bool, ProviderError> {
        transport::recover_once(&self.handle, resolved, used).await
    }
    /// The bundled rows of this kind.
    fn seed(&self) -> &'static [crate::adapters::model_catalog_cache::CachedModelEntry] {
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|catalog| catalog.providers.get(self.kind.id()))
            .map(|entry| entry.models.as_slice())
            .unwrap_or_default()
    }
    /// `(reasoning, vision)` of the served catalogue row; unknown ids have neither.
    fn traits(&self, model: &str) -> (bool, bool) {
        let of = |descriptor: &ModelDescriptor| {
            (
                descriptor.capabilities.contains(&ModelCapability::Thinking),
                descriptor.capabilities.contains(&ModelCapability::Vision),
            )
        };
        if let Some(models) = &**self.discovered_models.load() {
            return models
                .iter()
                .find(|m| m.model_id == model)
                .map(of)
                .unwrap_or_default();
        }
        self.seed()
            .iter()
            .map(|entry| &entry.descriptor)
            .find(|m| m.model_id == model)
            .map(of)
            .unwrap_or_default()
    }
    /// `GET <base>/models`, status only; a stored row's 401 recovers once.
    async fn probe_status(&self) -> Result<(u16, Duration), ProviderError> {
        let start = Instant::now();
        let mut resolved = self.handle.current().await?;
        let mut auth_retried = false;
        let url = format!("{}/models", self.base());
        loop {
            let mut headers = profile::auth_headers(&resolved.auth)?;
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

pub struct XaiAdapter {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for XaiAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XaiAdapter")
            .field("kind", &self.inner.kind)
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl XaiAdapter {
    pub fn new(
        handle: CredentialHandle,
        kind: XaiKind,
        configured_base: Option<String>,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::ConnectionFailed("Could not create xAI HTTP client".into())
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                handle,
                kind,
                configured_base,
                abort: AbortSlot::default(),
                discovered_models: arc_swap::ArcSwap::from_pointee(None),
            }),
        })
    }
}

#[async_trait]
impl StreamingProvider for XaiAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        // Armed first: an abort while the credential resolves cancels this request.
        let cancel = self.inner.abort.arm();
        let resolved = cancel.run(self.inner.handle.current()).await?;
        let (reasoning, vision) = self.inner.traits(&options.model);
        let profile = profile::Profile::new(self.inner.kind, &options, reasoning, vision);
        let url = format!("{}/responses", self.inner.base());
        transport::stream(
            self.inner.clone(),
            &url,
            cancel,
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
        self.inner.kind.id().into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        if let Some(models) = &**self.inner.discovered_models.load() {
            return models.clone();
        }
        self.inner
            .seed()
            .iter()
            .map(|entry| entry.descriptor.clone())
            .collect()
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
            .map_err(|_| ProviderError::ConnectionFailed("xAI health check timed out".into()))??;
        match status {
            200..=299 => Ok(()),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
            _ => Err(ProviderError::Other(format!("xAI models HTTP {status}"))),
        }
    }
    /// 404/405 is `EndpointUnsupported`, as for the other API-key providers, so an
    /// API-key login against a base without `/models` stores the key unvalidated.
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        let (status, latency) =
            tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
                .await
                .map_err(|_| ProviderError::Offline("xAI probe timed out".into()))?
                .map_err(|e| {
                    if matches!(
                        e,
                        ProviderError::ConnectionFailed(_) | ProviderError::Offline(_)
                    ) {
                        ProviderError::Offline("xAI probe transport failed".into())
                    } else {
                        e
                    }
                })?;
        match status {
            200..=299 => Ok(ProbeOutcome { latency }),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
            _ => Err(ProviderError::Other(format!("xAI models HTTP {status}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_ids_round_trip() {
        for kind in [XaiKind::Xai, XaiKind::XaiOauth] {
            assert_eq!(XaiKind::from_id(kind.id()), Some(kind));
        }
        assert_eq!(XaiKind::from_id("openai"), None);
    }

    #[test]
    fn a_configured_non_default_base_wins_for_both_kinds() {
        for kind in [XaiKind::Xai, XaiKind::XaiOauth] {
            assert_eq!(
                base_url(kind, Some("https://proxy.example/v1//")),
                "https://proxy.example/v1"
            );
        }
    }

    /// Keystone (DF-20-1D-RESPONSES-ABORT-WINDOW, review RF-05): an abort while
    /// the stored grant refreshes cancels the request before anything is sent.
    #[tokio::test]
    async fn abort_while_the_credential_resolves_cancels_the_stream() {
        use crate::adapters::responses::transport::test_support::{
            loopback, options, stored_oauth, user, within,
        };
        use futures::FutureExt;

        let (handle, refresh, _dir) = stored_oauth(
            XaiKind::XaiOauth.id(),
            true,
            crate::domain::models::CredentialIdentity::default(),
        )
        .await;
        let (listener, origin) = loopback().await;
        let adapter = Arc::new(XaiAdapter::new(handle, XaiKind::XaiOauth, Some(origin)).unwrap());
        let task = tokio::spawn({
            let adapter = adapter.clone();
            async move {
                adapter
                    .stream_completion(vec![user("hi")], options("grok-test"))
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
