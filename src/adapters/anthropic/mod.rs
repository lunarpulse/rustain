//! Anthropic API adapter implementing the `StreamingProvider` trait.
//! All Anthropic-specific types and SSE parsing live in this module.

pub mod claude_code;
pub mod sse;
pub mod stream;
pub mod types;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::domain::errors::ProviderError;
use crate::domain::models::{CompletionOptions, Message, StreamChunk};
use crate::domain::ports::StreamingProvider;

use self::sse::SseLineBuffer;
use self::stream::StreamTransformer;
use self::types::AnthropicRequest;

/// Authentication mode for Anthropic-compatible APIs.
///
/// Direct Anthropic uses `X-Api-Key` header; gateways/proxies (e.g., Z.AI) use `Authorization: Bearer`.
#[derive(Clone)]
pub enum AuthMode {
    /// Standard Anthropic auth: sends `X-Api-Key: {key}` header.
    ApiKey(crate::domain::models::SecretString),
    /// Gateway/proxy auth: sends `Authorization: Bearer {token}` header.
    BearerToken(crate::domain::models::SecretString),
}

/// Fixed credentials are for candidate-key probes; production resolves each request.
pub enum AnthropicCredential {
    Fixed(AuthMode),
    Resolved(crate::infrastructure::key_resolver::CredentialHandle),
}

/// Anthropic API adapter. Implements `StreamingProvider` for streaming completions.
///
/// `Debug` is manually implemented to mask credentials (prevent accidental key leakage).
pub struct AnthropicAdapter {
    client: reqwest::Client,
    credential: AnthropicCredential,
    model: String,
    base_url: String,
    #[allow(dead_code)] // Used by abort() method
    abort_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AnthropicAdapter {
    /// Create a new AnthropicAdapter.
    ///
    /// Returns `ProviderError::Auth` if the credential in `auth_mode` is empty or whitespace-only.
    pub fn new(
        auth_mode: AuthMode,
        model: String,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        Self::with_credential(AnthropicCredential::Fixed(auth_mode), model, base_url)
    }

    pub fn with_credential(
        credential: AnthropicCredential,
        model: String,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        if let AnthropicCredential::Fixed(mode) = &credential {
            let secret = match mode {
                AuthMode::ApiKey(s) | AuthMode::BearerToken(s) => s,
            };
            validate_credential(secret.expose_secret())?;
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

        Ok(Self {
            client,
            credential,
            model,
            base_url: crate::infrastructure::utils::normalize_base_url(
                &base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            ),
            abort_handle: Arc::new(Mutex::new(None)),
        })
    }

    /// Resolve exactly once per request, then retain the resulting profile and bytes.
    async fn current_auth(
        &self,
    ) -> Result<(crate::domain::models::ResolvedAuth, claude_code::AuthPlan), ProviderError> {
        use crate::domain::models::{AuthHeaderScheme, AuthSource, ResolvedAuth};
        let (source, auth, header) = match &self.credential {
            AnthropicCredential::Fixed(mode) => match mode {
                AuthMode::ApiKey(s) => (
                    AuthSource::Runtime,
                    ResolvedAuth::ApiKey(s.clone()),
                    Some(AuthHeaderScheme::ApiKey),
                ),
                AuthMode::BearerToken(s) => (
                    AuthSource::Runtime,
                    ResolvedAuth::ApiKey(s.clone()),
                    Some(AuthHeaderScheme::Bearer),
                ),
            },
            AnthropicCredential::Resolved(handle) => {
                let (source, auth) = handle.current().await?;
                (source, auth, handle.auth_header)
            }
        };
        validate_credential(auth.bearer_secret().expose_secret())?;
        let plan = claude_code::anthropic_auth(&source, &auth, header);
        Ok((auth, plan))
    }

    fn request_auth(
        &self,
        request: reqwest::RequestBuilder,
        auth: &crate::domain::models::ResolvedAuth,
        plan: &claude_code::AuthPlan,
    ) -> reqwest::RequestBuilder {
        let request = request.header("anthropic-version", "2023-06-01");
        match plan.scheme {
            crate::domain::models::AuthHeaderScheme::ApiKey => {
                request.header("x-api-key", auth.bearer_secret().expose_secret())
            }
            crate::domain::models::AuthHeaderScheme::Bearer => {
                request.bearer_auth(auth.bearer_secret().expose_secret())
            }
        }
    }

    fn profile(
        &self,
        auth: &crate::domain::models::ResolvedAuth,
        plan: &claude_code::AuthPlan,
    ) -> Result<Option<claude_code::Profile>, ProviderError> {
        plan.profile
            .then(|| claude_code::Profile::new(account(auth)))
            .transpose()
    }

    fn official(&self) -> bool {
        crate::infrastructure::provider_factory::anthropic_oauth_allowed(&self.base_url)
    }
}

fn account(auth: &crate::domain::models::ResolvedAuth) -> Option<&str> {
    match auth {
        crate::domain::models::ResolvedAuth::OAuth { account_id, .. } => account_id.as_deref(),
        _ => None,
    }
}

fn validate_credential(value: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty() {
        return Err(ProviderError::AuthenticationFailed);
    }
    if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(ProviderError::Other("Credential contains control characters (newlines, tabs, etc.) — check your env var or config".into()));
    }
    Ok(())
}

impl fmt::Debug for AnthropicAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth_display = match &self.credential {
            AnthropicCredential::Fixed(AuthMode::ApiKey(_)) => "ApiKey(***)",
            AnthropicCredential::Fixed(AuthMode::BearerToken(_)) => "BearerToken(***)",
            AnthropicCredential::Resolved(_) => "Resolved(***)",
        };
        f.debug_struct("AnthropicAdapter")
            .field("auth_mode", &auth_display)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[async_trait]
impl StreamingProvider for AnthropicAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, ProviderError> {
        if messages.is_empty() {
            return Err(ProviderError::Other(
                "Cannot send empty messages list to Anthropic API".to_string(),
            ));
        }

        let (auth, plan) = self.current_auth().await?;
        let profile = self.profile(&auth, &plan)?;
        let mut request_body = AnthropicRequest::from((messages.as_slice(), &options));
        if let Some(profile) = &profile {
            profile.apply(&mut request_body, account(&auth), self.official())?;
        }
        let url = format!(
            "{}/v1/messages{}",
            self.base_url,
            if plan.profile { "?beta=true" } else { "" }
        );
        let request = self.request_auth(self.client.post(&url), &auth, &plan);
        // reqwest appends headers; the profile owns content-type when it applies.
        let request = match &profile {
            Some(profile) => profile.headers(request, !options.tools.is_empty()),
            None => request.header("content-type", "application/json"),
        };
        let mut bytes = serde_json::to_vec(&request_body)
            .map_err(|_| ProviderError::Other("Could not serialize Anthropic request".into()))?;
        if plan.profile {
            claude_code::attest(&mut bytes);
        }
        let response = request
            .body(bytes)
            .send()
            .await
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;

        tracing::debug!(
            status = %response.status(),
            "Response received from {}",
            self.base_url
        );

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            return match status.as_u16() {
                401 => Err(ProviderError::AuthenticationFailed),
                429 => {
                    if let Some(secs) = retry_after {
                        tracing::debug!("Rate limited, retry-after: {}s", secs);
                    }
                    Err(ProviderError::RateLimited {
                        retry_after_ms: retry_after.map(|s| s * 1000),
                    })
                }
                status_code if status_code >= 500 => {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown".to_string());
                    Err(ProviderError::ConnectionFailed(format!(
                        "Server error {}: {}",
                        status_code, body
                    )))
                }
                _ => {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown".to_string());
                    Err(ProviderError::Other(format!("HTTP {}: {}", status, body)))
                }
            };
        }

        // Stream the response bytes through SseLineBuffer -> StreamTransformer
        let byte_stream = response.bytes_stream();

        let stream = futures::stream::unfold(
            (
                byte_stream,
                SseLineBuffer::new(),
                StreamTransformer::with_profile(plan.profile),
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

        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        let mut handle = self.abort_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        Ok(())
    }

    fn provider_id(&self) -> String {
        "anthropic".to_string()
    }

    fn list_models(&self) -> Vec<crate::domain::models::ModelDescriptor> {
        if let Some(catalog) = crate::adapters::model_catalog_cache::load_embedded_seed() {
            if let Some(entry) = catalog.providers.get("anthropic") {
                return entry.models.iter().map(|m| m.descriptor.clone()).collect();
            }
        }
        // Fallback: empty catalog (should never happen with a valid JSON seed)
        tracing::warn!("No embedded catalog entry for 'anthropic'; returning empty");
        Vec::new()
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let (auth, plan) = self.current_auth().await?;
        let profile = self.profile(&auth, &plan)?;
        let url = format!(
            "{}/v1/messages{}",
            self.base_url,
            if plan.profile { "?beta=true" } else { "" }
        );
        let mut request = self
            .request_auth(self.client.post(&url), &auth, &plan)
            .timeout(std::time::Duration::from_secs(5));
        let bytes = if let Some(profile) = &profile {
            let options = CompletionOptions {
                model: self.model.clone(),
                max_tokens: 1,
                system_prompt: String::new(),
                temperature: None,
                tools: vec![],
            };
            let messages = [Message {
                role: crate::domain::models::MessageRole::User,
                content: "hi".into(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
            }];
            let mut body = AnthropicRequest::from((messages.as_slice(), &options));
            body.stream = false;
            profile.apply(&mut body, account(&auth), self.official())?;
            request = profile.headers(request, false);
            let mut bytes = serde_json::to_vec(&body).map_err(|_| {
                ProviderError::Other("Could not serialize Anthropic request".into())
            })?;
            claude_code::attest(&mut bytes);
            bytes
        } else {
            request = request.header("content-type", "application/json");
            serde_json::to_vec(&serde_json::json!({
                "model": self.model, "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .map_err(|_| ProviderError::Other("Could not serialize Anthropic request".into()))?
        };
        let response = request.body(bytes).send().await;
        match response {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) if resp.status().as_u16() == 401 => Err(ProviderError::AuthenticationFailed),
            Ok(resp) => Err(ProviderError::Other(format!(
                "Health check failed: HTTP {}",
                resp.status()
            ))),
            Err(e) => Err(crate::adapters::provider::classify_reqwest_error(&e)),
        }
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
        use std::time::Instant;
        let start = Instant::now();
        // Non-billable: GET /v1/models (free, idempotent, validates auth).
        // base_url may already end with "/v1" (e.g. proxy); avoid doubling it.
        let url = if self.base_url.ends_with("/v1") {
            format!("{}/models", self.base_url)
        } else {
            format!("{}/v1/models", self.base_url)
        };
        let (auth, plan) = self.current_auth().await?;
        let mut req = self
            .request_auth(self.client.get(&url), &auth, &plan)
            .timeout(std::time::Duration::from_secs(5));
        if let Some(profile) = self.profile(&auth, &plan)? {
            req = profile.headers(req, false);
        }
        let response = req.send().await;
        let latency = start.elapsed();
        match response {
            Ok(resp) => {
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
            Err(e) => Err(crate::adapters::provider::classify_reqwest_error(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_adapter_debug_masks_api_key() {
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey(crate::domain::models::SecretString::new(
                "sk-ant-test-key".to_string(),
            )),
            "claude-sonnet-4-6".to_string(),
            None,
        )
        .unwrap();

        let debug_output = format!("{:?}", adapter);
        assert!(!debug_output.contains("sk-ant-test-key"));
        assert!(debug_output.contains("***"));
        assert!(debug_output.contains("ApiKey"));
        assert!(debug_output.contains("claude-sonnet-4-6"));
    }

    #[test]
    fn test_anthropic_adapter_debug_masks_bearer_token() {
        let adapter = AnthropicAdapter::new(
            AuthMode::BearerToken(crate::domain::models::SecretString::new(
                "my-secret-token".to_string(),
            )),
            "glm-4.7".to_string(),
            None,
        )
        .unwrap();

        let debug_output = format!("{:?}", adapter);
        assert!(!debug_output.contains("my-secret-token"));
        assert!(debug_output.contains("***"));
        assert!(debug_output.contains("BearerToken"));
    }

    #[test]
    fn test_anthropic_adapter_missing_api_key() {
        let result = AnthropicAdapter::new(
            AuthMode::ApiKey(crate::domain::models::SecretString::new(String::new())),
            "claude-sonnet-4-6".to_string(),
            None,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::AuthenticationFailed => {}
            other => panic!("Expected AuthenticationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_anthropic_adapter_missing_bearer_token() {
        let result = AnthropicAdapter::new(
            AuthMode::BearerToken(crate::domain::models::SecretString::new(String::new())),
            "claude-sonnet-4-6".to_string(),
            None,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::AuthenticationFailed => {}
            other => panic!("Expected AuthenticationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_anthropic_adapter_provider_id() {
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey(crate::domain::models::SecretString::new(
                "test-key".to_string(),
            )),
            "claude-sonnet-4-6".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(adapter.provider_id(), "anthropic");
    }

    #[test]
    fn test_anthropic_adapter_custom_base_url() {
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey(crate::domain::models::SecretString::new(
                "test-key".to_string(),
            )),
            "claude-sonnet-4-6".to_string(),
            Some("http://localhost:8080".to_string()),
        )
        .unwrap();
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("localhost:8080"));
    }

    #[test]
    fn test_api_key_never_in_serialized_output() {
        let key = "sk-ant-secret-key-12345";
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey(crate::domain::models::SecretString::new(key.to_string())),
            "claude-sonnet-4-6".to_string(),
            None,
        )
        .unwrap();

        let debug = format!("{:?}", adapter);
        assert!(!debug.contains(key), "API key leaked in Debug output");

        let debug2 = format!("{:?}", &adapter as &dyn std::fmt::Debug);
        assert!(!debug2.contains(key), "API key leaked in dyn Debug output");
    }

    #[test]
    fn test_bearer_token_never_in_serialized_output() {
        let token = "super-secret-bearer-token-xyz";
        let adapter = AnthropicAdapter::new(
            AuthMode::BearerToken(crate::domain::models::SecretString::new(token.to_string())),
            "glm-4.7".to_string(),
            None,
        )
        .unwrap();

        let debug = format!("{:?}", adapter);
        assert!(
            !debug.contains(token),
            "Bearer token leaked in Debug output"
        );
    }
}
