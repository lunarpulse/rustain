//! Google Cloud Code Assist (Gemini CLI subscription) adapter: native
//! `v1internal:streamGenerateContent` over SSE, adapted from oh-my-pi
//! (ai/src/providers/google-gemini-cli.ts, utils/src/fetch-retry.ts,
//! utils/src/stream.ts, ai/src/utils/idle-iterator.ts).
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

mod collapse;
mod discovery;
mod errors;
mod events;
pub(crate) mod profile;
mod request;
mod retry;
mod schema;

use crate::{
    adapters::{
        anthropic::sse::SseLineBuffer,
        responses::transport::{AbortSlot, Armed, recover_once},
    },
    domain::{
        errors::ProviderError,
        models::{
            CompletionOptions, Message, ModelDescriptor, ResolvedAuth, StopReason, StreamChunk,
            provider::DiscoveredModels,
        },
        ports::{ProbeOutcome, StreamingProvider},
    },
    infrastructure::key_resolver::{CredentialHandle, Resolved},
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderValue};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;

/// Provider kind and replay stamp of captured Gemini parts.
pub(crate) const PROVIDER_KIND: &str = "google-gemini-cli";
/// Payload key of a Gemini replay item: `{"gemini_parts": [...]}`.
pub(crate) const REPLAY_KEY: &str = "gemini_parts";
/// omp `DEFAULT_ENDPOINT`.
pub const DEFAULT_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";

const MISSING_PROJECT: &str = "Missing token or projectId in Google Cloud credentials. Run `rustain auth login google-gemini-cli` to re-authenticate.";
const FIRST_EVENT_TIMEOUT: &str =
    "Cloud Code Assist stream timed out while waiting for the first event";
const INVALID_JSON: &str = "Cloud Code Assist stream returned invalid JSON";
const EMPTY: &str = "Cloud Code Assist API returned an empty response";
const BODY_TRANSPORT: &str = "Cloud Code Assist transport error reading response body";
/// omp `loadCodeAssist` body without a project (probe and project-less discovery).
const LOAD_CODE_ASSIST_BODY: &str = r#"{"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#;

/// The API base without a trailing `/`: the configured one, else the default.
pub fn base_url(configured: Option<&str>) -> String {
    configured
        .map(|base| base.trim().trim_end_matches('/'))
        .filter(|base| !base.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crate::adapters::auth::policies::endpoint(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_owned()
        })
}

/// One consolidated exposure: the bearer header of every Cloud Code Assist request,
/// marked sensitive. Redaction reads it back from the sent header map.
fn bearer(auth: &ResolvedAuth) -> Result<HeaderValue, ProviderError> {
    let secret = auth.bearer_secret().expose_secret();
    if secret.trim().is_empty() {
        return Err(ProviderError::AuthenticationFailed);
    }
    let mut value = HeaderValue::try_from(format!("Bearer {secret}"))
        .map_err(|_| ProviderError::Other("Invalid Google Cloud credential".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The project of the grant whose bearer is sent; never read from the environment.
fn project(resolved: &Resolved) -> Result<&str, ProviderError> {
    match &resolved.auth {
        ResolvedAuth::OAuth {
            project_id: Some(project),
            ..
        } if !project.trim().is_empty() => Ok(project),
        _ => Err(ProviderError::Other(MISSING_PROJECT.into())),
    }
}

/// The stored login email of the grant; Gemini rows carry no account id, so the
/// resolver's identity key is `email:<email>` whenever an email exists.
fn email(resolved: &Resolved) -> Option<&str> {
    resolved.identity.as_deref()?.strip_prefix("email:")
}

/// omp `getGeminiCliHeaders(model)` plus the request headers of P:594-605.
fn headers(auth: &ResolvedAuth, model: &str, stream: bool) -> Result<HeaderMap, ProviderError> {
    use crate::adapters::auth::gemini_cli_project::{CLIENT_METADATA, user_agent};
    let mut headers = HeaderMap::new();
    headers.insert("authorization", bearer(auth)?);
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    if stream {
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    }
    headers.insert(
        "user-agent",
        HeaderValue::try_from(user_agent(model))
            .map_err(|_| ProviderError::Other("Invalid Gemini CLI model id".into()))?,
    );
    headers.insert("client-metadata", HeaderValue::from_static(CLIENT_METADATA));
    Ok(headers)
}

struct Inner {
    client: reqwest::Client,
    handle: CredentialHandle,
    base: String,
    abort: AbortSlot,
    discovered_models: arc_swap::ArcSwap<Option<Vec<ModelDescriptor>>>,
}

impl Inner {
    fn seed(&self) -> &'static [crate::adapters::model_catalog_cache::CachedModelEntry] {
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|catalog| catalog.providers.get(PROVIDER_KIND))
            .map(|entry| entry.models.as_slice())
            .unwrap_or_default()
    }

    /// The wire profile of `model`, read against the served catalogue (R3/R4): a
    /// served row naming a wire id is a collapsed one.
    fn profile(&self, model: &str) -> profile::GeminiModelProfile {
        use crate::domain::models::provider::ModelCapability;
        let served = self.discovered_models.load();
        let row = (**served)
            .as_deref()
            .and_then(|models| models.iter().find(|row| row.model_id == model));
        profile::resolve(
            model,
            match row {
                None => profile::Served::Absent,
                Some(row) => match row.wire_model_id.as_deref() {
                    Some(wire) => profile::Served::Collapsed {
                        wire,
                        reasoning: row.capabilities.contains(&ModelCapability::Thinking),
                    },
                    None => profile::Served::Row,
                },
            },
        )
    }

    /// `POST <base>/v1internal:loadCodeAssist`, status only; a stored row's 401
    /// recovers once.
    async fn probe_status(&self) -> Result<(u16, std::time::Duration), ProviderError> {
        let start = std::time::Instant::now();
        let mut resolved = self.handle.current().await?;
        let mut retried = false;
        let url = format!("{}/v1internal:loadCodeAssist", self.base);
        let body = LOAD_CODE_ASSIST_BODY;
        loop {
            let headers = headers(
                &resolved.auth,
                crate::adapters::auth::gemini_cli_project::DEFAULT_HEADER_MODEL,
                false,
            )?;
            let response = self
                .client
                .post(&url)
                .headers(headers)
                .body(body)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
            let status = response.status().as_u16();
            drop(response);
            if status == 401 && recover_once(&self.handle, &mut resolved, &mut retried).await? {
                continue;
            }
            return Ok((status, start.elapsed()));
        }
    }
}

pub struct GeminiCliAdapter {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for GeminiCliAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiCliAdapter")
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl GeminiCliAdapter {
    /// `base` is the configured `base_url` or the default endpoint ([`base_url`]).
    pub fn new(handle: CredentialHandle, base: String) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::ConnectionFailed("Could not create Gemini CLI HTTP client".into())
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                handle,
                base: base.trim_end_matches('/').to_owned(),
                abort: AbortSlot::default(),
                discovered_models: arc_swap::ArcSwap::from_pointee(None),
            }),
        })
    }
}

/// One logical request: history, options and the grant that is sent. The body is
/// encoded once per project; a recovery that changes the project re-encodes it.
struct Call {
    inner: Arc<Inner>,
    url: String,
    messages: Vec<Message>,
    options: CompletionOptions,
    profile: profile::GeminiModelProfile,
    tool_names: HashSet<String>,
    resolved: Resolved,
    auth_retried: bool,
    /// The header-less POST carrying the body encoded for the current project.
    request: Option<reqwest::Request>,
    headers: HeaderMap,
    cancel: Armed,
}

impl Call {
    fn encode(&mut self) -> Result<(), ProviderError> {
        let body = request::encode(
            &self.messages,
            &self.options,
            &self.profile,
            project(&self.resolved)?,
        )
        .map_err(|_| ProviderError::Other("Could not encode Cloud Code Assist request".into()))?;
        self.request = Some(
            self.inner
                .client
                .post(&self.url)
                .body(body)
                .build()
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?,
        );
        self.headers = headers(&self.resolved.auth, &self.options.model, true)?;
        Ok(())
    }

    async fn wait(&self, delay: Duration) -> Result<(), ProviderError> {
        tokio::select! {biased;()=self.cancel.cancelled()=>Err(ProviderError::Cancelled),()=tokio::time::sleep(delay)=>Ok(())}
    }

    /// omp `fetchWithRetry` under one pre-response watchdog that spans every
    /// attempt and backoff until headers arrive. `attempts` is 1 for the plain
    /// empty-response re-POST.
    async fn open(&mut self, attempts: u32) -> Result<reqwest::Response, ProviderError> {
        let watchdog = Instant::now() + retry::timeout(self.profile.first_event_ms);
        let timed_out = || ProviderError::ConnectionFailed(FIRST_EVENT_TIMEOUT.into());
        let mut attempt = 0;
        loop {
            // Cloning a built request shares its encoded body.
            let mut request = self
                .request
                .as_ref()
                .and_then(reqwest::Request::try_clone)
                .ok_or_else(|| {
                    ProviderError::Other("Could not reuse Cloud Code Assist request body".into())
                })?;
            *request.headers_mut() = self.headers.clone();
            let sent = tokio::select! {biased;()=self.cancel.cancelled()=>return Err(ProviderError::Cancelled),sent=tokio::time::timeout_at(watchdog,self.inner.client.execute(request))=>sent};
            let response = match sent {
                Err(_) => return Err(timed_out()),
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    if attempt + 1 >= attempts {
                        return Err(crate::adapters::provider::classify_reqwest_error(&error));
                    }
                    self.backoff(retry::default_delay_ms(attempt), watchdog)
                        .await?;
                    attempt += 1;
                    continue;
                }
            };
            let status = response.status().as_u16();
            if response.status().is_success() {
                return Ok(response);
            }
            // A recovery may refresh over the network: an abort ends it promptly.
            if status == 401
                && self
                    .cancel
                    .run(recover_once(
                        &self.inner.handle,
                        &mut self.resolved,
                        &mut self.auth_retried,
                    ))
                    .await?
            {
                // The recovered grant supplies both the bearer and the project.
                self.encode()?;
                continue;
            }
            let headers = response.headers().clone();
            let body = tokio::select! {biased;()=self.cancel.cancelled()=>return Err(ProviderError::Cancelled),body=tokio::time::timeout_at(watchdog,response.text())=>body};
            let body = match body {
                Err(_) => return Err(timed_out()),
                Ok(body) => body.unwrap_or_default(),
            };
            if retry::retry_status(status) && attempt + 1 < attempts {
                let hint = crate::adapters::responses::retry::hint(
                    &headers,
                    &body,
                    crate::infrastructure::utils::auth_now_ms(),
                );
                if hint.is_none_or(|hint| hint <= retry::MAX_DELAY_MS) {
                    let delay = hint
                        .unwrap_or_else(|| retry::default_delay_ms(attempt))
                        .min(retry::MAX_DELAY_MS);
                    self.backoff(delay, watchdog).await?;
                    attempt += 1;
                    continue;
                }
            }
            return Err(errors::http_error(
                status,
                &headers,
                &body,
                &self.headers,
                email(&self.resolved),
            ));
        }
    }

    /// A backoff wait is bounded by the pre-response watchdog and is cancellable.
    async fn backoff(&self, ms: u64, watchdog: Instant) -> Result<(), ProviderError> {
        tokio::select! {biased;()=self.cancel.cancelled()=>Err(ProviderError::Cancelled),slept=tokio::time::timeout_at(watchdog,tokio::time::sleep(retry::delay(ms)))=>slept.map_err(|_|ProviderError::ConnectionFailed(FIRST_EVENT_TIMEOUT.into()))}
    }

    fn mapper(&self) -> events::Mapper {
        events::Mapper::new(
            &self.options.model,
            self.profile.flash_leak,
            self.tool_names.clone(),
        )
    }
}

struct StreamState {
    call: Call,
    response: Option<reqwest::Response>,
    buffer: SseLineBuffer,
    mapper: events::Mapper,
    pending: VecDeque<StreamChunk>,
    /// The first-SSE-event allowance of this attempt; `None` once an event arrived.
    /// Gemini CLI has no idle timeout after the first event.
    first_event: Option<Instant>,
    empty_retries: u8,
    finished: bool,
}

impl StreamState {
    fn fail(&mut self, message: String) {
        self.finished = true;
        self.response.take();
        let message = errors::redact(&message, &self.call.headers);
        self.pending
            .push_back(StreamChunk::Error { content: message });
    }

    async fn pull(&mut self) -> Option<StreamChunk> {
        loop {
            if let Some(chunk) = self.pending.pop_front() {
                return Some(chunk);
            }
            if self.finished {
                return None;
            }
            let deadline = self.first_event;
            let cancel = &self.call.cancel;
            let response = &mut self.response;
            let received = tokio::select! {biased;
                ()=cancel.cancelled()=>None,
                read=async {
                    match (response.as_mut(), deadline) {
                        (None, _) => Ok(Ok(None)),
                        (Some(response), Some(deadline)) => tokio::time::timeout_at(deadline, response.chunk()).await,
                        (Some(response), None) => Ok(response.chunk().await),
                    }
                }=>Some(read),
            };
            let Some(received) = received else {
                self.finished = true;
                self.response.take();
                return Some(StreamChunk::TurnComplete {
                    stop_reason: StopReason::Cancelled,
                });
            };
            let (frames, eof) = match received {
                Ok(Ok(None)) => (self.buffer.finish(), true),
                Ok(Ok(Some(bytes))) => (self.buffer.feed(&bytes), false),
                Ok(Err(_)) => {
                    self.fail(BODY_TRANSPORT.into());
                    continue;
                }
                Err(_) => {
                    self.fail(FIRST_EVENT_TIMEOUT.into());
                    continue;
                }
            };
            let mut ended = eof;
            let mut failure = None;
            for frame in frames {
                let data = frame.data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    ended = true;
                    break;
                }
                let value: serde_json::Value = match serde_json::from_str(data) {
                    Ok(value) => value,
                    // A container-shaped tail flushed at EOF is a cut-off stream end.
                    Err(_) if eof && data.starts_with(['{', '[']) => {
                        ended = true;
                        break;
                    }
                    Err(_) => {
                        failure = Some(INVALID_JSON.to_owned());
                        break;
                    }
                };
                self.first_event = None;
                match self.mapper.frame(&value) {
                    Ok(chunks) => self.pending.extend(chunks),
                    Err(message) => {
                        failure = Some(message);
                        break;
                    }
                }
            }
            if let Some(message) = failure {
                self.fail(message);
                continue;
            }
            if !ended {
                continue;
            }
            self.response.take();
            match self.mapper.finish() {
                events::Finish::Done(chunks) => {
                    self.pending.extend(chunks);
                    self.finished = true;
                }
                events::Finish::Error { pending, message } => {
                    self.pending.extend(pending);
                    self.fail(message);
                }
                events::Finish::Empty if self.empty_retries < retry::EMPTY_RETRIES => {
                    self.empty_retries += 1;
                    let delay = retry::delay(retry::empty_delay_ms(self.empty_retries));
                    let reopened = match self.call.wait(delay).await {
                        Ok(()) => self.call.open(1).await,
                        Err(error) => Err(error),
                    };
                    match reopened {
                        Ok(response) => self.start(response),
                        Err(ProviderError::Cancelled) => {
                            self.finished = true;
                            self.pending.push_back(StreamChunk::TurnComplete {
                                stop_reason: StopReason::Cancelled,
                            });
                        }
                        Err(error) => self.fail(error.to_string()),
                    }
                }
                events::Finish::Empty => self.fail(EMPTY.into()),
            }
        }
    }

    /// A fresh attempt: its own SSE buffer, mapper and first-event allowance.
    fn start(&mut self, response: reqwest::Response) {
        self.response = Some(response);
        self.buffer = SseLineBuffer::new();
        self.mapper = self.call.mapper();
        self.first_event = Some(Instant::now() + retry::timeout(self.call.profile.first_event_ms));
    }
}

#[async_trait]
impl StreamingProvider for GeminiCliAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        // Armed first: an abort while the credential resolves cancels this request.
        let cancel = self.inner.abort.arm();
        let resolved = cancel.run(self.inner.handle.current()).await?;
        // Checked before any POST.
        project(&resolved)?;
        let profile = self.inner.profile(&options.model);
        let tool_names = options.tools.iter().map(|tool| tool.name.clone()).collect();
        let mut call = Call {
            inner: self.inner.clone(),
            url: format!(
                "{}/v1internal:streamGenerateContent?alt=sse",
                self.inner.base
            ),
            messages,
            options,
            profile,
            tool_names,
            resolved,
            auth_retried: false,
            request: None,
            headers: HeaderMap::new(),
            cancel,
        };
        call.encode()?;
        let response = call.open(retry::ATTEMPTS).await?;
        let mapper = call.mapper();
        let mut state = StreamState {
            call,
            response: None,
            buffer: SseLineBuffer::new(),
            mapper,
            pending: VecDeque::new(),
            first_event: None,
            empty_retries: 0,
            finished: false,
        };
        state.start(response);
        Ok(Box::pin(futures::stream::unfold(
            state,
            |mut state| async move {
                let chunk = state.pull().await?;
                Some((chunk, state))
            },
        )))
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
    ) -> Option<Result<DiscoveredModels, ProviderError>> {
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
    async fn health_check(&self) -> Result<(), ProviderError> {
        let (status, _) = tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
            .await
            .map_err(|_| {
                ProviderError::ConnectionFailed("Cloud Code Assist health check timed out".into())
            })??;
        match status {
            200..=299 => Ok(()),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
            _ => Err(ProviderError::Other(format!(
                "Cloud Code Assist loadCodeAssist HTTP {status}"
            ))),
        }
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        let (status, latency) =
            tokio::time::timeout(Duration::from_secs(5), self.inner.probe_status())
                .await
                .map_err(|_| ProviderError::Offline("Cloud Code Assist probe timed out".into()))?
                .map_err(|e| {
                    if matches!(
                        e,
                        ProviderError::ConnectionFailed(_) | ProviderError::Offline(_)
                    ) {
                        ProviderError::Offline("Cloud Code Assist probe transport failed".into())
                    } else {
                        e
                    }
                })?;
        match status {
            200..=299 => Ok(ProbeOutcome { latency }),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            // 404/405 included: the probe endpoint must exist for this provider.
            _ => Err(ProviderError::Other(format!(
                "Cloud Code Assist loadCodeAssist HTTP {status}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapters::responses::transport::test_support::{
            accept, loopback, options, reject, stored_oauth, user, within,
        },
        domain::models::CredentialIdentity,
    };
    use futures::FutureExt;

    fn identity() -> CredentialIdentity {
        CredentialIdentity {
            email: Some("user@example.com".into()),
            project_id: Some("project".into()),
            ..CredentialIdentity::default()
        }
    }

    fn spawn_stream(
        adapter: &Arc<GeminiCliAdapter>,
    ) -> tokio::task::JoinHandle<Result<(), ProviderError>> {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            adapter
                .stream_completion(vec![user("hi")], options("gemini-2.5-pro"))
                .await
                .map(drop)
        })
    }

    /// Keystone (a): an abort while the stored grant refreshes cancels the request.
    #[tokio::test]
    async fn abort_while_the_credential_resolves_cancels_the_stream() {
        let (handle, refresh, _dir) = stored_oauth(PROVIDER_KIND, true, identity()).await;
        let (listener, origin) = loopback().await;
        let adapter = Arc::new(GeminiCliAdapter::new(handle, origin).unwrap());
        let task = spawn_stream(&adapter);
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

    /// Keystone (b): an abort during a 401 refresh ends the open without a resend.
    #[tokio::test]
    async fn abort_during_401_recovery_cancels_before_the_resend() {
        let (handle, refresh, _dir) = stored_oauth(PROVIDER_KIND, false, identity()).await;
        let (listener, origin) = loopback().await;
        let adapter = Arc::new(GeminiCliAdapter::new(handle, origin).unwrap());
        let task = spawn_stream(&adapter);
        let mut socket = within(accept(&listener)).await;
        reject(&mut socket).await;
        within(refresh.entered.notified()).await;
        adapter.abort().await.unwrap();
        let result = within(task).await.unwrap();
        assert!(
            matches!(result, Err(ProviderError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(refresh.calls(), 1);
        assert!(
            listener.accept().now_or_never().is_none(),
            "the rejected request is never resent"
        );
        assert_eq!(adapter.inner.abort.live(), 0);
    }

    #[test]
    fn configured_base_wins_and_trailing_slashes_are_trimmed() {
        assert_eq!(
            base_url(Some("https://proxy.example/root//")),
            "https://proxy.example/root"
        );
        assert_eq!(base_url(Some("  ")), base_url(None));
    }
}
