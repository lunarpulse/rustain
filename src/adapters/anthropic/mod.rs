//! Anthropic API adapter implementing the `ProviderPort` trait.
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
use crate::domain::ports::ProviderPort;

use self::sse::SseLineBuffer;
use self::stream::StreamTransformer;
use self::types::AnthropicRequest;

/// Anthropic API adapter. Implements `ProviderPort` for streaming completions.
///
/// `Debug` is manually implemented to mask `api_key` (AC5 — prevent accidental key leakage).
pub struct AnthropicAdapter {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    #[allow(dead_code)] // Used by abort() method
    abort_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AnthropicAdapter {
    /// Create a new AnthropicAdapter.
    ///
    /// Returns `ProviderError::Auth` if `api_key` is empty.
    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        if api_key.is_empty() {
            return Err(ProviderError::AuthenticationFailed);
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            abort_handle: Arc::new(Mutex::new(None)),
        })
    }
}

impl fmt::Debug for AnthropicAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicAdapter")
            .field("api_key", &"***")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[async_trait]
impl ProviderPort for AnthropicAdapter {
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

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

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

    fn provider_id(&self) -> &str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_adapter_debug_masks_api_key() {
        let adapter = AnthropicAdapter::new(
            "sk-ant-test-key".to_string(),
            "claude-sonnet-4-6".to_string(),
            None,
        )
        .unwrap();

        let debug_output = format!("{:?}", adapter);
        assert!(!debug_output.contains("sk-ant-test-key"));
        assert!(debug_output.contains("***"));
        assert!(debug_output.contains("claude-sonnet-4-6"));
    }

    #[test]
    fn test_anthropic_adapter_missing_api_key() {
        let result = AnthropicAdapter::new(String::new(), "claude-sonnet-4-6".to_string(), None);
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::AuthenticationFailed => {}
            other => panic!("Expected AuthenticationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_anthropic_adapter_provider_id() {
        let adapter = AnthropicAdapter::new(
            "test-key".to_string(),
            "claude-sonnet-4-6".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(adapter.provider_id(), "anthropic");
    }

    #[test]
    fn test_anthropic_adapter_custom_base_url() {
        let adapter = AnthropicAdapter::new(
            "test-key".to_string(),
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
        let adapter =
            AnthropicAdapter::new(key.to_string(), "claude-sonnet-4-6".to_string(), None).unwrap();

        // Debug output must not contain the key
        let debug = format!("{:?}", adapter);
        assert!(!debug.contains(key), "API key leaked in Debug output");

        // Display/toString must not leak key either (Debug is the only impl)
        let debug2 = format!("{:?}", &adapter as &dyn std::fmt::Debug);
        assert!(!debug2.contains(key), "API key leaked in dyn Debug output");
    }
}
