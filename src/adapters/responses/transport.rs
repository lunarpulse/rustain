//! Shared Responses HTTP/SSE transport, adapted from oh-my-pi
//! (openai-codex-responses.ts; generic policy: openai-responses.ts,
//! utils/openai-http.ts, utils/empty-completion-retry.ts).
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

use super::{
    Dialect, WireProfile,
    events::{self, EventError, EventMapper},
    request::ResponsesRequest,
    retry,
};
use crate::{
    adapters::{anthropic::sse::SseLineBuffer, provider::request_profile::Alternate},
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, ResolvedAuth, StopReason, StreamChunk},
    },
    infrastructure::key_resolver::{CredentialHandle, Resolved},
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::{
    Response,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Adapter-lifetime state the transport borrows for every logical request.
pub trait Host: Send + Sync + 'static {
    /// A client built without redirects.
    fn client(&self) -> &reqwest::Client;
    fn credentials(&self) -> &CredentialHandle;
    /// The registry each request's token is armed from before its credential resolves.
    fn abort_slot(&self) -> &AbortSlot;
}

/// Request-scoped provider half: body policy (`WireProfile`), headers, error
/// mapping and redaction. The transport owns attempts, framing and timeouts.
#[async_trait]
pub trait Wire: WireProfile + Send + Sync + 'static {
    /// Every header of one send. Rebuilt before each open and after a 401 recovery.
    fn headers(&self, model: &str, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError>;
    /// Headers of a successful response and, under the Codex policy, of
    /// `response.metadata` events.
    async fn capture(&mut self, _headers: &HeaderMap) {}
    /// A non-retried HTTP failure; `request` holds the headers that were sent.
    fn http_error(
        &self,
        status: u16,
        headers: &HeaderMap,
        body: &str,
        request: &HeaderMap,
    ) -> ProviderError;
    /// Removes credential material of `request` from provider-supplied text.
    fn redact(&self, message: &str, request: &HeaderMap) -> String;
    /// A 401 recovery replaced the credential: request-scoped state keyed by it
    /// restarts, and `Some(url)` re-derives an endpoint that depends on it.
    fn recovered(&mut self, _resolved: &Resolved) -> Result<Option<String>, ProviderError> {
        Ok(None)
    }
    /// At most one header-only resend of a denied (400/403) attempt, decided
    /// from this request's state only; `sent` holds the denied attempt's headers.
    fn alternate(&mut self, _status: u16, _body: &str, _sent: &HeaderMap) -> Option<Alternate> {
        None
    }
}

/// What a denied attempt's identity alternate produced.
enum Resent {
    Opened(Opened),
    Answer {
        status: u16,
        headers: HeaderMap,
        body: String,
    },
    /// The resend got no answer (`None`: its deadline passed); it settled nothing.
    Unanswered(Option<reqwest::Error>),
}

/// The cancellation tokens of the adapter's in-flight requests. Every logical
/// request arms its own token, so concurrent streams never displace each other.
#[derive(Default)]
pub struct AbortSlot(Arc<Mutex<LiveTokens>>);

#[derive(Default)]
struct LiveTokens {
    next: u64,
    tokens: Vec<(u64, CancellationToken)>,
}

fn live(tokens: &Mutex<LiveTokens>) -> MutexGuard<'_, LiveTokens> {
    tokens.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AbortSlot {
    /// Registers a fresh token for a new logical request. Arm before the
    /// credential resolves; dropping the returned guard deregisters the token.
    pub fn arm(&self) -> Armed {
        let cancel = CancellationToken::new();
        let mut live_tokens = live(&self.0);
        let id = live_tokens.next;
        live_tokens.next += 1;
        live_tokens.tokens.push((id, cancel.clone()));
        drop(live_tokens);
        Armed {
            cancel,
            id,
            slot: Arc::clone(&self.0),
        }
    }
    /// Cancels every request armed before this call; later ones are unaffected.
    pub async fn abort(&self) {
        let tokens = std::mem::take(&mut live(&self.0).tokens);
        for (_, cancel) in tokens {
            cancel.cancel();
        }
    }
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        live(&self.0).tokens.len()
    }
}

/// One logical request's registered cancellation token; derefs to the token and
/// deregisters it from its [`AbortSlot`] when dropped.
pub struct Armed {
    cancel: CancellationToken,
    id: u64,
    slot: Arc<Mutex<LiveTokens>>,
}

impl Armed {
    /// Races `work` (credential resolution or recovery) against this request's abort.
    pub async fn run<T>(
        &self,
        work: impl Future<Output = Result<T, ProviderError>>,
    ) -> Result<T, ProviderError> {
        tokio::select! {biased;()=self.cancel.cancelled()=>Err(ProviderError::Cancelled),result=work=>result}
    }
}

impl std::ops::Deref for Armed {
    type Target = CancellationToken;
    fn deref(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        live(&self.slot).tokens.retain(|(id, _)| *id != self.id);
    }
}

/// One credential recovery per logical request: `Ok(true)` when `resolved` was
/// replaced by a refreshed grant and the request may be resent.
pub async fn recover_once(
    handle: &CredentialHandle,
    resolved: &mut Resolved,
    used: &mut bool,
) -> Result<bool, ProviderError> {
    if *used {
        return Ok(false);
    }
    *used = true;
    if let Some(refreshed) = handle.recover(resolved).await? {
        *resolved = refreshed;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Attempts per logical open for both policies (omp: `maxRetries: 5`).
const ATTEMPTS: u32 = 6;

/// Derived from the profile's dialect, so request shape and retry policy agree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetryPolicy {
    /// `500·n` backoff, 300 000 ms hint ceiling, per-attempt deadline, ≤5 in-band
    /// replays before commit, whitespace-loop replays, `response.metadata` capture.
    Codex,
    /// omp `fetchWithRetry` + openai-responses: exponential backoff, 60 000 ms hint
    /// ceiling, admission-429 bypass, one first-event watchdog spanning attempts,
    /// ≤1 replay before replay-unsafe output, ≤2 empty-completion retries.
    Generic,
}

struct Texts {
    encode: &'static str,
    reuse: &'static str,
    open_timeout: &'static str,
    error_body: &'static str,
    body_transport: &'static str,
    first_event: &'static str,
    idle: &'static str,
    unterminated: &'static str,
    malformed: &'static str,
}

const CODEX_TEXTS: Texts = Texts {
    encode: "Could not encode Codex request",
    reuse: "Could not reuse Codex request body",
    open_timeout: "Codex request timed out before first event",
    error_body: "Could not read Codex error response",
    body_transport: "Codex transport error reading response body",
    first_event: "Codex stream timed out waiting for an event",
    idle: "Codex stream timed out waiting for an event",
    unterminated: "Codex stream ended before terminal completion event",
    malformed: "Could not parse Codex stream event",
};

const GENERIC_TEXTS: Texts = Texts {
    encode: "Could not encode Responses request",
    reuse: "Could not reuse Responses request body",
    open_timeout: "OpenAI responses stream timed out while waiting for the first event",
    error_body: "Could not read Responses error response",
    body_transport: "Responses transport error reading response body",
    first_event: "OpenAI responses stream timed out while waiting for the first event",
    idle: "OpenAI responses stream stalled while waiting for the next event",
    unterminated: "OpenAI responses stream closed before a terminal response event was received",
    malformed: "Could not parse Responses stream event",
};

impl RetryPolicy {
    fn of(dialect: Dialect) -> Self {
        match dialect {
            Dialect::Codex => Self::Codex,
            Dialect::Generic => Self::Generic,
        }
    }
    fn texts(self) -> &'static Texts {
        match self {
            Self::Codex => &CODEX_TEXTS,
            Self::Generic => &GENERIC_TEXTS,
        }
    }
    /// Backoff before the `n`-th retry (1-based) when no hint applies.
    fn default_delay(self, n: u32) -> u64 {
        match self {
            Self::Codex => 500 * u64::from(n),
            Self::Generic => retry::generic_delay(n),
        }
    }
    fn first_event_timeout(self) -> ProviderError {
        ProviderError::ConnectionFailed(self.texts().open_timeout.into())
    }
    /// Codex waits per attempt; Generic waits are also bounded by the watchdog.
    async fn backoff(
        self,
        cancel: &CancellationToken,
        ms: u64,
        watchdog: Instant,
    ) -> Result<(), ProviderError> {
        match self {
            Self::Codex => wait(cancel, retry::delay(ms)).await,
            Self::Generic => {
                tokio::select! {biased;()=cancel.cancelled()=>Err(ProviderError::Cancelled),slept=tokio::time::timeout_at(watchdog,tokio::time::sleep(retry::delay(ms)))=>slept.map_err(|_|self.first_event_timeout())}
            }
        }
    }
}

async fn wait(cancel: &CancellationToken, delay: Duration) -> Result<(), ProviderError> {
    tokio::select! {biased;()=cancel.cancelled()=>Err(ProviderError::Cancelled),()=tokio::time::sleep(delay)=>Ok(())}
}

fn retryable(message: &str) -> EventError {
    EventError {
        message: message.into(),
        retryable: true,
        whitespace_loop: false,
    }
}

/// One logical request: its credential, profile and once-encoded body.
struct Exchange<H, W> {
    host: Arc<H>,
    policy: RetryPolicy,
    resolved: Resolved,
    auth_retried: bool,
    wire: W,
    model: String,
    /// Header-less POST carrying the body encoded once per logical request.
    request: reqwest::Request,
    cancel: Armed,
    headers: HeaderMap,
}

struct Opened {
    response: Response,
    deadline: Instant,
}

impl<H: Host, W: Wire> Exchange<H, W> {
    fn mapper(&self) -> EventMapper {
        // Captured items carry the stamp a later request replays them by.
        let mapper = match self.wire.capture_provider() {
            Some(provider) => EventMapper::with_replay(provider, &self.model),
            None => EventMapper::new(),
        };
        mapper.with_dialect(self.wire.dialect())
    }

    async fn open(&mut self) -> Result<Opened, ProviderError> {
        let policy = self.policy;
        let texts = policy.texts();
        let mut attempt: u32 = 0;
        // Only headers vary between sends: auth after recovery, captured turn state on replay.
        self.headers = self.wire.headers(&self.model, &self.resolved.auth)?;
        let watchdog = Instant::now() + retry::stream_timeout();
        loop {
            let mut request = self
                .request
                .try_clone()
                .ok_or_else(|| ProviderError::Other(texts.reuse.into()))?;
            *request.headers_mut() = self.headers.clone();
            let deadline = match policy {
                RetryPolicy::Codex => Instant::now() + retry::stream_timeout(),
                RetryPolicy::Generic => watchdog,
            };
            let sent = tokio::select! {biased;()=self.cancel.cancelled()=>return Err(ProviderError::Cancelled),result=tokio::time::timeout_at(deadline,self.host.client().execute(request))=>result};
            let response = match sent {
                Ok(Ok(response)) => response,
                Err(_) if policy == RetryPolicy::Generic => {
                    return Err(policy.first_event_timeout());
                }
                other => {
                    if attempt + 1 < ATTEMPTS {
                        attempt += 1;
                        policy
                            .backoff(&self.cancel, policy.default_delay(attempt), watchdog)
                            .await?;
                        continue;
                    }
                    return Err(match other {
                        Ok(Err(e)) => crate::adapters::provider::classify_reqwest_error(&e),
                        _ => policy.first_event_timeout(),
                    });
                }
            };
            let status = response.status().as_u16();
            if response.status().is_success() {
                self.wire.capture(response.headers()).await;
                let deadline = match policy {
                    RetryPolicy::Codex => deadline,
                    // Headers arrived: the first-event timer restarts for the body.
                    RetryPolicy::Generic => Instant::now() + retry::stream_timeout(),
                };
                return Ok(Opened { response, deadline });
            }
            // A recovery may refresh over the network: an abort ends it promptly.
            if status == 401
                && self
                    .cancel
                    .run(recover_once(
                        self.host.credentials(),
                        &mut self.resolved,
                        &mut self.auth_retried,
                    ))
                    .await?
            {
                if let Some(url) = self.wire.recovered(&self.resolved)? {
                    *self.request.url_mut() = url
                        .parse()
                        .map_err(|_| ProviderError::Other(texts.reuse.into()))?;
                }
                self.headers = self.wire.headers(&self.model, &self.resolved.auth)?;
                continue;
            }
            let headers = response.headers().clone();
            let body = tokio::select! {biased;()=self.cancel.cancelled()=>return Err(ProviderError::Cancelled),body=tokio::time::timeout_at(deadline,response.text())=>body};
            let body = match body {
                Ok(Ok(body)) => body,
                Err(_) if policy == RetryPolicy::Generic => {
                    return Err(policy.first_event_timeout());
                }
                _ => {
                    if attempt + 1 < ATTEMPTS && retry::retry_status(status) {
                        attempt += 1;
                        policy
                            .backoff(&self.cancel, policy.default_delay(attempt), watchdog)
                            .await?;
                        continue;
                    }
                    return Err(ProviderError::ConnectionFailed(texts.error_body.into()));
                }
            };
            let (status, headers, body) = match self
                .resend_alternate(status, headers, body, deadline)
                .await?
            {
                Resent::Opened(opened) => return Ok(opened),
                Resent::Answer {
                    status,
                    headers,
                    body,
                } => (status, headers, body),
                // As an unanswered send above: the denial is not the answer.
                Resent::Unanswered(error) => {
                    if error.is_none() && policy == RetryPolicy::Generic {
                        return Err(policy.first_event_timeout());
                    }
                    if attempt + 1 < ATTEMPTS {
                        attempt += 1;
                        policy
                            .backoff(&self.cancel, policy.default_delay(attempt), watchdog)
                            .await?;
                        continue;
                    }
                    return Err(match error {
                        Some(e) => crate::adapters::provider::classify_reqwest_error(&e),
                        None => policy.first_event_timeout(),
                    });
                }
            };
            let hint = retry::hint(&headers, &body, crate::infrastructure::utils::auth_now_ms());
            if attempt + 1 < ATTEMPTS && retry::retry_status(status) {
                let delay = match policy {
                    RetryPolicy::Codex => hint
                        .is_none_or(|v| v <= retry::CODEX_MAX_DELAY_MS)
                        .then(|| hint.unwrap_or(policy.default_delay(attempt + 1))),
                    // A hint above the ceiling, a proxy admission rejection or a
                    // body-read timeout of this full-replay request is surfaced.
                    RetryPolicy::Generic => (hint.is_none_or(|v| v <= retry::GENERIC_MAX_DELAY_MS)
                        && !retry::admission_rejection(&headers, &body)
                        && !retry::request_body_read_timeout(status, &body))
                    .then(|| {
                        hint.unwrap_or_else(|| policy.default_delay(attempt + 1))
                            .min(retry::GENERIC_MAX_DELAY_MS)
                    }),
                };
                if let Some(delay) = delay {
                    attempt += 1;
                    policy.backoff(&self.cancel, delay, watchdog).await?;
                    continue;
                }
            }
            return Err(self.wire.http_error(status, &headers, &body, &self.headers));
        }
    }

    /// The identity alternate of a denied attempt: one resend with the state's
    /// headers, settled with its answer. Without one, the denial is the answer.
    async fn resend_alternate(
        &mut self,
        status: u16,
        headers: HeaderMap,
        body: String,
        deadline: Instant,
    ) -> Result<Resent, ProviderError> {
        if !matches!(status, 400 | 403) {
            return Ok(Resent::Answer {
                status,
                headers,
                body,
            });
        }
        let Some(alternate) = self.wire.alternate(status, &body, &self.headers) else {
            return Ok(Resent::Answer {
                status,
                headers,
                body,
            });
        };
        let mut request = self
            .request
            .try_clone()
            .ok_or_else(|| ProviderError::Other(self.policy.texts().reuse.into()))?;
        let mut sent = self.headers.clone();
        for (name, value) in &alternate.headers {
            sent.insert(name, value.clone());
        }
        *request.headers_mut() = sent;
        let answer = tokio::select! {biased;()=self.cancel.cancelled()=>return Err(ProviderError::Cancelled),result=tokio::time::timeout_at(deadline,self.host.client().execute(request))=>result};
        let response = match answer {
            Ok(Ok(response)) => response,
            // omp: a resend that never answered settles nothing (the learned
            // identity stays); the attempt loop retries its failure.
            Ok(Err(error)) => return Ok(Resent::Unanswered(Some(error))),
            Err(_) => return Ok(Resent::Unanswered(None)),
        };
        let ok = response.status().is_success();
        (alternate.settle)(ok);
        if ok {
            self.wire.capture(response.headers()).await;
            let deadline = match self.policy {
                RetryPolicy::Codex => deadline,
                RetryPolicy::Generic => Instant::now() + retry::stream_timeout(),
            };
            return Ok(Resent::Opened(Opened { response, deadline }));
        }
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = tokio::select! {biased;()=self.cancel.cancelled()=>return Err(ProviderError::Cancelled),body=tokio::time::timeout_at(deadline,response.text())=>body};
        Ok(Resent::Answer {
            status,
            headers,
            body: body.ok().and_then(Result::ok).unwrap_or_default(),
        })
    }
}

struct StreamState<H, W> {
    ctx: Exchange<H, W>,
    response: Option<Response>,
    buffer: SseLineBuffer,
    mapper: EventMapper,
    pending: VecDeque<StreamChunk>,
    deadline: Instant,
    /// This attempt parsed an event: later timeouts are idle stalls.
    seen_event: bool,
    /// Generic: this attempt saw replay-unsafe output, so it is never replayed.
    unsafe_seen: bool,
    retries: u8,
    whitespace_retries: u8,
    empty_retries: u8,
    finished: bool,
}

impl<H: Host, W: Wire> StreamState<H, W> {
    /// In-band replay delay for a failed attempt, or `None` when the failure ends the turn.
    fn replay_delay(&mut self, error: &EventError) -> Option<u64> {
        match self.ctx.policy {
            RetryPolicy::Codex => {
                if self.mapper.committed() {
                    None
                } else if error.whitespace_loop {
                    (self.whitespace_retries < 2).then(|| {
                        self.whitespace_retries += 1;
                        250 * u64::from(self.whitespace_retries)
                    })
                } else {
                    (error.retryable && self.retries < 5).then(|| {
                        self.retries += 1;
                        500 * u64::from(self.retries)
                    })
                }
            }
            RetryPolicy::Generic => (!self.unsafe_seen && error.retryable && self.retries < 1)
                .then(|| {
                    self.retries += 1;
                    500
                }),
        }
    }

    /// omp `withReplaySafeStreamRetry`: a plain stop with no committed output,
    /// no error and at most one output token is re-requested, at most twice.
    fn empty_completion(&self, terminal_batch: &[StreamChunk]) -> bool {
        self.ctx.policy == RetryPolicy::Generic
            && self.empty_retries < 2
            && !self.mapper.committed()
            && terminal_batch.iter().any(|chunk| {
                matches!(
                    chunk,
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn
                    }
                )
            })
            && !terminal_batch.iter().any(
                |chunk| matches!(chunk, StreamChunk::Usage { usage, .. } if usage.output_tokens > 1),
            )
    }

    async fn pull(&mut self) -> Option<StreamChunk> {
        loop {
            if let Some(chunk) = self.pending.pop_front() {
                return Some(chunk);
            }
            if self.finished {
                return None;
            }
            let texts = self.ctx.policy.texts();
            let received = tokio::select! {biased;()=self.ctx.cancel.cancelled()=>{self.finished=true;self.response.take();return Some(StreamChunk::TurnComplete{stop_reason:StopReason::Cancelled});},value=tokio::time::timeout_at(self.deadline,self.response.as_mut().expect("live response").chunk())=>value};
            let mut failure = None;
            let (frames, eof) = match received {
                Ok(Ok(Some(bytes))) => (self.buffer.feed(&bytes), false),
                // Services do not always close the final event with a blank line.
                Ok(Ok(None)) => (self.buffer.finish(), true),
                Ok(Err(_)) => {
                    failure = Some(retryable(texts.body_transport));
                    (Vec::new(), false)
                }
                Err(_) => {
                    let message = if self.seen_event {
                        texts.idle
                    } else {
                        texts.first_event
                    };
                    // omp aborts the request on its own first-event or idle timeout,
                    // which rules out a Generic replay; Codex keeps its in-band replays.
                    failure = Some(EventError {
                        message: message.into(),
                        retryable: self.ctx.policy == RetryPolicy::Codex,
                        whitespace_loop: false,
                    });
                    (Vec::new(), false)
                }
            };
            let mut empty = false;
            for frame in frames {
                if frame.data.trim().is_empty() {
                    continue;
                }
                if frame.data.trim() == "[DONE]" {
                    if !self.mapper.terminal() {
                        failure = Some(retryable(texts.unterminated));
                    }
                    break;
                }
                let value: Value = match serde_json::from_str(&frame.data) {
                    Ok(v) => v,
                    Err(_) => {
                        failure = Some(retryable(texts.malformed));
                        break;
                    }
                };
                // Generic timers count only omp's progress events; Codex restarts
                // them on every event.
                if self.ctx.policy == RetryPolicy::Codex || events::progress(&value) {
                    self.deadline = Instant::now() + retry::stream_timeout();
                    self.seen_event = true;
                }
                match self.ctx.policy {
                    RetryPolicy::Codex if value["type"] == "response.metadata" => {
                        let mut headers = HeaderMap::new();
                        if let Some(values) = value["headers"].as_object() {
                            for key in ["x-codex-turn-state", "x-models-etag"] {
                                if let Some(value) = values
                                    .get(key)
                                    .and_then(|v| v.as_str())
                                    .and_then(|v| v.parse::<HeaderValue>().ok())
                                {
                                    headers.insert(key, value);
                                }
                            }
                        }
                        self.ctx.wire.capture(&headers).await;
                    }
                    RetryPolicy::Generic if events::replay_unsafe(&value) => {
                        self.unsafe_seen = true;
                    }
                    _ => {}
                }
                match self.mapper.event(value) {
                    Ok(chunks) => {
                        // Decide before forwarding: a retried empty completion drops its
                        // whole terminal batch, replay items and usage included.
                        if self.mapper.terminal() && self.empty_completion(&chunks) {
                            empty = true;
                            self.response.take();
                            break;
                        }
                        self.pending.extend(chunks);
                    }
                    Err(mut error) => {
                        // Only provider-supplied text crosses this redaction boundary.
                        error.message = self.ctx.wire.redact(&error.message, &self.ctx.headers);
                        failure = Some(error);
                        break;
                    }
                }
                if self.mapper.terminal() {
                    self.finished = true;
                    self.response.take();
                    break;
                }
            }
            if eof && failure.is_none() && !self.finished && !empty {
                failure = Some(retryable(texts.unterminated));
            }
            let delay = if empty {
                self.empty_retries += 1;
                // A fresh request, as omp re-runs the whole attempt: its replay budget restarts.
                self.retries = 0;
                Some(500u64 << (self.empty_retries - 1))
            } else if let Some(error) = failure {
                self.response.take();
                match self.replay_delay(&error) {
                    Some(delay) => Some(delay),
                    None => {
                        self.finished = true;
                        self.pending.push_back(StreamChunk::Error {
                            content: error.message,
                        });
                        None
                    }
                }
            } else {
                None
            };
            let Some(delay) = delay else {
                continue;
            };
            self.pending.clear();
            let reopened = match wait(&self.ctx.cancel, retry::delay(delay)).await {
                Ok(()) => self.ctx.open().await,
                Err(e) => Err(e),
            };
            match reopened {
                Ok(opened) => {
                    self.response = Some(opened.response);
                    self.deadline = opened.deadline;
                    self.buffer = SseLineBuffer::new();
                    // A fresh mapper discards the failed attempt's replay buffer.
                    self.mapper = self.ctx.mapper();
                    self.seen_event = false;
                    self.unsafe_seen = false;
                }
                // An abort during backoff or reopen ends like any other abort.
                Err(ProviderError::Cancelled) => {
                    self.finished = true;
                    self.pending.push_back(StreamChunk::TurnComplete {
                        stop_reason: StopReason::Cancelled,
                    });
                }
                Err(error) => {
                    self.finished = true;
                    self.pending.push_back(StreamChunk::Error {
                        content: error.to_string(),
                    });
                }
            }
        }
    }
}

/// Encode the body once, open with the profile's retry policy and stream the
/// mapped chunks. Dropping the stream drops the response (closing the
/// connection). `cancel` was armed from `host.abort_slot()` before `resolved`
/// was resolved, so `abort()` cancels the request from its first await; the
/// stream deregisters it when finished or dropped.
pub async fn stream<H: Host, W: Wire>(
    host: Arc<H>,
    url: &str,
    cancel: Armed,
    resolved: Resolved,
    wire: W,
    messages: Vec<Message>,
    options: CompletionOptions,
) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
    let policy = RetryPolicy::of(wire.dialect());
    // History, options and request-scoped metadata fix the body: encode it once,
    // then reuse it across transport attempts, 401 recovery and in-band replays.
    let body = serde_json::to_vec(&ResponsesRequest::new(&messages, &options, &wire))
        .map_err(|_| ProviderError::Other(policy.texts().encode.into()))?;
    drop(messages);
    let request = host
        .client()
        .post(url)
        .body(body)
        .build()
        .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
    let mut ctx = Exchange {
        host,
        policy,
        resolved,
        auth_retried: false,
        wire,
        model: options.model,
        request,
        cancel,
        headers: HeaderMap::new(),
    };
    let opened = ctx.open().await?;
    let mapper = ctx.mapper();
    let state = StreamState {
        ctx,
        response: Some(opened.response),
        buffer: SseLineBuffer::new(),
        mapper,
        pending: VecDeque::new(),
        deadline: opened.deadline,
        seen_event: false,
        unsafe_seen: false,
        retries: 0,
        whitespace_retries: 0,
        empty_retries: 0,
        finished: false,
    };
    Ok(Box::pin(futures::stream::unfold(
        state,
        |mut state| async move {
            let chunk = state.pull().await?;
            Some((chunk, state))
        },
    )))
}

/// Credential and loopback fixtures of the Responses and Gemini CLI cancellation tests.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::{
        adapters::auth_store::FileAuthStore,
        domain::{
            models::{
                CompletionOptions, Credential, CredentialIdentity, CredentialOrigin, Message,
                OAuthCredential,
            },
            ports::{AuthStorePort, OAuthRefreshPort, RefreshFailure, RefreshedTokens},
        },
        infrastructure::key_resolver::{CredentialHandle, KeyResolver},
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::Notify,
    };

    /// The resolver clock of every fixture.
    pub(crate) const NOW: i64 = 1_000_000_000;

    /// Refresh exchanges that never finish; `entered` is signalled on entry.
    #[derive(Default)]
    pub(crate) struct HeldRefresh {
        pub(crate) entered: Notify,
        calls: AtomicUsize,
    }

    impl HeldRefresh {
        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl OAuthRefreshPort for HeldRefresh {
        async fn refresh(
            &self,
            _: &str,
            _: &OAuthCredential,
        ) -> Result<RefreshedTokens, RefreshFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    /// A handle over one stored OAuth row of `provider` in a fresh file store. A
    /// `stale` row refreshes before it resolves; a fresh one only on recovery.
    pub(crate) async fn stored_oauth(
        provider: &str,
        stale: bool,
        identity: CredentialIdentity,
    ) -> (CredentialHandle, Arc<HeldRefresh>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileAuthStore::at(dir.path().join("auth.json")));
        let credential = Credential::OAuth(OAuthCredential {
            access: "STORED-ACCESS".into(),
            refresh: "STORED-REFRESH".into(),
            expires_at_ms: if stale { 0 } else { NOW + 3_600_000 },
            authorized_at_ms: None,
            identity,
            routing: Default::default(),
        });
        store
            .upsert(provider, credential, CredentialOrigin::Login, false)
            .await
            .unwrap();
        let refresh = Arc::new(HeldRefresh::default());
        let resolver = Arc::new(KeyResolver::new(store, refresh.clone(), Arc::new(|| NOW)));
        let handle = CredentialHandle::new(
            resolver,
            provider.to_owned(),
            Vec::new(),
            crate::domain::services::key_cascade::RowPolicy::ANY,
            None,
        );
        (handle, refresh, dir)
    }

    pub(crate) fn user(text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "role": "user", "content": text, "images": [], "toolResults": [], "toolUses": [],
            "contextPrefix": null
        }))
        .unwrap()
    }

    pub(crate) fn options(model: &str) -> CompletionOptions {
        serde_json::from_value(serde_json::json!({
            "model": model, "maxTokens": 256, "systemPrompt": "policy", "temperature": 0.7,
            "tools": []
        }))
        .unwrap()
    }

    /// A loopback listener and its `http://` origin.
    pub(crate) async fn loopback() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        (listener, origin)
    }

    /// Bounds a structural wait so a regression fails instead of hanging.
    pub(crate) async fn within<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("structural gate reached")
    }

    /// Accepts one connection and reads one complete request from it.
    pub(crate) async fn accept(listener: &TcpListener) -> TcpStream {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let head = loop {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "request closed before its head");
            bytes.extend_from_slice(&buffer[..count]);
        };
        let length = String::from_utf8_lossy(&bytes[..head])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while bytes.len() < head + length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "request closed before its body");
            bytes.extend_from_slice(&buffer[..count]);
        }
        socket
    }

    /// Answers 401 and ends the connection, so a resend needs a new one.
    pub(crate) async fn reject(socket: &mut TcpStream) {
        socket
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
        socket.flush().await.unwrap();
    }

    /// Opens an event stream whose body never arrives.
    pub(crate) async fn hold_open(socket: &mut TcpStream) {
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        socket.flush().await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::*, *};
    use crate::{
        adapters::noop::NoOpAuthStore,
        domain::models::{CredentialIdentity, SecretString},
        infrastructure::key_resolver::compose,
    };
    use futures::{FutureExt, StreamExt};

    struct TestHost {
        client: reqwest::Client,
        handle: CredentialHandle,
        abort: AbortSlot,
    }

    impl TestHost {
        fn new(handle: CredentialHandle) -> Arc<Self> {
            Arc::new(Self {
                client: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                handle,
                abort: AbortSlot::default(),
            })
        }
    }

    impl Host for TestHost {
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

    struct TestWire;

    impl WireProfile for TestWire {
        fn dialect(&self) -> Dialect {
            Dialect::Codex
        }
        fn prompt_cache_key(&self) -> Option<&str> {
            None
        }
        fn client_metadata(&self) -> Option<&Value> {
            None
        }
    }

    #[async_trait]
    impl Wire for TestWire {
        fn headers(&self, _: &str, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
            let bearer = format!("Bearer {}", auth.bearer_secret().expose_secret());
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                HeaderValue::try_from(bearer).map_err(|_| ProviderError::AuthenticationFailed)?,
            );
            Ok(headers)
        }
        fn http_error(&self, status: u16, _: &HeaderMap, _: &str, _: &HeaderMap) -> ProviderError {
            ProviderError::Other(format!("HTTP {status}"))
        }
        fn redact(&self, message: &str, _: &HeaderMap) -> String {
            message.to_owned()
        }
    }

    fn runtime_handle() -> CredentialHandle {
        let resolver = compose(
            Arc::new(NoOpAuthStore),
            Arc::new(HeldRefresh::default()),
            Arc::new(|| NOW),
        );
        CredentialHandle::runtime(resolver, "test".into(), SecretString::new("KEY".into()))
    }

    #[tokio::test]
    async fn abort_slot_arms_one_token_per_request() {
        let slot = AbortSlot::default();
        let first = slot.arm();
        let second = slot.arm();
        drop(slot.arm());
        assert_eq!(slot.live(), 2, "a dropped request leaves no stale token");
        slot.abort().await;
        assert!(first.is_cancelled() && second.is_cancelled());
        let later = slot.arm();
        assert!(
            !later.is_cancelled(),
            "an abort never reaches a later request"
        );
        drop((first, second));
        assert_eq!(slot.live(), 1);
        drop(later);
        assert_eq!(slot.live(), 0);
    }

    /// Keystone (c): every stream in flight on one adapter is cancelled.
    #[tokio::test]
    async fn abort_cancels_every_concurrent_stream() {
        let (listener, origin) = loopback().await;
        let url = format!("{origin}/responses");
        let host = TestHost::new(runtime_handle());
        let mut streams = Vec::new();
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let cancel = host.abort_slot().arm();
            let resolved = cancel.run(host.handle.current()).await.unwrap();
            let request = vec![user("hi")];
            let (opened, socket) = tokio::join!(
                stream(
                    host.clone(),
                    &url,
                    cancel,
                    resolved,
                    TestWire,
                    request,
                    options("m")
                ),
                async {
                    let mut socket = within(accept(&listener)).await;
                    hold_open(&mut socket).await;
                    socket
                }
            );
            streams.push(opened.unwrap());
            sockets.push(socket);
        }
        assert_eq!(host.abort.live(), 2, "each stream owns its token");
        host.abort_slot().abort().await;
        for opened in &mut streams {
            let chunk = within(opened.next()).await;
            assert!(
                matches!(
                    chunk,
                    Some(StreamChunk::TurnComplete {
                        stop_reason: StopReason::Cancelled,
                        ..
                    })
                ),
                "every in-flight stream ends cancelled"
            );
            assert!(within(opened.next()).await.is_none());
        }
        let later = host.abort_slot().arm();
        assert!(
            !later.is_cancelled(),
            "an abort never reaches a later request"
        );
        drop(later);
        drop(streams);
        assert_eq!(
            host.abort.live(),
            0,
            "finished streams leave no stale token"
        );
    }

    /// Keystone (b): an abort during a 401 refresh ends the open without a resend.
    #[tokio::test]
    async fn abort_during_401_recovery_cancels_before_the_resend() {
        let (handle, refresh, _dir) =
            stored_oauth("responses-test", false, CredentialIdentity::default()).await;
        let (listener, origin) = loopback().await;
        let host = TestHost::new(handle);
        let task = tokio::spawn({
            let host = host.clone();
            async move {
                let url = format!("{origin}/responses");
                let cancel = host.abort_slot().arm();
                let resolved = cancel.run(host.handle.current()).await.unwrap();
                let request = vec![user("hi")];
                stream(
                    host,
                    &url,
                    cancel,
                    resolved,
                    TestWire,
                    request,
                    options("m"),
                )
                .await
                .map(drop)
            }
        });
        let mut socket = within(accept(&listener)).await;
        reject(&mut socket).await;
        within(refresh.entered.notified()).await;
        host.abort_slot().abort().await;
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
        assert_eq!(host.abort.live(), 0);
    }
}
