//! Replay-safe stream re-issue with first-event and idle watchdogs for the Chat
//! Completions and Messages transports, and the Messages provider retry loop,
//! adapted from oh-my-pi (utils/empty-completion-retry.ts,
//! utils/idle-iterator.ts, utils/retry-after.ts, error/retryable.ts,
//! providers/openai-completions.ts, providers/anthropic.ts).
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

use std::collections::VecDeque;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use reqwest::header::HeaderMap;
use tokio::time::Instant;

use crate::adapters::responses::{
    events::{generic_retryable, usage_limit},
    retry,
};
use crate::domain::{
    errors::ProviderError,
    models::{StopReason, StreamChunk},
};

/// omp `MAX_EMPTY_COMPLETION_RETRIES`.
const EMPTY_RETRIES: u8 = 2;
/// omp `EMPTY_COMPLETION_BASE_DELAY_MS`.
const BASE_DELAY_MS: u64 = 500;

/// Pre-commit re-issue rules of one route.
#[derive(Debug)]
pub struct StreamResilience {
    first_event: &'static str,
    idle: &'static str,
    /// Sent when the stream ends after committed output without a terminal chunk.
    truncated: Option<&'static str>,
    /// omp `PROVIDER_MAX_RETRIES`: re-issues before the first token, of an
    /// in-stream provider failure or a rejected open, from one budget.
    transient_retries: u8,
    /// Anthropic backoff (`min(500·2^n, 8000)·(1 − u/4)`) instead of omp's
    /// replay-safe `500·2^n`.
    sdk_backoff: bool,
}

/// omp openai-completions: one transient re-issue, empty completions retried.
pub static CHAT: StreamResilience = StreamResilience {
    first_event: "OpenAI completions stream timed out while waiting for the first event",
    idle: "OpenAI completions stream stalled while waiting for the next event",
    truncated: Some("OpenAI completions stream closed before a finish_reason was received"),
    transient_retries: 1,
    sdk_backoff: false,
};

/// omp anthropic provider loop (`anthropic.ts` `PROVIDER_MAX_RETRIES = 10`):
/// up to ten re-issues before the first token, failed opens included (the
/// route has no HTTP-attempt policy: omp pins the client retries to 0).
pub static MESSAGES: StreamResilience = StreamResilience {
    first_event: "Anthropic stream timed out while waiting for the first event",
    idle: "Anthropic stream stalled while waiting for the next event",
    truncated: None,
    transient_retries: 10,
    sdk_backoff: true,
};

/// One replay-safe attempt: the adapter's single HTTP exchange (with its own
/// HTTP-attempt policy) for the same resolved credential and encoded body.
#[async_trait]
pub trait Attempt: Send + 'static {
    async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, Rejected>;
}

/// A failed open: its error, and whether omp's provider loop re-issues it
/// (`isProviderRetryableError`) and after which server hint.
#[derive(Debug)]
pub struct Rejected {
    pub error: ProviderError,
    retryable: bool,
    hint_ms: Option<u64>,
}

/// A final error, never re-issued.
impl From<ProviderError> for Rejected {
    fn from(error: ProviderError) -> Self {
        Self {
            error,
            retryable: false,
            hint_ms: None,
        }
    }
}

impl Rejected {
    /// A final non-2xx answer: 408, 429 and ≥ 500 are re-issued unless the body
    /// reads as an account usage limit; every other status (409 included) is
    /// terminal. `x-should-retry` is not consulted.
    pub fn status(error: ProviderError, status: u16, headers: &HeaderMap, body: &str) -> Self {
        Self {
            error,
            retryable: (matches!(status, 408 | 429) || status >= 500) && !usage_limit(body),
            hint_ms: retry_after(headers, crate::infrastructure::utils::auth_now_ms()),
        }
    }

    /// A send that got no answer: omp re-issues every network failure of its
    /// client (connection error, timeout, unreachable, refused, TLS).
    pub fn transport(error: ProviderError) -> Self {
        Self {
            retryable: matches!(
                error,
                ProviderError::Offline(_) | ProviderError::ConnectionFailed(_)
            ),
            error,
            hint_ms: None,
        }
    }
}

/// omp `getRetryAfterMsFromHeaders`: the largest positive hint of
/// `retry-after-ms`, `retry-after` (seconds or a date), `x-ratelimit-reset-ms`
/// and `x-ratelimit-reset` (a delta, or an epoch in s/ms when past 1e9).
fn retry_after(headers: &HeaderMap, now: i64) -> Option<u64> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let positive = |name: &str| {
        text(name)?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v > 0.0)
    };
    let now = now as f64;
    let reset = |name: &str, unit: f64| {
        let value = positive(name)?;
        let target = if value > 1e12 {
            value
        } else if value > 1e9 {
            value * 1000.0
        } else {
            return Some((value * unit).ceil());
        };
        (target > now).then(|| (target - now).ceil())
    };
    let after = text("retry-after").and_then(|value| match value.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => (seconds > 0.0).then(|| (seconds * 1000.0).ceil()),
        _ => retry::date(value)
            .map(|at| at as f64 - now)
            .filter(|delay| *delay > 0.0)
            .map(f64::ceil),
    });
    [
        positive("retry-after-ms").map(f64::ceil),
        after,
        reset("x-ratelimit-reset-ms", 1.0),
        reset("x-ratelimit-reset", 1000.0),
    ]
    .into_iter()
    .flatten()
    .reduce(f64::max)
    .map(|ms| ms.min(u64::MAX as f64) as u64)
}

impl StreamResilience {
    fn transient_delay(&self, retry: u8) -> u64 {
        if self.sdk_backoff {
            use rand_core::RngCore;
            let base = (BASE_DELAY_MS << u32::from(retry).min(16)).min(8_000);
            let u = f64::from(rand_core::OsRng.next_u32()) / (f64::from(u32::MAX) + 1.0);
            (base as f64 * (1.0 - u * 0.25)) as u64
        } else {
            BASE_DELAY_MS << u32::from(retry).min(16)
        }
    }

    /// omp provider loop: the wait before re-issue `n` (0-based) of a
    /// retryable failure, `max(hint, backoff)`; a hint above 60 s surfaces the
    /// failure (`None`).
    fn provider_delay(&self, n: u8, hint_ms: Option<u64>) -> Option<u64> {
        let backoff = self.transient_delay(n);
        match hint_ms {
            Some(hint) if hint > retry::GENERIC_MAX_DELAY_MS => None,
            Some(hint) => Some(hint.max(backoff)),
            None => Some(backoff),
        }
    }
}

/// The first-event watchdog deadline, armed before the first attempt.
pub fn deadline() -> Instant {
    Instant::now() + retry::stream_timeout()
}

/// The first attempt's open under the first-event watchdog.
pub async fn first_open<T, E: From<ProviderError>>(
    deadline: Instant,
    policy: &'static StreamResilience,
    open: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    match tokio::time::timeout_at(deadline, open).await {
        Ok(opened) => opened,
        Err(_) => Err(ProviderError::ConnectionFailed(policy.first_event.into()).into()),
    }
}

/// The replay-safe stream over the opened first attempt; `attempt` re-issues.
pub fn resume(
    first: BoxStream<'static, StreamChunk>,
    attempt: Box<dyn Attempt>,
    policy: &'static StreamResilience,
    first_deadline: Instant,
) -> BoxStream<'static, StreamChunk> {
    State::new(attempt, policy, Some(first), first_deadline).into_stream()
}

/// omp's provider loop over a rejected first attempt: re-issues it within the
/// route budget and streams the first attempt that opens (the budget it spent
/// carries over); a final rejection is the request's error.
pub async fn reopen(
    attempt: Box<dyn Attempt>,
    policy: &'static StreamResilience,
    first_deadline: Instant,
    rejected: Rejected,
) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
    let mut state = State::new(attempt, policy, None, first_deadline);
    match state.reissue(Ending::Rejected(rejected)).await {
        Reissued::Opened | Reissued::Finished => Ok(state.into_stream()),
        Reissued::Rejected(error) => Err(error),
        Reissued::Watchdog => Err(ProviderError::ConnectionFailed(policy.first_event.into())),
    }
}

struct State {
    attempt: Box<dyn Attempt>,
    policy: &'static StreamResilience,
    inner: Option<BoxStream<'static, StreamChunk>>,
    /// Armed before the first attempt; spans re-issues until any event arrives.
    first_deadline: Instant,
    seen_event: bool,
    /// The current attempt produced a chunk.
    attempt_event: bool,
    idle_deadline: Instant,
    committed: bool,
    /// Pre-output chunks of the current attempt; dropped with a discarded attempt.
    buffered: Vec<StreamChunk>,
    pending: VecDeque<StreamChunk>,
    empty_retries: u8,
    transient_retries: u8,
    finished: bool,
}

/// What ends a pre-commit attempt.
enum Ending {
    /// The inner stream ended.
    Eof,
    Failed(String),
    /// A re-issue (or the first attempt) failed to open.
    Rejected(Rejected),
}

/// How a pre-commit re-issue ended.
enum Reissued {
    /// A new attempt is streaming.
    Opened,
    /// The stream finished; its chunks are queued.
    Finished,
    /// The last open was rejected and is final.
    Rejected(ProviderError),
    /// The first-event watchdog fired while opening.
    Watchdog,
}

fn meaningful(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::Text { content, .. } | StreamChunk::Thinking { content, .. } => {
            !content.is_empty()
        }
        StreamChunk::ToolUse { .. } => true,
        _ => false,
    }
}

impl State {
    fn new(
        attempt: Box<dyn Attempt>,
        policy: &'static StreamResilience,
        inner: Option<BoxStream<'static, StreamChunk>>,
        first_deadline: Instant,
    ) -> Self {
        Self {
            attempt,
            policy,
            inner,
            first_deadline,
            seen_event: false,
            attempt_event: false,
            idle_deadline: first_deadline,
            committed: false,
            buffered: Vec::new(),
            pending: VecDeque::new(),
            empty_retries: 0,
            transient_retries: 0,
            finished: false,
        }
    }

    fn into_stream(self) -> BoxStream<'static, StreamChunk> {
        Box::pin(futures::stream::unfold(self, |mut state| async move {
            let chunk = state.pull().await?;
            Some((chunk, state))
        }))
    }

    fn finish_with(&mut self, chunk: StreamChunk) {
        self.finished = true;
        self.inner = None;
        self.pending.push_back(chunk);
    }

    /// omp `withReplaySafeStreamRetry`: an uncommitted plain stop with at most
    /// one output token and no visible output is re-requested.
    fn empty_completion(&self) -> bool {
        let stopped = self.buffered.iter().any(|chunk| {
            matches!(
                chunk,
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn
                }
            )
        });
        // A transport end without a finish is a default stop.
        let terminal = self
            .buffered
            .iter()
            .any(|chunk| matches!(chunk, StreamChunk::TurnComplete { .. }));
        (stopped || !terminal)
            && !self
                .buffered
                .iter()
                .any(|chunk| matches!(chunk, StreamChunk::Usage { usage, .. } if usage.output_tokens > 1))
    }

    async fn pull(&mut self) -> Option<StreamChunk> {
        loop {
            if let Some(chunk) = self.pending.pop_front() {
                return Some(chunk);
            }
            if self.finished {
                return None;
            }
            let deadline = if self.seen_event || self.attempt_event {
                self.idle_deadline
            } else {
                self.first_deadline
            };
            let next = {
                let inner = self.inner.as_mut().expect("live attempt");
                tokio::time::timeout_at(deadline, inner.next()).await
            };
            let ending = match next {
                Err(_) => {
                    // A local watchdog abort is never re-issued.
                    let text = if self.seen_event || self.attempt_event {
                        self.policy.idle
                    } else {
                        self.policy.first_event
                    };
                    // After commit `buffered` only holds copies of terminal chunks
                    // already delivered; only a pre-output attempt flushes its buffer.
                    if !self.committed {
                        let mut flushed = std::mem::take(&mut self.buffered);
                        self.pending.extend(flushed.drain(..));
                    }
                    self.finish_with(StreamChunk::Error {
                        content: text.into(),
                    });
                    continue;
                }
                Ok(Some(chunk)) => {
                    self.attempt_event = true;
                    self.seen_event = true;
                    self.idle_deadline = Instant::now() + retry::stream_timeout();
                    if self.committed {
                        match &chunk {
                            StreamChunk::TurnComplete { .. } => self.buffered.push(chunk.clone()),
                            // A committed failure is final: nothing (not even the
                            // truncation notice) follows it.
                            StreamChunk::Error { .. } => {
                                self.finished = true;
                                self.inner = None;
                            }
                            _ => {}
                        }
                        return Some(chunk);
                    }
                    if meaningful(&chunk) {
                        self.committed = true;
                        let mut flushed = std::mem::take(&mut self.buffered);
                        self.pending.extend(flushed.drain(..));
                        self.pending.push_back(chunk);
                        continue;
                    }
                    if let StreamChunk::Error { content } = chunk {
                        Ending::Failed(content)
                    } else {
                        self.buffered.push(chunk);
                        continue;
                    }
                }
                Ok(None) => Ending::Eof,
            };
            if self.committed {
                // Only an end of stream reaches here after commit.
                let terminal = self
                    .buffered
                    .iter()
                    .any(|chunk| matches!(chunk, StreamChunk::TurnComplete { .. }));
                self.finished = true;
                self.inner = None;
                if !terminal && let Some(text) = self.policy.truncated {
                    self.pending.push_back(StreamChunk::Error {
                        content: text.into(),
                    });
                }
                continue;
            }
            match self.reissue(ending).await {
                Reissued::Opened | Reissued::Finished => {}
                Reissued::Rejected(error) => self.finish_with(StreamChunk::Error {
                    content: error.to_string(),
                }),
                Reissued::Watchdog => self.finish_with(StreamChunk::Error {
                    content: self.policy.first_event.into(),
                }),
            }
        }
    }

    /// Re-issues after a pre-commit ending while the route allows it, again on
    /// each rejected re-open; otherwise finishes with the buffered chunks (and
    /// the in-stream failure).
    async fn reissue(&mut self, mut ending: Ending) -> Reissued {
        loop {
            let delay = match &ending {
                Ending::Eof => (self.empty_retries < EMPTY_RETRIES && self.empty_completion())
                    .then(|| {
                        let delay = BASE_DELAY_MS << self.empty_retries;
                        self.empty_retries += 1;
                        delay
                    }),
                Ending::Failed(message) => self.provider_retry(generic_retryable(message), None),
                Ending::Rejected(rejected) => {
                    self.provider_retry(rejected.retryable, rejected.hint_ms)
                }
            };
            let Some(delay) = delay else {
                let mut flushed = std::mem::take(&mut self.buffered);
                self.pending.extend(flushed.drain(..));
                self.finished = true;
                self.inner = None;
                return match ending {
                    Ending::Eof => Reissued::Finished,
                    Ending::Failed(message) => {
                        self.pending
                            .push_back(StreamChunk::Error { content: message });
                        Reissued::Finished
                    }
                    Ending::Rejected(rejected) => Reissued::Rejected(rejected.error),
                };
            };
            // Discard the attempt: its buffered pre-output chunks never surface.
            self.inner = None;
            self.buffered.clear();
            tokio::time::sleep(retry::delay(delay)).await;
            let deadline = if self.seen_event {
                Instant::now() + retry::stream_timeout()
            } else {
                self.first_deadline
            };
            match tokio::time::timeout_at(deadline, self.attempt.open()).await {
                Ok(Ok(inner)) => {
                    self.inner = Some(inner);
                    self.attempt_event = false;
                    self.idle_deadline = deadline;
                    return Reissued::Opened;
                }
                Ok(Err(rejected)) => ending = Ending::Rejected(rejected),
                Err(_) => return Reissued::Watchdog,
            }
        }
    }

    /// omp's provider loop: one budget for every pre-commit re-issue of a
    /// provider-retryable failure, in-stream or at open.
    fn provider_retry(&mut self, retryable: bool, hint_ms: Option<u64>) -> Option<u64> {
        if !retryable || self.transient_retries >= self.policy.transient_retries {
            return None;
        }
        let delay = self
            .policy
            .provider_delay(self.transient_retries, hint_ms)?;
        self.transient_retries += 1;
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::UsageInfo;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// Scripted attempts: each open yields the next chunk list; `None` stalls forever.
    struct Script {
        attempts: Vec<Option<Vec<StreamChunk>>>,
        opened: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Attempt for Script {
        async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, Rejected> {
            let index = self.opened.fetch_add(1, Ordering::SeqCst);
            match self.attempts.get(index).cloned().flatten() {
                Some(chunks) => Ok(futures::stream::iter(chunks).boxed()),
                None => Ok(futures::stream::pending().boxed()),
            }
        }
    }

    fn text(content: &str) -> StreamChunk {
        StreamChunk::Text {
            content: content.into(),
            parent_tool_use_id: None,
        }
    }
    fn usage(output: u32) -> StreamChunk {
        StreamChunk::Usage {
            usage: UsageInfo {
                input_tokens: 0,
                output_tokens: output,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            },
            session_id: None,
        }
    }
    fn stop() -> StreamChunk {
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        }
    }

    /// The adapters' shape: arm, open the first attempt, then resume.
    async fn stream(
        mut attempt: Box<dyn Attempt>,
        policy: &'static StreamResilience,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let deadline = deadline();
        let first = first_open(deadline, policy, attempt.open())
            .await
            .map_err(|rejected| rejected.error)?;
        Ok(resume(first, attempt, policy, deadline))
    }

    async fn run(
        attempts: Vec<Option<Vec<StreamChunk>>>,
        policy: &'static StreamResilience,
    ) -> (Vec<StreamChunk>, usize) {
        let opened = Arc::new(AtomicUsize::new(0));
        let script = Script {
            attempts,
            opened: opened.clone(),
        };
        let chunks = stream(Box::new(script), policy)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        (chunks, opened.load(Ordering::SeqCst))
    }

    fn texts(chunks: &[StreamChunk]) -> Vec<String> {
        chunks
            .iter()
            .filter_map(|chunk| match chunk {
                StreamChunk::Text { content, .. } => Some(format!("text:{content}")),
                StreamChunk::Error { content } => Some(format!("error:{content}")),
                StreamChunk::TurnComplete { .. } => Some("stop".into()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn empty_completions_reissue_twice_then_deliver_the_empty_turn() {
        let empty = Some(vec![stop(), usage(0)]);
        let (chunks, opened) = run(vec![empty.clone(), empty.clone(), empty], &CHAT).await;
        assert_eq!(opened, 3);
        assert_eq!(texts(&chunks), ["stop"]);
        let (chunks, opened) = run(
            vec![Some(vec![stop(), usage(0)]), Some(vec![text("hi"), stop()])],
            &MESSAGES,
        )
        .await;
        assert_eq!(opened, 2);
        assert_eq!(texts(&chunks), ["text:hi", "stop"]);
    }

    #[tokio::test(start_paused = true)]
    async fn eof_before_output_reissues_and_eof_after_output_is_an_error() {
        let (chunks, opened) = run(vec![Some(vec![]), Some(vec![text("ok"), stop()])], &CHAT).await;
        assert_eq!(opened, 2);
        assert_eq!(texts(&chunks), ["text:ok", "stop"]);
        let (chunks, opened) = run(vec![Some(vec![text("partial")])], &CHAT).await;
        assert_eq!(opened, 1);
        assert_eq!(
            texts(&chunks),
            [
                "text:partial",
                "error:OpenAI completions stream closed before a finish_reason was received"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_reissue_only_before_output_within_the_route_budget() {
        let failed = || {
            Some(vec![StreamChunk::Error {
                content: "overloaded".into(),
            }])
        };
        let (chunks, opened) = run(vec![failed(), failed()], &CHAT).await;
        assert_eq!(opened, 2, "chat re-issues one transient failure");
        assert_eq!(texts(&chunks), ["error:overloaded"]);
        let mut attempts = vec![failed(); 10];
        attempts.push(Some(vec![text("late"), stop()]));
        let (chunks, opened) = run(attempts, &MESSAGES).await;
        assert_eq!(opened, 11);
        assert_eq!(texts(&chunks), ["text:late", "stop"]);
        let (chunks, opened) = run(
            vec![
                Some(vec![
                    text("x"),
                    StreamChunk::Error {
                        content: "overloaded".into(),
                    },
                ]),
                Some(vec![text("never")]),
            ],
            &CHAT,
        )
        .await;
        assert_eq!(opened, 1, "a committed attempt is never re-issued");
        assert_eq!(texts(&chunks), ["text:x", "error:overloaded"]);
        let (_, opened) = run(
            vec![
                Some(vec![StreamChunk::Error {
                    content: "invalid request".into(),
                }]),
                Some(vec![]),
            ],
            &CHAT,
        )
        .await;
        assert_eq!(opened, 1, "a non-transient failure surfaces");
    }

    #[tokio::test(start_paused = true)]
    async fn watchdogs_fire_with_the_route_texts_and_discard_buffered_chunks() {
        let (chunks, opened) = run(vec![None], &CHAT).await;
        assert_eq!(opened, 1);
        assert_eq!(
            texts(&chunks),
            ["error:OpenAI completions stream timed out while waiting for the first event"]
        );
        // The first-event watchdog spans a re-issue: an empty attempt, then a stall.
        let (chunks, opened) = run(vec![Some(vec![]), None], &MESSAGES).await;
        assert_eq!(opened, 2);
        assert_eq!(
            texts(&chunks),
            ["error:Anthropic stream timed out while waiting for the first event"]
        );
        // Idle between chunks after output; a delivered terminal is never repeated.
        struct Stall(Vec<StreamChunk>);
        #[async_trait]
        impl Attempt for Stall {
            async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, Rejected> {
                Ok(futures::stream::iter(self.0.clone())
                    .chain(futures::stream::pending())
                    .boxed())
            }
        }
        for (script, mut expected) in [
            (vec![text("a")], vec!["text:a"]),
            (vec![text("a"), stop()], vec!["text:a", "stop"]),
        ] {
            let chunks = stream(Box::new(Stall(script)), &MESSAGES)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            expected.push("error:Anthropic stream stalled while waiting for the next event");
            assert_eq!(texts(&chunks), expected);
        }
    }

    /// Scripted opens for the provider loop; an open past the script panics.
    struct Opens {
        replies: VecDeque<Result<Vec<StreamChunk>, Rejected>>,
        opened: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Attempt for Opens {
        async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, Rejected> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            let chunks = self
                .replies
                .pop_front()
                .expect("an open within the script")?;
            Ok(futures::stream::iter(chunks).boxed())
        }
    }

    fn header_map(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }
    fn status(code: u16, headers: &[(&'static str, &str)], body: &str) -> Rejected {
        Rejected::status(
            ProviderError::Other(format!("HTTP {code}")),
            code,
            &header_map(headers),
            body,
        )
    }
    fn busy() -> Rejected {
        status(503, &[], "busy")
    }
    fn overloaded() -> Vec<StreamChunk> {
        vec![StreamChunk::Error {
            content: "overloaded".into(),
        }]
    }

    /// `reopen` after the rejected first attempt `first`: the surfaced chunks
    /// or error, the re-opens and the time slept.
    async fn reopened(
        first: Rejected,
        replies: Vec<Result<Vec<StreamChunk>, Rejected>>,
    ) -> (Result<Vec<String>, String>, usize, std::time::Duration) {
        let opened = Arc::new(AtomicUsize::new(0));
        let attempt = Opens {
            replies: replies.into(),
            opened: opened.clone(),
        };
        let start = Instant::now();
        let result = match reopen(Box::new(attempt), &MESSAGES, deadline(), first).await {
            Ok(stream) => Ok(texts(&stream.collect::<Vec<_>>().await)),
            Err(error) => Err(error.to_string()),
        };
        (result, opened.load(Ordering::SeqCst), start.elapsed())
    }

    #[tokio::test(start_paused = true)]
    async fn messages_provider_loop_retries_ten_times_from_one_budget() {
        // Ten rejected opens, then a stream: 11 attempts (the first + 10 re-opens).
        let mut replies: Vec<_> = (0..9).map(|_| Err(busy())).collect();
        replies.push(Ok(vec![text("ok"), stop()]));
        let (result, opened, slept) = reopened(busy(), replies).await;
        assert_eq!(result.unwrap(), ["text:ok", "stop"]);
        assert_eq!(opened, 10);
        // omp's waits 500, 1000, 2000, 4000, then 8000 × 6, each × (1 − u/4)
        // (omp anthropic-stream-timeout.test.ts "retries 502s ten times").
        let full = 500 + 1000 + 2000 + 4000 + 8000 * 6;
        assert!(
            (full * 3 / 4..=full).contains(&(slept.as_millis() as u64)),
            "{slept:?}"
        );
        // Eleven failures: the budget is spent, the last rejection is the error.
        let replies: Vec<_> = (0..10).map(|_| Err(busy())).collect();
        let (result, opened, _) = reopened(busy(), replies).await;
        assert_eq!(result.unwrap_err(), "HTTP 503");
        assert_eq!(opened, 10);
        // Rejected opens and in-stream failures share the budget: 1 + 4 rejected,
        // 5 in-stream, then the 11th failure (a rejected re-open) surfaces.
        let mut replies: Vec<_> = (0..4).map(|_| Err(busy())).collect();
        replies.extend((0..5).map(|_| Ok(overloaded())));
        replies.push(Err(busy()));
        let (result, opened, _) = reopened(busy(), replies).await;
        assert_eq!(result.unwrap(), ["error:HTTP 503"]);
        assert_eq!(opened, 10);
    }

    #[tokio::test(start_paused = true)]
    async fn messages_provider_loop_follows_omp_verdicts_and_hints() {
        let ok = || Ok(vec![text("ok"), stop()]);
        // Final at once: 409, 400, 401, a hint over 60 s, a usage limit, a
        // non-transport error.
        for first in [
            status(409, &[("x-should-retry", "true")], "conflict"),
            status(400, &[], "bad"),
            status(401, &[], "unauthorized"),
            status(429, &[("retry-after", "61")], "slow down"),
            status(429, &[], "monthly quota exceeded"),
            Rejected::transport(ProviderError::Other("no body".into())),
            Rejected::from(ProviderError::Offline("down".into())),
        ] {
            let (result, opened, _) = reopened(first, vec![ok()]).await;
            assert!(result.is_err());
            assert_eq!(opened, 0);
        }
        // Retried: 408, 529 despite `x-should-retry: false`, a transport failure.
        for first in [
            status(408, &[], "timeout"),
            status(529, &[("x-should-retry", "false")], "overloaded"),
            Rejected::transport(ProviderError::Offline("refused".into())),
            Rejected::transport(ProviderError::ConnectionFailed("reset".into())),
        ] {
            let (result, opened, _) = reopened(first, vec![ok()]).await;
            assert_eq!(result.unwrap(), ["text:ok", "stop"]);
            assert_eq!(opened, 1);
        }
        // The wait is max(hint, backoff): a longer hint wins, a shorter one not.
        let (_, _, slept) = reopened(
            status(429, &[("retry-after", "2")], "slow down"),
            vec![ok()],
        )
        .await;
        assert!((2000..=2001).contains(&slept.as_millis()), "{slept:?}");
        let (_, _, slept) =
            reopened(status(503, &[("retry-after-ms", "1")], "busy"), vec![ok()]).await;
        assert!((375..=500).contains(&slept.as_millis()), "{slept:?}");
        // A hint over 60 s on a later re-open surfaces that failure.
        let (result, opened, _) = reopened(
            busy(),
            vec![Err(status(429, &[("retry-after", "61")], "slow")), ok()],
        )
        .await;
        assert_eq!(result.unwrap_err(), "HTTP 429");
        assert_eq!(opened, 1);
    }

    #[test]
    fn retry_after_takes_the_largest_positive_hint() {
        let now = 1_700_000_000_000;
        let hint = |pairs: &[(&'static str, &str)]| retry_after(&header_map(pairs), now);
        assert_eq!(hint(&[]), None);
        assert_eq!(hint(&[("retry-after-ms", "1.2")]), Some(2));
        assert_eq!(
            hint(&[("retry-after-ms", "1500"), ("retry-after", "3")]),
            Some(3000)
        );
        assert_eq!(
            hint(&[("retry-after-ms", "5000"), ("retry-after", "3")]),
            Some(5000)
        );
        assert_eq!(
            hint(&[("retry-after", "0"), ("retry-after-ms", "-1")]),
            None
        );
        assert_eq!(hint(&[("x-ratelimit-reset-ms", "2500")]), Some(2500));
        assert_eq!(hint(&[("x-ratelimit-reset", "4")]), Some(4000));
        assert_eq!(hint(&[("x-ratelimit-reset", "1700000010")]), Some(10_000));
        assert_eq!(
            hint(&[("x-ratelimit-reset-ms", "1700000007000")]),
            Some(7000)
        );
        assert_eq!(hint(&[("x-ratelimit-reset", "1699999990")]), None);
        let at = chrono::DateTime::from_timestamp_millis(now + 30_000)
            .unwrap()
            .to_rfc2822();
        assert_eq!(hint(&[("retry-after", at.as_str())]), Some(30_000));
        let past = chrono::DateTime::from_timestamp_millis(now - 30_000)
            .unwrap()
            .to_rfc2822();
        assert_eq!(hint(&[("retry-after", past.as_str())]), None);
    }
}
