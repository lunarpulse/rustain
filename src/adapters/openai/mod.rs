//! OpenAI-compatible API adapter implementing the `StreamingProvider` trait.
//!
//! Works with any provider that implements the OpenAI Chat Completions API:
//! - **Kimi** (Moonshot AI) — `https://api.moonshot.cn/v1`
//! - **OpenAI** — `https://api.openai.com/v1`
//! - **DeepSeek** — `https://api.deepseek.com/v1`
//! - Any other OpenAI-compatible endpoint
//!
//! Wire format: OpenAI Chat Completions with `stream: true`.
//! Auth: `Authorization: Bearer {api_key}`.

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

use self::stream::OpenAiStreamTransformer;
use self::types::OpenAiRequest;
use crate::adapters::anthropic::sse::SseLineBuffer;

/// OpenAI-compatible API adapter. Implements `StreamingProvider` for streaming completions.
///
/// `Debug` is manually implemented to mask credentials (prevent accidental key leakage).
pub struct OpenAiAdapter {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    #[allow(dead_code)] // Used by abort() method
    abort_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl OpenAiAdapter {
    /// Create a new OpenAiAdapter.
    ///
    /// # Arguments
    /// * `api_key` — Bearer token for `Authorization` header
    /// * `model` — Model identifier (e.g., `moonshot-v1-auto`, `gpt-4o`)
    /// * `base_url` — API base URL including `/v1` path (e.g., `https://api.moonshot.cn/v1`)
    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        if api_key.trim().is_empty() {
            return Err(ProviderError::AuthenticationFailed);
        }
        if api_key.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::Other(
                "Credential contains control characters (newlines, tabs, etc.) — check your env var or config".to_string(),
            ));
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

        let resolved_base_url = crate::infrastructure::utils::normalize_base_url(
            &base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        );

        Ok(Self {
            client,
            api_key,
            model,
            base_url: resolved_base_url,
            abort_handle: Arc::new(Mutex::new(None)),
        })
    }
}

impl fmt::Debug for OpenAiAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiAdapter")
            .field("api_key", &"(***)")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[async_trait]
impl StreamingProvider for OpenAiAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, ProviderError> {
        if messages.is_empty() {
            return Err(ProviderError::Other(
                "Cannot send empty messages list to API".to_string(),
            ));
        }

        let request_body = OpenAiRequest::from((messages.as_slice(), &options));
        let url = format!("{}/chat/completions", self.base_url);

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

        let response = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {}", self.api_key))
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

        // Stream the response bytes through SseLineBuffer -> OpenAiStreamTransformer
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

        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        let mut handle = self.abort_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        Ok(())
    }

    fn provider_id(&self) -> &str {
        "openai"
    }

    fn list_models(&self) -> Vec<crate::domain::models::ModelDescriptor> {
        use crate::domain::models::ModelCapability;
        vec![crate::domain::models::ModelDescriptor {
            model_id: self.model.clone(),
            display_name: self.model.clone(),
            provider_id: "openai".to_string(),
            context_window: 128_000,
            capabilities: std::collections::HashSet::from([
                ModelCapability::ToolUse,
            ]),
            pricing_tier: None,
        }]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = self
            .client
            .get(&url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        match response {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) if resp.status().as_u16() == 401 => {
                Err(ProviderError::AuthenticationFailed)
            }
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
    fn test_openai_adapter_debug_masks_api_key() {
        let adapter = OpenAiAdapter::new(
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
        let result = OpenAiAdapter::new(
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
    fn test_openai_adapter_provider_id() {
        let adapter = OpenAiAdapter::new(
            "test-key".to_string(),
            "moonshot-v1-auto".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(adapter.provider_id(), "openai");
    }

    #[test]
    fn test_openai_adapter_custom_base_url() {
        let adapter = OpenAiAdapter::new(
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
        let adapter = OpenAiAdapter::new(
            key.to_string(),
            "moonshot-v1-auto".to_string(),
            None,
        )
        .unwrap();

        let debug = format!("{:?}", adapter);
        assert!(!debug.contains(key), "API key leaked in Debug output");
    }
}
