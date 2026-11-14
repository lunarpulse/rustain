//! OpenAI-compatible API adapter implementing the `StreamingProvider` trait.
//!
//! Works with any provider that implements the OpenAI Chat Completions API:
//! - **Kimi** (Moonshot AI) — `https://api.moonshot.cn/v1`
//! - **OpenAI** — `https://api.openai.com/v1`
//! - **DeepSeek** — `https://api.deepseek.com/v1`
//! - Any other OpenAI-compatible endpoint
//!
//! Wire format: OpenAI Chat Completions with `stream: true`.
//! Auth: `Authorization: Bearer {key}`, resolved per request; a profile may
//! derive the base URL, bearer and headers from the resolved credential.

pub mod allowlists;
pub mod discovery;
pub mod profile;
pub mod stream;
pub mod types;
pub mod variant;

pub use profile::{
    ChatCompat, ChatImages, ChatProfile, InBandError, InBandText, StaticChatProfile,
};
pub use variant::OpenAiCompatibleVariant;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::adapters::provider::http_retry::{self, Outcome};
use crate::adapters::provider::request_profile::{RequestContext, RequestState};
use crate::adapters::provider::stream_retry;
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelDescriptor, ModelRecord};
use crate::domain::models::{
    AuthSource, CompletionOptions, Message, ResolvedAuth, SecretString, StreamChunk,
};
use crate::domain::ports::StreamingProvider;
use crate::infrastructure::key_resolver::{CredentialHandle, Resolved};

use self::stream::OpenAiStreamTransformer;
use self::types::OpenAiRequest;
use crate::adapters::anthropic::sse::SseLineBuffer;

/// A fixed key serves candidate-key probes and keyless endpoints (empty: no
/// `Authorization`); production entries resolve their credential per request.
pub enum ChatCredential {
    Fixed(SecretString),
    Resolved(CredentialHandle),
}

/// OpenAI-compatible API adapter. Implements `StreamingProvider` for streaming completions.
///
/// `Debug` is manually implemented to mask credentials (prevent accidental key leakage).
pub struct OpenAiAdapter {
    client: reqwest::Client,
    credential: ChatCredential,
    model: String,
    variant: OpenAiCompatibleVariant,
    profile: Arc<dyn ChatProfile>,
    #[allow(dead_code)] // Used by abort() method
    abort_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    discovered_models: Arc<arc_swap::ArcSwap<Option<Vec<ModelDescriptor>>>>,
}

/// Consolidated secret exposure: the `Authorization` value of every request
/// and its construction-time checks. An empty key sends no header.
pub(crate) fn authorization(bearer: &SecretString) -> Result<Option<HeaderValue>, ProviderError> {
    let secret = bearer.expose_secret();
    if secret.is_empty() {
        return Ok(None);
    }
    if secret.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(ProviderError::Other(
            "Credential contains control characters (newlines, tabs, etc.) — check your env var or config".to_string(),
        ));
    }
    let mut value = HeaderValue::try_from(format!("Bearer {secret}"))
        .map_err(|_| ProviderError::Other("Invalid credential".into()))?;
    value.set_sensitive(true);
    Ok(Some(value))
}

/// Today's mapping of a final non-2xx Chat Completions response.
fn legacy_error(status: u16, headers: &HeaderMap, body: &str) -> ProviderError {
    match status {
        401 => ProviderError::AuthenticationFailed,
        429 => {
            let retry_after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if let Some(secs) = retry_after {
                tracing::debug!("Rate limited, retry-after: {}s", secs);
            }
            ProviderError::RateLimited {
                retry_after_ms: retry_after.map(|s| s * 1000),
            }
        }
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

/// Bytes of a body without a stream event kept for the error text.
const CAPTURE_CAP: usize = 8 * 1024;
/// Bytes read before the first SSE frame: a body that reaches this without one
/// is not a stream (an SSE line buffer would otherwise grow without limit).
const PRE_FRAME_CAP: usize = 1024 * 1024;

/// The final error of a 2xx whose body carried no SSE event: status, content
/// type and either the error object the body carried (whole body captured) or a
/// redacted, bounded prefix of it.
fn zero_event_error(
    errors: &InBandText,
    content_type: Option<&str>,
    mut capture: Vec<u8>,
    truncated: bool,
) -> ProviderError {
    if truncated {
        // A token cut by the capture end must never survive as an
        // unrecognisable prefix: drop the trailing run of non-whitespace.
        while capture.last().is_some_and(|b| !b.is_ascii_whitespace()) {
            capture.pop();
        }
    }
    let body = String::from_utf8_lossy(&capture);
    let parts = if truncated {
        None
    } else {
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| stream::error_parts(&value))
    };
    let (code, message) = parts.unwrap_or_default();
    let text = errors.text(&InBandError {
        status: errors.status(),
        content_type,
        code: code.as_deref(),
        message: message.as_deref(),
        raw: &body,
        in_stream: false,
    });
    // Only the text already made safe (normalised, request-redacted, bounded)
    // is logged: no separate raw header or body field.
    tracing::warn!(
        status = errors.status(),
        "Chat answer carried no stream event: {text}"
    );
    ProviderError::Other(text)
}

/// Reads `response` until its first SSE frame, then streams the decoded chunks.
async fn open_stream(
    response: reqwest::Response,
    errors: Arc<InBandText>,
) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    open_chunks(response.bytes_stream(), content_type, errors).await
}

/// The open path over a body's chunks. A body that ends, or whose first
/// `PRE_FRAME_CAP` bytes complete no SSE frame, is a final error carrying what
/// was sent; a read error before the first frame is the stream's first chunk.
/// A chunk crossing the cap is fed only up to it, and the rest only when a
/// frame completed within the allowance. An in-stream error ends the stream.
async fn open_chunks<S, B, E>(
    mut byte_stream: S,
    content_type: Option<String>,
    errors: Arc<InBandText>,
) -> Result<BoxStream<'static, StreamChunk>, ProviderError>
where
    S: futures::Stream<Item = Result<B, E>> + Unpin + Send + 'static,
    B: AsRef<[u8]> + Send,
    E: fmt::Display + Send + 'static,
{
    let mut sse_buf = SseLineBuffer::new();
    let mut transformer = OpenAiStreamTransformer::with_errors(errors.clone());
    let mut capture = Vec::new();
    let mut read = 0usize;
    let mut ended = false;
    let first = loop {
        match byte_stream.next().await {
            Some(Ok(bytes)) => {
                let bytes = bytes.as_ref();
                let (head, tail) = bytes.split_at(bytes.len().min(PRE_FRAME_CAP - read));
                capture.extend_from_slice(
                    &head[..head.len().min(CAPTURE_CAP.saturating_sub(capture.len()))],
                );
                read += head.len();
                let mut frames = sse_buf.feed(head);
                if !frames.is_empty() {
                    frames.extend(sse_buf.feed(tail));
                    break frames;
                }
                if read >= PRE_FRAME_CAP {
                    let truncated = true;
                    return Err(zero_event_error(
                        &errors,
                        content_type.as_deref(),
                        capture,
                        truncated,
                    ));
                }
            }
            Some(Err(e)) => {
                return Ok(Box::pin(futures::stream::once(async move {
                    StreamChunk::Error {
                        content: format!("Stream read error: {}", e),
                    }
                })));
            }
            None => {
                ended = true;
                let frames = sse_buf.finish();
                if frames.is_empty() {
                    let truncated = read > capture.len();
                    return Err(zero_event_error(
                        &errors,
                        content_type.as_deref(),
                        capture,
                        truncated,
                    ));
                }
                break frames;
            }
        }
    };
    let mut pending = VecDeque::new();
    for frame in &first {
        pending.extend(transformer.transform(frame));
    }
    let stream = futures::stream::unfold(
        (byte_stream, sse_buf, transformer, pending, ended),
        |(mut byte_stream, mut sse_buf, mut transformer, mut pending, mut ended)| async move {
            loop {
                // Drain pending chunks first (FIFO order)
                if let Some(chunk) = pending.pop_front() {
                    return Some((chunk, (byte_stream, sse_buf, transformer, pending, ended)));
                }
                // An in-stream error is the last thing this stream says.
                if ended || transformer.failed() {
                    return None;
                }
                match byte_stream.next().await {
                    Some(Ok(bytes)) => {
                        for frame in sse_buf.feed(bytes.as_ref()) {
                            pending.extend(transformer.transform(&frame));
                        }
                    }
                    Some(Err(e)) => {
                        ended = true;
                        return Some((
                            StreamChunk::Error {
                                content: format!("Stream read error: {}", e),
                            },
                            (byte_stream, sse_buf, transformer, pending, ended),
                        ));
                    }
                    None => {
                        // An unterminated last event is still an event.
                        ended = true;
                        for frame in sse_buf.finish() {
                            pending.extend(transformer.transform(&frame));
                        }
                    }
                }
            }
        },
    );
    Ok(Box::pin(stream))
}

/// One logical completion request for one resolved credential: the encoded
/// body, its headers and the request-scoped identity state.
struct ChatAttempt {
    client: reqwest::Client,
    request: reqwest::Request,
    state: Option<Box<dyn RequestState>>,
    profile: Arc<dyn ChatProfile>,
    /// The model id this request sent.
    model: String,
}

impl ChatAttempt {
    /// Re-issues send the stored request, which carries a won identity.
    async fn exchange(&mut self) -> Result<Outcome, ProviderError> {
        http_retry::send_adopting(
            &self.client,
            &mut self.request,
            self.state
                .as_deref_mut()
                .map(|s| s as &mut dyn RequestState),
            self.profile.retry(),
        )
        .await
    }
    fn error(&self, status: u16, headers: &HeaderMap, body: &str) -> ProviderError {
        self.profile
            .error(status, headers, body, self.request.headers(), &self.model)
            .unwrap_or_else(|| legacy_error(status, headers, body))
    }
    /// How a 2xx answer of this attempt words an error it carries.
    fn in_band(&self, response: &reqwest::Response) -> Arc<InBandText> {
        Arc::new(InBandText::new(
            self.profile.clone(),
            self.request.headers().clone(),
            self.model.clone(),
            response.status().as_u16(),
        ))
    }
}

#[async_trait]
impl stream_retry::Attempt for ChatAttempt {
    /// Every failed open is final here: the Chat route's HTTP attempts already
    /// ran under `OPENAI_WIRE`.
    async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, stream_retry::Rejected> {
        match self.exchange().await? {
            Outcome::Success(response) => {
                let errors = self.in_band(&response);
                Ok(open_stream(response, errors).await?)
            }
            Outcome::Failure {
                status,
                headers,
                body,
            } => Err(self.error(status, &headers, &body).into()),
        }
    }
}

impl OpenAiAdapter {
    /// Create a new OpenAiAdapter.
    ///
    /// # Arguments
    /// * `credential` — a fixed key (validated here) or a per-request handle
    /// * `model` — Model identifier (e.g., `moonshot-v1-auto`, `gpt-4o`)
    /// * `profile` — base URL and request policy (`StaticChatProfile` for a fixed base)
    pub fn new(
        variant: OpenAiCompatibleVariant,
        credential: ChatCredential,
        model: String,
        profile: Arc<dyn ChatProfile>,
    ) -> Result<Self, ProviderError> {
        if let ChatCredential::Fixed(key) = &credential {
            let header = authorization(key)?;
            if header.is_none() && !matches!(variant, OpenAiCompatibleVariant::Custom { .. }) {
                return Err(ProviderError::AuthenticationFailed);
            }
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

        Ok(Self {
            client,
            credential,
            model,
            variant,
            profile,
            abort_handle: Arc::new(Mutex::new(None)),
            discovered_models: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
        })
    }

    /// The credential of one logical request.
    async fn resolve(&self) -> Result<Resolved, ProviderError> {
        match &self.credential {
            ChatCredential::Fixed(key) => Ok(Resolved {
                source: AuthSource::Runtime,
                auth: ResolvedAuth::ApiKey(key.clone()),
                row: None,
                identity: None,
            }),
            ChatCredential::Resolved(handle) => handle.current().await,
        }
    }

    /// One recovery per logical request; only a rejected stored grant can recover.
    async fn recover(
        &self,
        resolved: &mut Resolved,
        used: &mut bool,
    ) -> Result<bool, ProviderError> {
        if *used {
            return Ok(false);
        }
        *used = true;
        if let ChatCredential::Resolved(handle) = &self.credential
            && let Some(refreshed) = handle.recover(resolved).await?
        {
            *resolved = refreshed;
            return Ok(true);
        }
        Ok(false)
    }

    /// `GET <base>/models` with the bearer only, recovering a rejected stored grant once.
    async fn get_models(&self) -> Result<(reqwest::Response, Resolved), ProviderError> {
        let mut resolved = self.resolve().await?;
        let mut recovered = false;
        loop {
            let url = format!("{}/models", self.profile.base_url(&resolved)?);
            let mut req = self
                .client
                .get(&url)
                .timeout(std::time::Duration::from_secs(5));
            if let Some(value) = authorization(&*self.profile.bearer(&resolved)?)? {
                req = req.header("authorization", value);
            }
            let response = req
                .send()
                .await
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
            if response.status().as_u16() == 401
                && self.recover(&mut resolved, &mut recovered).await?
            {
                continue;
            }
            return Ok((response, resolved));
        }
    }

    /// Fetch model catalog from the provider's `/v1/models` endpoint; also returns
    /// the credential the successful request sent.
    async fn fetch_models(
        &self,
        model_filter: &[String],
    ) -> Result<(Vec<ModelRecord>, Resolved), ProviderError> {
        let (resp, resolved) = self.get_models().await?;
        match resp.status().as_u16() {
            200..=299 => {
                let text = resp.text().await.map_err(|e| {
                    ProviderError::Other(format!("Failed to read models response: {}", e))
                })?;
                crate::adapters::openai::discovery::parse_and_filter_models(
                    &text,
                    &self.variant,
                    model_filter,
                )
                .map(|records| (records, resolved))
            }
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            code @ (404 | 405) => Err(ProviderError::EndpointUnsupported(code)),
            _ => Err(ProviderError::Other(format!(
                "Models fetch failed: HTTP {}",
                resp.status()
            ))),
        }
    }

    /// Fetch model catalog from the provider's `/v1/models` endpoint.
    pub async fn fetch_remote_models(
        &self,
        model_filter: &[String],
    ) -> Result<Vec<ModelRecord>, ProviderError> {
        self.fetch_models(model_filter)
            .await
            .map(|(records, _)| records)
    }

    /// The request of one attempt set: headers are the bearer, the profile's,
    /// then the request state's (which override).
    fn attempt(
        &self,
        resolved: &Resolved,
        messages: &[Message],
        options: &CompletionOptions,
    ) -> Result<ChatAttempt, ProviderError> {
        let ctx = RequestContext { messages, options };
        let compat = self.profile.compat(resolved, &ctx);
        let request_body = OpenAiRequest::new(messages, options, &compat);
        let base = self.profile.base_url(resolved)?;
        let url = format!("{base}/chat/completions");
        tracing::debug!(
            target_url = %url,
            model = %options.model,
            message_count = messages.len(),
            "Sending OpenAI-compatible completion request"
        );
        tracing::trace!(
            request_body = %serde_json::to_string(&request_body).unwrap_or_default(),
            "Full request body"
        );
        let body = serde_json::to_vec(&request_body)
            .map_err(|_| ProviderError::Other("Could not encode completion request".into()))?;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        if let Some(value) = authorization(&*self.profile.bearer(resolved)?)? {
            headers.insert("authorization", value);
        }
        self.profile.headers(resolved, &ctx, &mut headers);
        let state = self.profile.begin(resolved, &base, &ctx);
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
        Ok(ChatAttempt {
            client: self.client.clone(),
            request,
            state,
            profile: self.profile.clone(),
            model: options.model.clone(),
        })
    }
}

impl fmt::Debug for OpenAiAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let credential = match &self.credential {
            ChatCredential::Fixed(_) => "Fixed(***)",
            ChatCredential::Resolved(_) => "Resolved(***)",
        };
        f.debug_struct("OpenAiAdapter")
            .field("variant", &self.variant)
            .field("api_key", &credential)
            .field("model", &self.model)
            .field("profile", &self.profile)
            .finish()
    }
}

impl OpenAiAdapter {
    /// One logical Chat request. `resolved` is the credential a wrapping
    /// adapter already read for this request, so its request shaping and the
    /// request itself come from one store read; `None` resolves here.
    pub(crate) async fn stream_with(
        &self,
        resolved: Option<Resolved>,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, ProviderError> {
        if messages.is_empty() {
            return Err(ProviderError::Other(
                "Cannot send empty messages list to API".to_string(),
            ));
        }

        let resilience = self.profile.resilience();
        let mut resolved = match resolved {
            Some(resolved) => resolved,
            None => self.resolve().await?,
        };
        let mut recovered = false;
        loop {
            // The first-event watchdog is armed once the credential is read.
            let deadline = resilience.map(|_| stream_retry::deadline());
            let mut attempt = self.attempt(&resolved, &messages, &options)?;
            let outcome = match (resilience, deadline) {
                (Some(policy), Some(deadline)) => {
                    stream_retry::first_open(deadline, policy, attempt.exchange()).await?
                }
                _ => attempt.exchange().await?,
            };
            match outcome {
                Outcome::Success(response) => {
                    tracing::debug!(status = %response.status(), "Response received");
                    let errors = attempt.in_band(&response);
                    let first = match (resilience, deadline) {
                        (Some(policy), Some(deadline)) => {
                            stream_retry::first_open(
                                deadline,
                                policy,
                                open_stream(response, errors),
                            )
                            .await?
                        }
                        _ => open_stream(response, errors).await?,
                    };
                    return Ok(match (resilience, deadline) {
                        (Some(policy), Some(deadline)) => {
                            stream_retry::resume(first, Box::new(attempt), policy, deadline)
                        }
                        _ => first,
                    });
                }
                Outcome::Failure { status: 401, .. }
                    if self.recover(&mut resolved, &mut recovered).await? =>
                {
                    continue;
                }
                Outcome::Failure {
                    status,
                    headers,
                    body,
                } => return Err(attempt.error(status, &headers, &body)),
            }
        }
    }
}

#[async_trait]
impl StreamingProvider for OpenAiAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, ProviderError> {
        self.stream_with(None, messages, options).await
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        let mut handle = self.abort_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        Ok(())
    }

    fn provider_id(&self) -> String {
        self.variant.provider_id().to_string()
    }

    /// Only a fixed key is known before a request resolves one.
    fn discovery_credential_fingerprint(&self, context: &str) -> Option<String> {
        use crate::adapters::model_catalog_cache::{ScopeCredential, credential_scope};
        match &self.credential {
            ChatCredential::Fixed(key) => {
                Some(credential_scope(context, ScopeCredential::Secret(key)))
            }
            ChatCredential::Resolved(_) => None,
        }
    }

    /// Credentials resolve per request, so the scope is read through that same path.
    async fn discovery_scope(&self, scope_context: &str) -> Option<String> {
        let resolved = self.resolve().await.ok()?;
        Some(crate::adapters::model_catalog_cache::request_scope(
            scope_context,
            &resolved,
        ))
    }

    fn default_context_window(&self, model_id: &str) -> Option<u32> {
        Some(
            self.variant
                .context_window_for(model_id)
                .unwrap_or_else(|| allowlists::variant_default_context(&self.variant)),
        )
    }

    async fn discover_models(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Option<Result<crate::domain::models::provider::DiscoveredModels, ProviderError>> {
        // The records are stamped with the scope of the credential actually sent.
        Some(
            self.fetch_models(model_filter)
                .await
                .map(
                    |(records, resolved)| crate::domain::models::provider::DiscoveredModels {
                        records,
                        credential_scope: crate::adapters::model_catalog_cache::request_scope(
                            scope_context,
                            &resolved,
                        ),
                    },
                ),
        )
    }

    fn set_discovered_models(&self, models: Vec<ModelDescriptor>) {
        self.discovered_models.store(Arc::new(Some(models)));
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        let guard = self.discovered_models.load();
        match &**guard {
            Some(models) => models.clone(),
            None => self.variant.known_models(&self.model),
        }
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let (resp, _) = self.get_models().await?;
        match resp.status().as_u16() {
            200..=299 => Ok(()),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            // A gateway without `/models` still answered: D4, not an outage.
            code @ (404 | 405) => Err(ProviderError::EndpointUnsupported(code)),
            _ => Err(ProviderError::Other(format!(
                "Health check failed: HTTP {}",
                resp.status()
            ))),
        }
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
        use std::time::Instant;
        let start = Instant::now();
        // Non-billable: GET /v1/models (free, idempotent, validates auth).
        // base_url is expected to end with "/v1" (e.g. https://api.openai.com/v1).
        let (resp, _) = self.get_models().await?;
        let latency = start.elapsed();
        let status = resp.status().as_u16();
        match status {
            200..=299 => Ok(crate::domain::ports::ProbeOutcome { latency }),
            401 | 403 => Err(ProviderError::AuthenticationFailed),
            404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
            _ => Err(ProviderError::Other(format!(
                "Probe failed: HTTP {}",
                status
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed-key adapter on `base_url`, or the variant's default base.
    fn fixed(
        variant: OpenAiCompatibleVariant,
        key: String,
        model: String,
        base_url: Option<String>,
    ) -> Result<OpenAiAdapter, ProviderError> {
        let profile = StaticChatProfile::new(
            &base_url.unwrap_or_else(|| variant.default_base_url().to_string()),
        );
        OpenAiAdapter::new(
            variant,
            ChatCredential::Fixed(SecretString::new(key)),
            model,
            profile,
        )
    }

    #[test]
    fn request_serializes_stream_options_include_usage() {
        // The fix for non-Anthropic token tracking: the request must ask the
        // server for usage data via stream_options.include_usage = true.
        use crate::adapters::openai::types::OpenAiRequest;
        use crate::domain::models::{CompletionOptions, Message, MessageRole};

        let msgs: Vec<Message> = vec![Message {
            role: MessageRole::User,
            content: "hi".to_string(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
            provider_replay: vec![],
        }];
        let opts = CompletionOptions {
            session_id: None,
            model: "gpt-4o".to_string(),
            max_tokens: 4096,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        };
        let req: OpenAiRequest = (&msgs[..], &opts).into();

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["stream"], true, "streaming is always on");
        assert_eq!(
            json["stream_options"]["include_usage"], true,
            "request must request usage stats"
        );
    }

    #[test]
    fn test_openai_adapter_debug_masks_api_key() {
        let adapter = fixed(
            OpenAiCompatibleVariant::Moonshot,
            "sk-test-secret-key".to_string(),
            "moonshot-v1-auto".to_string(),
            None,
        )
        .unwrap();

        let debug_output = format!("{:?}", adapter);
        assert!(!debug_output.contains("sk-test-secret-key"));
        assert!(debug_output.contains("***"));
        assert!(debug_output.contains("moonshot-v1-auto"));
    }

    #[test]
    fn test_openai_adapter_missing_api_key() {
        let result = fixed(
            OpenAiCompatibleVariant::Moonshot,
            String::new(),
            "moonshot-v1-auto".to_string(),
            None,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::AuthenticationFailed => {}
            other => panic!("Expected AuthenticationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_openai_adapter_custom_variant_allows_empty_key() {
        let result = fixed(
            OpenAiCompatibleVariant::Custom {
                provider_id: "local".to_string(),
                display_name: "Local".to_string(),
                context_window: None,
                supports_tools: None,
            },
            String::new(),
            "m".to_string(),
            Some("http://localhost:8080/v1".to_string()),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_openai_adapter_provider_id() {
        let adapter = fixed(
            OpenAiCompatibleVariant::Moonshot,
            "test-key".to_string(),
            "moonshot-v1-auto".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(adapter.provider_id(), "moonshot");
    }

    #[test]
    fn test_openai_adapter_custom_base_url() {
        let adapter = fixed(
            OpenAiCompatibleVariant::Moonshot,
            "test-key".to_string(),
            "moonshot-v1-auto".to_string(),
            Some("https://api.moonshot.cn/v1".to_string()),
        )
        .unwrap();
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("api.moonshot.cn"));
    }

    #[test]
    fn test_api_key_never_in_serialized_output() {
        let key = "sk-super-secret-kimi-key-12345";
        let adapter = fixed(
            OpenAiCompatibleVariant::Moonshot,
            key.to_string(),
            "moonshot-v1-auto".to_string(),
            None,
        )
        .unwrap();

        let debug = format!("{:?}", adapter);
        assert!(!debug.contains(key), "API key leaked in Debug output");
    }

    #[test]
    fn test_set_discovered_models_overlay() {
        let adapter = fixed(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            None,
        )
        .unwrap();

        let m1 = ModelDescriptor {
            model_id: "m1".to_string(),
            display_name: "M1".to_string(),
            provider_id: "openai".to_string(),
            context_window: 128_000,
            capabilities: std::collections::HashSet::new(),
            pricing_tier: None,
            stale: false,
            wire_model_id: None,
        };
        let m2 = ModelDescriptor {
            model_id: "m2".to_string(),
            display_name: "M2".to_string(),
            provider_id: "openai".to_string(),
            context_window: 128_000,
            capabilities: std::collections::HashSet::new(),
            pricing_tier: None,
            stale: false,
            wire_model_id: None,
        };
        let provider: &dyn StreamingProvider = &adapter;
        provider.set_discovered_models(vec![m1, m2]);
        let models = adapter.list_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id, "m1");
        assert_eq!(models[1].model_id, "m2");
    }

    #[test]
    fn test_empty_discovered_models_stays_empty() {
        let adapter = fixed(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            None,
        )
        .unwrap();

        assert!(!adapter.list_models().is_empty());
        adapter.set_discovered_models(vec![]);
        assert!(adapter.list_models().is_empty());
    }

    #[tokio::test]
    async fn discovery_and_health_classify_auth_and_unsupported_endpoints() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [401, 403, 404, 405] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .and(header("authorization", "Bearer test-key"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let adapter = fixed(
                OpenAiCompatibleVariant::OpenAI,
                "test-key".to_string(),
                "gpt-4o".to_string(),
                Some(format!("{}/v1", server.uri())),
            )
            .unwrap();
            let provider: &dyn StreamingProvider = &adapter;
            let error = provider
                .discover_models(&[], "scope")
                .await
                .unwrap()
                .unwrap_err();
            match status {
                401 | 403 => {
                    assert!(matches!(error, ProviderError::AuthenticationFailed));
                    assert!(matches!(
                        provider.health_check().await,
                        Err(ProviderError::AuthenticationFailed)
                    ));
                }
                _ => {
                    assert!(
                        matches!(error, ProviderError::EndpointUnsupported(code) if code == status)
                    );
                    assert!(matches!(
                        provider.health_check().await,
                        Err(ProviderError::EndpointUnsupported(code)) if code == status
                    ));
                }
            }
        }
    }

    #[tokio::test]
    async fn discovery_returns_sparse_records_over_provider_port() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data": [{"id": "gpt-sparse", "object": "model"}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let adapter = fixed(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            Some(format!("{}/v1", server.uri())),
        )
        .unwrap();
        let provider: &dyn StreamingProvider = &adapter;
        let models = provider
            .discover_models(&["*".to_string()], "scope")
            .await
            .unwrap()
            .unwrap()
            .records;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "gpt-sparse");
        assert!(models[0].display_name.is_none());
        assert!(models[0].context_window.is_none());
        assert!(models[0].capabilities.is_none());
        assert!(models[0].pricing_tier.is_none());
    }

    mod tool_call_ordering {
        use super::*;
        use crate::domain::models::{MessageRole, ToolResultMessage, ToolUseMessage};

        fn make_options() -> CompletionOptions {
            CompletionOptions {
                session_id: None,
                model: "test-model".to_string(),
                max_tokens: 4096,
                system_prompt: String::new(),
                temperature: None,
                tools: vec![],
            }
        }

        #[test]
        fn test_tool_results_emitted_before_user_text() {
            // Reproduce the bug scenario: build_api_messages produces
            // [User, Assistant(tool_uses), User(text + tool_results)]
            // The OpenAI adapter must emit tool messages BEFORE the user text
            // so the API sees: [user, assistant(tool_calls), tool, user]
            let tool_use = ToolUseMessage {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
            };
            let tool_result = ToolResultMessage {
                tool_use_id: "call_1".to_string(),
                content: "src/ tests/".to_string(),
                is_error: false,
                images: Vec::new(),
            };

            let messages = [
                Message {
                    role: MessageRole::User,
                    content: "what files are in .?".to_string(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
                Message {
                    role: MessageRole::Assistant,
                    content: "Let me check".to_string(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![tool_use],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
                Message {
                    role: MessageRole::User,
                    content: "what about hidden files?".to_string(),
                    images: vec![],
                    tool_results: vec![tool_result],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
            ];

            let request = OpenAiRequest::from((&messages[..], &make_options()));

            assert_eq!(
                request.messages.len(),
                4,
                "expected 4 messages: [user, assistant(tool_calls), tool, user]"
            );

            assert_eq!(request.messages[0].role, "user");
            assert_eq!(request.messages[0].content, "what files are in .?");

            assert_eq!(request.messages[1].role, "assistant");
            assert_eq!(request.messages[1].content, "Let me check");
            let tc = request.messages[1].tool_calls.as_ref().unwrap();
            assert_eq!(tc.len(), 1);
            assert_eq!(tc[0].id, "call_1");

            // Tool message MUST come before the follow-up user text
            assert_eq!(request.messages[2].role, "tool");
            assert_eq!(request.messages[2].tool_call_id.as_deref(), Some("call_1"));
            assert_eq!(request.messages[2].content, "src/ tests/");

            assert_eq!(request.messages[3].role, "user");
            assert_eq!(request.messages[3].content, "what about hidden files?");
        }

        #[test]
        fn test_tool_results_empty_content_skips_text_message() {
            // Reproduce the inner turn loop pattern: User message with empty
            // content and tool_results. The adapter should only emit tool
            // messages (no empty user text message).
            let tool_use = ToolUseMessage {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
            };
            let tool_result = ToolResultMessage {
                tool_use_id: "call_1".to_string(),
                content: "src/ tests/".to_string(),
                is_error: false,
                images: Vec::new(),
            };

            let messages = [
                Message {
                    role: MessageRole::User,
                    content: "what files are in .?".to_string(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
                Message {
                    role: MessageRole::Assistant,
                    content: "Let me check".to_string(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![tool_use],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
                Message {
                    role: MessageRole::User,
                    content: String::new(), // empty — inner-loop pattern
                    images: vec![],
                    tool_results: vec![tool_result],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
            ];

            let request = OpenAiRequest::from((&messages[..], &make_options()));

            assert_eq!(
                request.messages.len(),
                3,
                "expected 3 messages: [user, assistant(tool_calls), tool] — no empty user"
            );

            assert_eq!(request.messages[0].role, "user");
            assert_eq!(request.messages[1].role, "assistant");
            assert_eq!(request.messages[2].role, "tool");
            assert_eq!(request.messages[2].tool_call_id.as_deref(), Some("call_1"));
        }

        #[test]
        fn test_tool_results_with_error_flag() {
            let tool_use = ToolUseMessage {
                id: "call_err".to_string(),
                name: "broken_tool".to_string(),
                input: serde_json::json!({}),
            };
            let tool_result = ToolResultMessage {
                tool_use_id: "call_err".to_string(),
                content: "command not found".to_string(),
                is_error: true,
                images: Vec::new(),
            };

            let messages = [
                Message {
                    role: MessageRole::Assistant,
                    content: String::new(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![tool_use],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
                Message {
                    role: MessageRole::User,
                    content: String::new(),
                    images: vec![],
                    tool_results: vec![tool_result],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                    provider_replay: vec![],
                },
            ];

            let request = OpenAiRequest::from((&messages[..], &make_options()));

            assert_eq!(request.messages.len(), 2);
            assert_eq!(request.messages[1].role, "tool");
            assert_eq!(
                request.messages[1].tool_call_id.as_deref(),
                Some("call_err")
            );
            assert!(request.messages[1].content.text().starts_with("Error: "));
        }
    }
}

/// The open path of a 2xx answer, fed real `reqwest::Response`s from a loopback
/// stub (cp-free-2 AD-3).
#[cfg(test)]
mod open_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "SECRETTOKEN12345";

    async fn answer(template: ResponseTemplate) -> (MockServer, reqwest::Response) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(template)
            .mount(&server)
            .await;
        let response = reqwest::get(format!("{}/a", server.uri())).await.unwrap();
        (server, response)
    }

    fn errors() -> Arc<InBandText> {
        let mut sent = HeaderMap::new();
        sent.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
        );
        Arc::new(InBandText::new(
            StaticChatProfile::new("http://stub.invalid/v1"),
            sent,
            "m".into(),
            200,
        ))
    }

    async fn open(template: ResponseTemplate) -> Result<Vec<StreamChunk>, String> {
        let (_server, response) = answer(template).await;
        match open_stream(response, errors()).await {
            Ok(stream) => Ok(stream.collect().await),
            Err(ProviderError::Other(text)) => Err(text),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    const FRAME: &str = r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#;
    const STOP: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

    fn text(chunks: &[StreamChunk]) -> String {
        chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Text { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn an_empty_body_is_an_error_naming_status_and_content_type() {
        let with_type =
            open(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream"))
                .await
                .unwrap_err();
        assert!(
            with_type.contains("200") && with_type.contains("text/event-stream"),
            "{with_type}"
        );
        assert!(with_type.contains("empty body"), "{with_type}");
        let bare = open(ResponseTemplate::new(200)).await.unwrap_err();
        assert!(bare.contains("no content-type"), "{bare}");
    }

    #[tokio::test]
    async fn a_json_error_body_gives_its_code_and_message_without_the_bearer() {
        let text = open(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"error":{"message":format!("nope {TOKEN}"),"code":"zz_code"}}),
        ))
        .await
        .unwrap_err();
        assert!(text.contains("zz_code") && text.contains("nope"), "{text}");
        assert!(text.contains("application/json"), "{text}");
        assert!(!text.contains(TOKEN), "{text}");
    }

    #[tokio::test]
    async fn an_html_page_is_a_bounded_control_free_prefix() {
        let page = format!(
            "<title>Blocked</title>\u{1b}[2J\u{7}{TOKEN}{}",
            "x".repeat(5000)
        );
        let text = open(ResponseTemplate::new(200).set_body_raw(page.into_bytes(), "text/html"))
            .await
            .unwrap_err();
        assert!(
            text.contains("Blocked") && text.contains("text/html"),
            "{text}"
        );
        assert!(!text.contains(TOKEN), "{text}");
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(text.chars().count() < 700, "{}", text.chars().count());
    }

    #[tokio::test]
    async fn junk_then_a_frame_and_keep_alives_then_a_frame_are_streams() {
        let body = format!(
            ": keep-alive\n\n: still here\n\nnot sse at all\ndata: {FRAME}\n\ndata: {STOP}\n\n"
        );
        let chunks = open(ResponseTemplate::new(200).set_body_string(body))
            .await
            .unwrap();
        assert_eq!(text(&chunks), "hi", "{chunks:?}");
        assert!(
            matches!(chunks.last(), Some(StreamChunk::TurnComplete { .. })),
            "{chunks:?}"
        );
    }

    #[tokio::test]
    async fn a_sole_unterminated_data_line_is_a_frame() {
        let chunks = open(ResponseTemplate::new(200).set_body_string(format!("data: {FRAME}")))
            .await
            .unwrap();
        assert_eq!(text(&chunks), "hi", "{chunks:?}");
    }

    #[tokio::test]
    async fn a_body_past_the_pre_frame_cap_is_not_read_to_its_frame() {
        let body = format!(
            "{}\ndata: {FRAME}\n\n",
            "x".repeat(PRE_FRAME_CAP + 2 * 1024 * 1024)
        );
        let text = open(ResponseTemplate::new(200).set_body_string(body))
            .await
            .unwrap_err();
        assert!(
            text.contains("200") && text.contains("without a stream event"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_bearer_cut_by_the_capture_end_leaves_no_prefix() {
        let pad = format!(
            "{}{}",
            "é".repeat(100),
            "a".repeat(CAPTURE_CAP - 8 - 1 - 200)
        );
        let body = format!("{pad} {TOKEN} and more text after the token");
        let text = open(ResponseTemplate::new(200).set_body_string(body))
            .await
            .unwrap_err();
        for start in 0..=TOKEN.len() - 8 {
            assert!(!text.contains(&TOKEN[start..start + 8]), "{text}");
        }
    }

    #[test]
    fn a_truncated_capture_drops_the_token_it_cuts() {
        let capture = format!("ééé blocked {}", &TOKEN[..8]).into_bytes();
        let ProviderError::Other(text) =
            zero_event_error(&errors(), Some("text/html"), capture, true)
        else {
            panic!("expected the zero-event error");
        };
        assert!(text.contains("blocked"), "{text}");
        assert!(!text.contains(&TOKEN[..8]), "{text}");
        // Whole (not truncated), the same capture keeps its last word.
        let capture = format!("ééé blocked {}", &TOKEN[..8]).into_bytes();
        let ProviderError::Other(text) =
            zero_event_error(&errors(), Some("text/html"), capture, false)
        else {
            panic!("expected the zero-event error");
        };
        assert!(text.contains(&TOKEN[..8]), "{text}");
    }

    // ── patch round 1 ───────────────────────────────────────────────────────

    type Part = Result<Vec<u8>, String>;

    fn body(parts: Vec<Vec<u8>>) -> futures::stream::Iter<std::vec::IntoIter<Part>> {
        futures::stream::iter(parts.into_iter().map(Ok).collect::<Vec<_>>())
    }

    async fn opened<S>(source: S) -> Result<Vec<StreamChunk>, String>
    where
        S: futures::Stream<Item = Part> + Unpin + Send + 'static,
    {
        match open_chunks(source, Some("text/event-stream".into()), errors()).await {
            Ok(stream) => Ok(tokio::time::timeout(
                std::time::Duration::from_secs(30),
                stream.collect(),
            )
            .await
            .expect("an error ends the stream; nothing may keep it pending")),
            Err(ProviderError::Other(text)) => Err(text),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    /// CP2-R4: a chunk crossing the cap is fed only up to it; a frame that
    /// completes after the boundary does not count.
    #[tokio::test]
    async fn a_single_chunk_crossing_the_cap_with_its_first_frame_after_it_is_rejected() {
        let mut chunk = "x".repeat(PRE_FRAME_CAP - 5).into_bytes();
        chunk.extend_from_slice(format!("\ndata: {FRAME}\n\n").as_bytes());
        let text = opened(body(vec![chunk])).await.unwrap_err();
        assert!(text.contains("without a stream event"), "{text}");
    }

    /// CP2-R4: a first frame inside the allowance is kept, and the remainder of
    /// the same chunk still streams.
    #[tokio::test]
    async fn a_single_chunk_crossing_the_cap_with_its_first_frame_inside_it_streams_the_rest() {
        let mut chunk = format!("data: {FRAME}\n\n").into_bytes();
        chunk.extend_from_slice("x".repeat(PRE_FRAME_CAP).as_bytes());
        chunk.extend_from_slice(format!("\ndata: {STOP}\n\n").as_bytes());
        let chunks = opened(body(vec![chunk])).await.unwrap();
        assert_eq!(text(&chunks), "hi", "{chunks:?}");
        assert!(
            matches!(chunks.last(), Some(StreamChunk::TurnComplete { .. })),
            "{chunks:?}"
        );
    }

    /// CP2-R2: tool call, error, `[DONE]` is the Error alone, and an error
    /// followed by a body that never ends does not keep the stream pending.
    #[tokio::test]
    async fn an_error_ends_the_stream_without_the_tool_and_without_waiting_for_the_body() {
        let tool = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"Bash","arguments":"{}"}}]}}]}"#;
        let sequence = format!(
            "data: {tool}\n\ndata: {{\"error\":{{\"message\":\"request rejected\"}}}}\n\ndata: [DONE]\n\n"
        );
        let chunks = opened(body(vec![sequence.into_bytes()])).await.unwrap();
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert!(
            matches!(&chunks[0], StreamChunk::Error { .. }),
            "{chunks:?}"
        );

        let held = body(vec![
            b"data: {\"error\":{\"message\":\"request rejected\"}}\n\n".to_vec(),
        ])
        .chain(futures::stream::pending::<Part>());
        let chunks = opened(Box::pin(held)).await.unwrap();
        assert_eq!(chunks.len(), 1, "{chunks:?}");

        // An error in a later chunk ends the stream the same way.
        let held = body(vec![
            format!("data: {FRAME}\n\n").into_bytes(),
            b"data: {\"error\":{\"message\":\"request rejected\"}}\n\n".to_vec(),
        ])
        .chain(futures::stream::pending::<Part>());
        let chunks = opened(Box::pin(held)).await.unwrap();
        assert_eq!(text(&chunks), "hi", "{chunks:?}");
        assert!(
            matches!(chunks.last(), Some(StreamChunk::Error { .. })),
            "{chunks:?}"
        );
    }

    /// CP2-R1: a credential split by an escape sequence is rebuilt by the
    /// normaliser, so it is removed after normalising, in code, message and body.
    #[test]
    fn an_escape_split_credential_does_not_survive_normalising() {
        let split = format!("{}\u{1b}[0m{}", &TOKEN[..6], &TOKEN[6..]);
        let capture = format!("<html>{split}</html>").into_bytes();
        let ProviderError::Other(text) = zero_event_error(&errors(), None, capture, false) else {
            panic!("expected the zero-event error");
        };
        assert!(
            !text.contains(TOKEN) && !text.contains(&TOKEN[..6]),
            "{text}"
        );
        let capture = serde_json::json!({"error":{"code":split,"message":format!("m {split}")}})
            .to_string()
            .into_bytes();
        let ProviderError::Other(text) = zero_event_error(&errors(), None, capture, false) else {
            panic!("expected the zero-event error");
        };
        assert!(
            !text.contains(TOKEN) && !text.contains(&TOKEN[..6]),
            "{text}"
        );
    }

    /// CP2-R6: code and message share ONE 500-character bound.
    #[test]
    fn code_and_message_share_one_bound() {
        let capture =
            serde_json::json!({"error":{"code":"c".repeat(600),"message":"m".repeat(600)}})
                .to_string()
                .into_bytes();
        let ProviderError::Other(long) =
            zero_event_error(&errors(), Some("application/json"), capture, false)
        else {
            panic!("expected the zero-event error");
        };
        let ProviderError::Other(empty) =
            zero_event_error(&errors(), Some("application/json"), Vec::new(), false)
        else {
            panic!("expected the zero-event error");
        };
        let fixed = empty.chars().count() - "empty body".len();
        assert_eq!(
            long.chars().count(),
            fixed + crate::adapters::auth::PROVIDER_TEXT_CAP,
            "{long}"
        );
        // Short code and message stay whole.
        let capture = br#"{"error":{"code":"zz","message":"nope"}}"#.to_vec();
        let ProviderError::Other(short) = zero_event_error(&errors(), None, capture, false) else {
            panic!("expected the zero-event error");
        };
        assert!(short.ends_with("zz: nope"), "{short}");
    }

    /// CP2-R3: the zero-event WARN logs only the safe text: a valid
    /// content-type value carrying the bearer never reaches the log.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn the_zero_event_warning_never_logs_the_bearer_from_a_header() {
        let template = ResponseTemplate::new(200)
            .insert_header("content-type", format!("text/html; diagnostic={TOKEN}"));
        let shown = open(template).await.unwrap_err();
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(logs_contain("Chat answer carried no stream event"));
        assert!(!logs_contain(TOKEN));
    }
}
