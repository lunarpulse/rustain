//! Anthropic API adapter implementing the `StreamingProvider` trait.
//! All Anthropic-specific types and SSE parsing live in this module.

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
    ApiKey(String),
    /// Gateway/proxy auth: sends `Authorization: Bearer {token}` header.
    BearerToken(String),
}

/// Anthropic API adapter. Implements `StreamingProvider` for streaming completions.
///
/// `Debug` is manually implemented to mask credentials (prevent accidental key leakage).
pub struct AnthropicAdapter {
    client: reqwest::Client,
    auth_mode: AuthMode,
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
        let credential = match &auth_mode {
            AuthMode::ApiKey(key) => key,
            AuthMode::BearerToken(token) => token,
        };
        if credential.trim().is_empty() {
            return Err(ProviderError::AuthenticationFailed);
        }
        if credential.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::Other(
                "Credential contains control characters (newlines, tabs, etc.) — check your env var or config".to_string(),
            ));
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

        Ok(Self {
            client,
            auth_mode,
            model,
            base_url: crate::infrastructure::utils::normalize_base_url(
                &base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            ),
            abort_handle: Arc::new(Mutex::new(None)),
        })
    }
}

impl fmt::Debug for AnthropicAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth_display = match &self.auth_mode {
            AuthMode::ApiKey(_) => "ApiKey(***)",
            AuthMode::BearerToken(_) => "BearerToken(***)",
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

        let request_body = AnthropicRequest::from((messages.as_slice(), &options));

        let url = format!("{}/v1/messages", self.base_url);

        let auth_header_name = match &self.auth_mode {
            AuthMode::ApiKey(_) => "x-api-key",
            AuthMode::BearerToken(_) => "authorization",
        };
        tracing::debug!(
            target_url = %url,
            auth_type = auth_header_name,
            model = %options.model,
            message_count = messages.len(),
            "Sending completion request"
        );
        tracing::trace!(
            request_body = %serde_json::to_string(&request_body).unwrap_or_default(),
            "Full request body"
        );

        let request = match &self.auth_mode {
            AuthMode::ApiKey(key) => self.client.post(&url).header("x-api-key", key),
            AuthMode::BearerToken(token) => self
                .client
                .post(&url)
                .header("authorization", format!("Bearer {}", token)),
        };

        let response = request
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

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
                StreamTransformer::new(),
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
        use crate::domain::models::ModelCapability;
        vec![
            crate::domain::models::ModelDescriptor {
                model_id: "claude-sonnet-4-6".to_string(),
                display_name: "Claude Sonnet 4".to_string(),
                provider_id: "anthropic".to_string(),
                context_window: 200_000,
                capabilities: std::collections::HashSet::from([
                    ModelCapability::Vision,
                    ModelCapability::ToolUse,
                    ModelCapability::Thinking,
                    ModelCapability::ParallelToolCalls,
                ]),
                pricing_tier: Some("flagship".to_string()),
            },
            crate::domain::models::ModelDescriptor {
                model_id: "claude-opus-4-6".to_string(),
                display_name: "Claude Opus 4".to_string(),
                provider_id: "anthropic".to_string(),
                context_window: 200_000,
                capabilities: std::collections::HashSet::from([
                    ModelCapability::Vision,
                    ModelCapability::ToolUse,
                    ModelCapability::Thinking,
                    ModelCapability::ParallelToolCalls,
                ]),
                pricing_tier: Some("flagship".to_string()),
            },
            crate::domain::models::ModelDescriptor {
                model_id: "claude-haiku-4-5".to_string(),
                display_name: "Claude Haiku 4.5".to_string(),
                provider_id: "anthropic".to_string(),
                context_window: 200_000,
                capabilities: std::collections::HashSet::from([
                    ModelCapability::Vision,
                    ModelCapability::ToolUse,
                ]),
                pricing_tier: Some("cheap".to_string()),
            },
        ]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let request = match &self.auth_mode {
            AuthMode::ApiKey(key) => self
                .client
                .post(&url)
                .header("x-api-key", key.to_string())
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .timeout(std::time::Duration::from_secs(5))
                .body(serde_json::to_string(&body).unwrap_or_default()),
            AuthMode::BearerToken(token) => self
                .client
                .post(&url)
                .header("authorization", format!("Bearer {}", token))
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .timeout(std::time::Duration::from_secs(5))
                .body(serde_json::to_string(&body).unwrap_or_default()),
        };
        let response = request.send().await;
        match response {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) if resp.status().as_u16() == 401 => Err(ProviderError::AuthenticationFailed),
            Ok(resp) => Err(ProviderError::Other(format!(
                "Health check failed: HTTP {}",
                resp.status()
            ))),
            Err(e) => Err(ProviderError::ConnectionFailed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_adapter_debug_masks_api_key() {
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("sk-ant-test-key".to_string()),
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
            AuthMode::BearerToken("my-secret-token".to_string()),
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
            AuthMode::ApiKey(String::new()),
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
            AuthMode::BearerToken(String::new()),
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
            AuthMode::ApiKey("test-key".to_string()),
            "claude-sonnet-4-6".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(adapter.provider_id(), "anthropic");
    }

    #[test]
    fn test_anthropic_adapter_custom_base_url() {
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".to_string()),
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
            AuthMode::ApiKey(key.to_string()),
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
            AuthMode::BearerToken(token.to_string()),
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
