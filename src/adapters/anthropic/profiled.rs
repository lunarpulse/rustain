//! Messages transport under a provider `RequestProfile` (gateways that speak
//! the Anthropic Messages wire with their own identity, e.g. GitHub Copilot):
//! no Claude Code profile, betas, identity block or `?beta=true`.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderValue};

use super::AnthropicAdapter;
use super::sse::SseLineBuffer;
use super::stream::StreamTransformer;
use super::types::AnthropicRequest;
use crate::adapters::provider::http_retry::{self, Outcome};
use crate::adapters::provider::request_profile::{RequestContext, RequestProfile, RequestState};
use crate::adapters::provider::stream_retry::{self, Rejected};
use crate::domain::errors::ProviderError;
use crate::domain::models::{CompletionOptions, Message, StreamChunk};
use crate::infrastructure::key_resolver::Resolved;

/// Today's mapping of a final non-2xx Messages response.
fn legacy_error(status: u16, headers: &HeaderMap, body: &str) -> ProviderError {
    match status {
        401 => ProviderError::AuthenticationFailed,
        429 => ProviderError::RateLimited {
            retry_after_ms: headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| s * 1000),
        },
        code if code >= 500 => {
            ProviderError::ConnectionFailed(format!("Server error {}: {}", code, body))
        }
        code => {
            let status = reqwest::StatusCode::from_u16(code)
                .map(|s| s.to_string())
                .unwrap_or_else(|_| code.to_string());
            ProviderError::Other(format!("HTTP {}: {}", status, body))
        }
    }
}

fn chunks(response: reqwest::Response) -> BoxStream<'static, StreamChunk> {
    let stream = futures::stream::unfold(
        (
            response.bytes_stream(),
            SseLineBuffer::new(),
            StreamTransformer::with_profile(false),
            std::collections::VecDeque::new(),
        ),
        |(mut bytes, mut sse, mut transformer, mut pending)| async move {
            loop {
                if let Some(chunk) = pending.pop_front() {
                    return Some((chunk, (bytes, sse, transformer, pending)));
                }
                match bytes.next().await {
                    Some(Ok(data)) => {
                        for frame in sse.feed(&data) {
                            pending.extend(transformer.transform(&frame));
                        }
                    }
                    Some(Err(e)) => {
                        return Some((
                            StreamChunk::Error {
                                content: format!("Stream read error: {}", e),
                            },
                            (bytes, sse, transformer, pending),
                        ));
                    }
                    None => return None,
                }
            }
        },
    );
    Box::pin(stream)
}

/// One logical Messages request for one resolved credential.
struct MessagesAttempt {
    client: reqwest::Client,
    request: reqwest::Request,
    state: Option<Box<dyn RequestState>>,
    profile: Arc<dyn RequestProfile>,
}

impl MessagesAttempt {
    async fn exchange(&mut self) -> Result<Outcome, ProviderError> {
        http_retry::send(
            &self.client,
            &self.request,
            self.state
                .as_deref_mut()
                .map(|s| s as &mut dyn RequestState),
            self.profile.retry(),
        )
        .await
    }
    fn error(&self, status: u16, headers: &HeaderMap, body: &str) -> ProviderError {
        self.profile
            .error(status, headers, body, self.request.headers())
            .unwrap_or_else(|| legacy_error(status, headers, body))
    }
}

#[async_trait]
impl stream_retry::Attempt for MessagesAttempt {
    /// omp's provider-loop verdict travels with a failed open.
    async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, Rejected> {
        match self.exchange().await.map_err(Rejected::transport)? {
            Outcome::Success(response) => Ok(chunks(response)),
            Outcome::Failure {
                status,
                headers,
                body,
            } => Err(Rejected::status(
                self.error(status, &headers, &body),
                status,
                &headers,
                &body,
            )),
        }
    }
}

impl AnthropicAdapter {
    fn profiled_attempt(
        &self,
        profile: &Arc<dyn RequestProfile>,
        resolved: &Resolved,
        messages: &[Message],
        options: &CompletionOptions,
    ) -> Result<MessagesAttempt, ProviderError> {
        let ctx = RequestContext { messages, options };
        let base = profile.base_url(resolved)?;
        let url = format!("{base}/v1/messages");
        let body = serde_json::to_vec(&AnthropicRequest::from((messages, options)))
            .map_err(|_| ProviderError::Other("Could not serialize Anthropic request".into()))?;
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "authorization",
            super::bearer_header(&*profile.bearer(resolved)?)?,
        );
        profile.headers(resolved, &ctx, &mut headers);
        let state = profile.begin(resolved, &base, &ctx);
        if let Some(state) = &state {
            state.headers(&mut headers);
        }
        let request = self
            .client
            .post(&url)
            .headers(headers)
            .body(body)
            .build()
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
        Ok(MessagesAttempt {
            client: self.client.clone(),
            request,
            state,
            profile: profile.clone(),
        })
    }

    /// A profiled request whose credential the wrapping adapter already read
    /// for this logical request, so its request shaping and the request itself
    /// come from one store read.
    pub(crate) async fn stream_resolved(
        &self,
        resolved: Resolved,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let Some(profile) = &self.request_profile else {
            return Err(ProviderError::Other(
                "a pre-resolved Messages request needs a request profile".into(),
            ));
        };
        if messages.is_empty() {
            return Err(ProviderError::Other(
                "Cannot send empty messages list to Anthropic API".to_string(),
            ));
        }
        self.stream_profiled(profile, Some(resolved), messages, options)
            .await
    }

    /// Per logical request: resolve (unless `resolved` was read by the caller),
    /// send under the profile's attempts and identity alternate, recover a
    /// rejected stored grant once (then `begin` again), and stream through the
    /// profile's resilience policy, whose provider loop also re-issues a
    /// rejected first attempt.
    pub(super) async fn stream_profiled(
        &self,
        profile: &Arc<dyn RequestProfile>,
        resolved: Option<Resolved>,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let resilience = profile.resilience();
        // The first-event watchdog is armed before the first attempt.
        let deadline = resilience.map(|_| stream_retry::deadline());
        let mut resolved = match (resolved, &self.credential) {
            (Some(resolved), _) => resolved,
            (None, super::AnthropicCredential::Resolved(handle)) => handle.current().await?,
            (None, super::AnthropicCredential::Fixed(_)) => {
                return Err(ProviderError::Other(
                    "a profiled Messages transport resolves its credential per request".into(),
                ));
            }
        };
        let mut recovered = false;
        loop {
            let mut attempt = self.profiled_attempt(profile, &resolved, &messages, &options)?;
            let opened = match (resilience, deadline) {
                (Some(policy), Some(deadline)) => {
                    stream_retry::first_open(deadline, policy, async {
                        attempt.exchange().await.map_err(Rejected::transport)
                    })
                    .await
                }
                _ => attempt.exchange().await.map_err(Rejected::from),
            };
            let rejected = match opened {
                Ok(Outcome::Success(response)) => {
                    let first = chunks(response);
                    return Ok(match (resilience, deadline) {
                        (Some(policy), Some(deadline)) => {
                            stream_retry::resume(first, Box::new(attempt), policy, deadline)
                        }
                        _ => first,
                    });
                }
                Ok(Outcome::Failure { status: 401, .. })
                    if self.recover_auth(&mut resolved, &mut recovered).await? =>
                {
                    continue;
                }
                Ok(Outcome::Failure {
                    status,
                    headers,
                    body,
                }) => Rejected::status(
                    attempt.error(status, &headers, &body),
                    status,
                    &headers,
                    &body,
                ),
                Err(rejected) => rejected,
            };
            return match (resilience, deadline) {
                (Some(policy), Some(deadline)) => {
                    stream_retry::reopen(Box::new(attempt), policy, deadline, rejected).await
                }
                _ => Err(rejected.error),
            };
        }
    }
}
