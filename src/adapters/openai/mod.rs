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

pub use profile::{ChatCompat, ChatImages, ChatProfile, StaticChatProfile};
pub use variant::OpenAiCompatibleVariant;

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
fn authorization(bearer: &SecretString) -> Result<Option<HeaderValue>, ProviderError> {
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

/// SSE bytes → `StreamChunk`s of one response.
fn chunks(response: reqwest::Response) -> BoxStream<'static, StreamChunk> {
    let byte_stream = response.bytes_stream();
    let stream = futures::stream::unfold(
        (
            byte_stream,
            SseLineBuffer::new(),
            OpenAiStreamTransformer::new(),
            std::collections::VecDeque::new(),
        ),
        |(mut byte_stream, mut sse_buf, mut transformer, mut pending)| async move {
            loop {
                // Drain pending chunks first (FIFO order)
                if let Some(chunk) = pending.pop_front() {
                    return Some((chunk, (byte_stream, sse_buf, transformer, pending)));
                }

                // Get next bytes from HTTP stream
                match byte_stream.next().await {
                    Some(Ok(bytes)) => {
                        let frames = sse_buf.feed(&bytes);
                        for frame in frames {
                            let chunks = transformer.transform(&frame);
                            pending.extend(chunks);
                        }
                        // Loop back to drain pending
                    }
                    Some(Err(e)) => {
                        return Some((
                            StreamChunk::Error {
                                content: format!("Stream read error: {}", e),
                            },
                            (byte_stream, sse_buf, transformer, pending),
                        ));
                    }
                    None => return None, // Stream ended
                }
            }
        },
    );
    Box::pin(stream)
}

/// One logical completion request for one resolved credential: the encoded
/// body, its headers and the request-scoped identity state.
struct ChatAttempt {
    client: reqwest::Client,
    request: reqwest::Request,
    state: Option<Box<dyn RequestState>>,
    profile: Arc<dyn ChatProfile>,
}

impl ChatAttempt {
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
impl stream_retry::Attempt for ChatAttempt {
    /// Every failed open is final here: the Chat route's HTTP attempts already
    /// ran under `OPENAI_WIRE`.
    async fn open(&mut self) -> Result<BoxStream<'static, StreamChunk>, stream_retry::Rejected> {
        match self.exchange().await? {
            Outcome::Success(response) => Ok(chunks(response)),
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
        // The first-event watchdog is armed before the first attempt.
        let deadline = resilience.map(|_| stream_retry::deadline());
        let mut resolved = match resolved {
            Some(resolved) => resolved,
            None => self.resolve().await?,
        };
        let mut recovered = false;
        loop {
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
                    let first = chunks(response);
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
