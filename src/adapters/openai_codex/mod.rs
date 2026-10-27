//! ChatGPT subscription Responses transport, adapted from oh-my-pi.
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
pub mod profile;
mod retry;
mod session;

use crate::{
    adapters::{
        anthropic::sse::SseLineBuffer,
        responses::{
            events::{EventError, EventMapper},
            request::ResponsesRequest,
        },
    },
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, ModelDescriptor, StreamChunk},
        ports::{ProbeOutcome, StreamingProvider},
    },
    infrastructure::key_resolver::{CredentialHandle, Resolved},
};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{Response, header::HeaderMap};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

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
    sessions: session::Sessions,
    abort_handle: Mutex<Option<CancellationToken>>,
    discovered_models: arc_swap::ArcSwap<Option<Vec<ModelDescriptor>>>,
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
struct RequestContext {
    resolved: Resolved,
    auth_retried: bool,
    session: session::Snapshot,
    profile: profile::Profile,
    model: String,
    /// Header-less POST carrying the body encoded once per logical request.
    request: reqwest::Request,
    cancel: CancellationToken,
    headers: HeaderMap,
}
struct Opened {
    response: Response,
    deadline: tokio::time::Instant,
}
struct StreamState<S> {
    inner: Arc<Inner>,
    ctx: RequestContext,
    bytes: Option<S>,
    buffer: SseLineBuffer,
    mapper: EventMapper,
    pending: VecDeque<StreamChunk>,
    deadline: tokio::time::Instant,
    retries: u8,
    whitespace_retries: u8,
    finished: bool,
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
                sessions: session::Sessions::default(),
                abort_handle: Mutex::new(None),
                discovered_models: arc_swap::ArcSwap::from_pointee(None),
            }),
        })
    }
}
async fn wait(cancel: &CancellationToken, delay: Duration) -> Result<(), ProviderError> {
    tokio::select! {biased;()=cancel.cancelled()=>Err(ProviderError::Cancelled),()=tokio::time::sleep(delay)=>Ok(())}
}
impl Inner {
    async fn recover(
        &self,
        resolved: &mut Resolved,
        used: &mut bool,
    ) -> Result<bool, ProviderError> {
        if *used {
            return Ok(false);
        }
        *used = true;
        if let Some(refreshed) = self.handle.recover(resolved).await? {
            *resolved = refreshed;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    async fn open(&self, ctx: &mut RequestContext) -> Result<Opened, ProviderError> {
        let mut attempt = 0;
        // Only headers vary between sends: auth after recovery, captured turn state on replay.
        ctx.headers = ctx
            .profile
            .headers(&ctx.session, &ctx.model, &ctx.resolved.auth)?;
        loop {
            let mut request = ctx
                .request
                .try_clone()
                .ok_or_else(|| ProviderError::Other("Could not reuse Codex request body".into()))?;
            *request.headers_mut() = ctx.headers.clone();
            let deadline = tokio::time::Instant::now() + retry::stream_timeout();
            let sent = tokio::select! {biased;()=ctx.cancel.cancelled()=>return Err(ProviderError::Cancelled),result=tokio::time::timeout_at(deadline,self.client.execute(request))=>result};
            let response = match sent {
                Ok(Ok(response)) => response,
                other => {
                    if attempt < 5 {
                        attempt += 1;
                        wait(&ctx.cancel, retry::delay(500 * attempt)).await?;
                        continue;
                    }
                    return Err(match other {
                        Ok(Err(e)) => crate::adapters::provider::classify_reqwest_error(&e),
                        _ => ProviderError::ConnectionFailed(
                            "Codex request timed out before first event".into(),
                        ),
                    });
                }
            };
            let status = response.status().as_u16();
            if response.status().is_success() {
                self.sessions
                    .capture(&mut ctx.session, response.headers())
                    .await;
                return Ok(Opened { response, deadline });
            }
            if status == 401
                && self
                    .recover(&mut ctx.resolved, &mut ctx.auth_retried)
                    .await?
            {
                ctx.headers = ctx
                    .profile
                    .headers(&ctx.session, &ctx.model, &ctx.resolved.auth)?;
                continue;
            }
            let headers = response.headers().clone();
            let body = tokio::select! {biased;()=ctx.cancel.cancelled()=>return Err(ProviderError::Cancelled),body=tokio::time::timeout_at(deadline,response.text())=>body};
            let body = match body {
                Ok(Ok(body)) => body,
                _ => {
                    if attempt < 5 && retry::retry_status(status) {
                        attempt += 1;
                        wait(&ctx.cancel, retry::delay(500 * attempt)).await?;
                        continue;
                    }
                    return Err(ProviderError::ConnectionFailed(
                        "Could not read Codex error response".into(),
                    ));
                }
            };
            let hint = retry::hint(&headers, &body, crate::infrastructure::utils::auth_now_ms());
            if attempt < 5
                && retry::retry_status(status)
                && hint.is_none_or(|v| v <= retry::MAX_DELAY_MS)
            {
                attempt += 1;
                wait(&ctx.cancel, retry::delay(hint.unwrap_or(500 * attempt))).await?;
                continue;
            }
            return Err(errors::http_error(status, &headers, &body, &ctx.headers));
        }
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
        let profile = profile::Profile::new(&session)?;
        // History, options and request-scoped metadata fix the body: encode it once,
        // then reuse it across transport attempts, 401 recovery and in-band replays.
        let body = serde_json::to_vec(&ResponsesRequest::new(&messages, &options, &profile))
            .map_err(|_| ProviderError::Other("Could not encode Codex request".into()))?;
        drop(messages);
        let request = self
            .inner
            .client
            .post(&self.inner.url)
            .body(body)
            .build()
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
        let cancel = CancellationToken::new();
        *self.inner.abort_handle.lock().await = Some(cancel.clone());
        let mut ctx = RequestContext {
            resolved,
            auth_retried: false,
            session,
            profile,
            model: options.model,
            request,
            cancel,
            headers: HeaderMap::new(),
        };
        let opened = self.inner.open(&mut ctx).await?;
        let state = StreamState {
            inner: self.inner.clone(),
            ctx,
            bytes: Some(Box::pin(opened.response.bytes_stream())),
            buffer: SseLineBuffer::new(),
            mapper: EventMapper::new(),
            pending: VecDeque::new(),
            deadline: opened.deadline,
            retries: 0,
            whitespace_retries: 0,
            finished: false,
        };
        let stream = futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((chunk, state));
                }
                if state.finished {
                    return None;
                }
                let received = tokio::select! {biased;()=state.ctx.cancel.cancelled()=>{state.finished=true;state.bytes.take();return Some((StreamChunk::TurnComplete{stop_reason:crate::domain::models::StopReason::Cancelled},state));},value=tokio::time::timeout_at(state.deadline,state.bytes.as_mut().expect("live response").next())=>value};
                let mut failure = None;
                let (frames, eof) = match received {
                    Ok(Some(Ok(bytes))) => (state.buffer.feed(&bytes), false),
                    // Services do not always close the final event with a blank line.
                    Ok(None) => (state.buffer.finish(), true),
                    Ok(Some(Err(_))) => {
                        failure = Some(EventError {
                            message: "Codex transport error reading response body".into(),
                            retryable: true,
                            whitespace_loop: false,
                        });
                        (Vec::new(), false)
                    }
                    Err(_) => {
                        failure = Some(EventError {
                            message: "Codex stream timed out waiting for an event".into(),
                            retryable: true,
                            whitespace_loop: false,
                        });
                        (Vec::new(), false)
                    }
                };
                for frame in frames {
                    if frame.data.trim().is_empty() {
                        continue;
                    }
                    if frame.data.trim() == "[DONE]" {
                        if !state.mapper.terminal() {
                            failure = Some(EventError {
                                message: "Codex stream ended before terminal completion event"
                                    .into(),
                                retryable: true,
                                whitespace_loop: false,
                            });
                        }
                        break;
                    }
                    let value: serde_json::Value = match serde_json::from_str(&frame.data) {
                        Ok(v) => v,
                        Err(_) => {
                            failure = Some(EventError {
                                message: "Could not parse Codex stream event".into(),
                                retryable: true,
                                whitespace_loop: false,
                            });
                            break;
                        }
                    };
                    state.deadline = tokio::time::Instant::now() + retry::stream_timeout();
                    if value["type"] == "response.metadata" {
                        let mut headers = HeaderMap::new();
                        if let Some(values) = value["headers"].as_object() {
                            for key in ["x-codex-turn-state", "x-models-etag"] {
                                if let Some(value) = values
                                    .get(key)
                                    .and_then(|v| v.as_str())
                                    .and_then(|v| v.parse::<reqwest::header::HeaderValue>().ok())
                                {
                                    headers.insert(key, value);
                                }
                            }
                        }
                        state
                            .inner
                            .sessions
                            .capture(&mut state.ctx.session, &headers)
                            .await;
                    }
                    match state.mapper.event(value) {
                        Ok(chunks) => state.pending.extend(chunks),
                        Err(mut error) => {
                            // Only provider-supplied text crosses this redaction boundary.
                            error.message = errors::redact(&error.message, &state.ctx.headers);
                            failure = Some(error);
                            break;
                        }
                    }
                    if state.mapper.terminal() {
                        state.finished = true;
                        state.bytes.take();
                        break;
                    }
                }
                if eof && failure.is_none() && !state.finished {
                    failure = Some(EventError {
                        message: "Codex stream ended before terminal completion event".into(),
                        retryable: true,
                        whitespace_loop: false,
                    });
                }
                if let Some(error) = failure {
                    state.bytes.take();
                    let replay = !state.mapper.committed()
                        && if error.whitespace_loop {
                            state.whitespace_retries < 2
                        } else {
                            error.retryable && state.retries < 5
                        };
                    if replay {
                        state.pending.clear();
                        let delay = if error.whitespace_loop {
                            state.whitespace_retries += 1;
                            250 * u64::from(state.whitespace_retries)
                        } else {
                            state.retries += 1;
                            500 * u64::from(state.retries)
                        };
                        let reopened = match wait(&state.ctx.cancel, retry::delay(delay)).await {
                            Ok(()) => state.inner.open(&mut state.ctx).await,
                            Err(e) => Err(e),
                        };
                        match reopened {
                            Ok(opened) => {
                                state.bytes = Some(Box::pin(opened.response.bytes_stream()));
                                state.deadline = opened.deadline;
                                state.buffer = SseLineBuffer::new();
                                state.mapper = EventMapper::new();
                                continue;
                            }
                            // An abort during backoff or reopen ends like any other abort.
                            Err(ProviderError::Cancelled) => {
                                state.finished = true;
                                state.pending.push_back(StreamChunk::TurnComplete {
                                    stop_reason: crate::domain::models::StopReason::Cancelled,
                                });
                            }
                            Err(error) => {
                                state.finished = true;
                                state.pending.push_back(StreamChunk::Error {
                                    content: error.to_string(),
                                });
                            }
                        }
                    } else {
                        state.finished = true;
                        state.pending.push_back(StreamChunk::Error {
                            content: error.message,
                        });
                    }
                }
            }
        });
        Ok(Box::pin(stream))
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        if let Some(cancel) = self.inner.abort_handle.lock().await.take() {
            cancel.cancel();
        }
        Ok(())
    }
    fn provider_id(&self) -> String {
        "openai-codex".into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        if let Some(models) = &**self.inner.discovered_models.load() {
            return models.clone();
        }
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|catalog| catalog.providers.get("openai-codex"))
            .map(|entry| entry.models.iter().map(|m| m.descriptor.clone()).collect())
            .unwrap_or_default()
    }
    fn set_discovered_models(&self, models: Vec<ModelDescriptor>) {
        self.inner.discovered_models.store(Arc::new(Some(models)));
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
