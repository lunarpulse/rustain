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

pub mod allowlists;
pub mod discovery;
pub mod stream;
pub mod types;
pub mod variant;

pub use variant::OpenAiCompatibleVariant;

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
    variant: OpenAiCompatibleVariant,
    #[allow(dead_code)] // Used by abort() method
    abort_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    discovered_models: Arc<arc_swap::ArcSwap<Option<Vec<crate::adapters::model_catalog_cache::CachedModelEntry>>>>,
}

impl OpenAiAdapter {
    /// Create a new OpenAiAdapter.
    ///
    /// # Arguments
    /// * `api_key` — Bearer token for `Authorization` header
    /// * `model` — Model identifier (e.g., `moonshot-v1-auto`, `gpt-4o`)
    /// * `base_url` — API base URL including `/v1` path (e.g., `https://api.moonshot.cn/v1`)
    pub fn new(
        variant: OpenAiCompatibleVariant,
        api_key: String,
        model: String,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        if !matches!(variant, OpenAiCompatibleVariant::Custom { .. }) && api_key.trim().is_empty() {
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
            &base_url.unwrap_or_else(|| variant.default_base_url().to_string()),
        );

        Ok(Self {
            client,
            api_key,
            model,
            base_url: resolved_base_url,
            variant,
            abort_handle: Arc::new(Mutex::new(None)),
            discovered_models: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
        })
    }

    /// Fetch model catalog from the provider's `/v1/models` endpoint.
    pub async fn fetch_remote_models(
        &self,
        model_filter: &[String],
    ) -> Result<Vec<crate::domain::models::ModelDescriptor>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let mut req = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5));
        if !self.api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {}", self.api_key));
        }
        let response = req.send().await;
        match response {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.map_err(|e| {
                    ProviderError::Other(format!("Failed to read models response: {}", e))
                })?;
                crate::adapters::openai::discovery::parse_and_filter_models(
                    &text,
                    &self.variant,
                    model_filter,
                )
            }
            Ok(resp) if resp.status().as_u16() == 401 => Err(ProviderError::AuthenticationFailed),
            Ok(resp) => Err(ProviderError::Other(format!(
                "Models fetch failed: HTTP {}",
                resp.status()
            ))),
            Err(e) => Err(ProviderError::ConnectionFailed(e.to_string())),
        }
    }

    /// Overlay discovered models into `list_models()`.
    pub fn set_discovered_models(&self, models: Vec<crate::adapters::model_catalog_cache::CachedModelEntry>) {
        self.discovered_models.store(Arc::new(Some(models)));
    }

    /// Clear the discovered models overlay (falls back to bundled snapshot).
    pub fn clear_discovered_models(&self) {
        self.discovered_models.store(Arc::new(None));
    }
}

impl fmt::Debug for OpenAiAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiAdapter")
            .field("variant", &self.variant)
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

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request_body);
        if !self.api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {}", self.api_key));
        }
        let response = req
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

    fn provider_id(&self) -> String {
        self.variant.provider_id().to_string()
    }

    fn list_models(&self) -> Vec<crate::domain::models::ModelDescriptor> {
        let guard = self.discovered_models.load();
        if let Some(ref entries) = **guard {
            if !entries.is_empty() {
                return entries.iter().map(|e| {
                    let mut desc = e.descriptor.clone();
                    desc.stale = e.descriptor.stale;
                    desc
                }).collect();
            }
        }
        self.variant.known_models(&self.model)
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let url = format!("{}/models", self.base_url);
        let mut req = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5));
        if !self.api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {}", self.api_key));
        }
        let response = req.send().await;
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
    fn test_openai_adapter_debug_masks_api_key() {
        let adapter = OpenAiAdapter::new(
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
        let result = OpenAiAdapter::new(
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
        let result = OpenAiAdapter::new(
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
        let adapter = OpenAiAdapter::new(
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
        let adapter = OpenAiAdapter::new(
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
        let adapter = OpenAiAdapter::new(
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
        let adapter = OpenAiAdapter::new(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            None,
        )
        .unwrap();

        let m1 = crate::adapters::model_catalog_cache::CachedModelEntry {
            descriptor: crate::domain::models::ModelDescriptor {
                model_id: "m1".to_string(),
                display_name: "M1".to_string(),
                provider_id: "openai".to_string(),
                context_window: 128_000,
                capabilities: std::collections::HashSet::new(),
                pricing_tier: None,
                stale: false,
            },
        };
        let m2 = crate::adapters::model_catalog_cache::CachedModelEntry {
            descriptor: crate::domain::models::ModelDescriptor {
                model_id: "m2".to_string(),
                display_name: "M2".to_string(),
                provider_id: "openai".to_string(),
                context_window: 128_000,
                capabilities: std::collections::HashSet::new(),
                pricing_tier: None,
                stale: false,
            },
        };
        adapter.set_discovered_models(vec![m1.clone(), m2.clone()]);
        let models = adapter.list_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id, "m1");
        assert_eq!(models[1].model_id, "m2");
    }

    #[test]
    fn test_clear_discovered_models_fallback() {
        let adapter = OpenAiAdapter::new(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            None,
        )
        .unwrap();

        let m1 = crate::adapters::model_catalog_cache::CachedModelEntry {
            descriptor: crate::domain::models::ModelDescriptor {
                model_id: "m1".to_string(),
                display_name: "M1".to_string(),
                provider_id: "openai".to_string(),
                context_window: 128_000,
                capabilities: std::collections::HashSet::new(),
                pricing_tier: None,
                stale: false,
            },
        };
        adapter.set_discovered_models(vec![m1]);
        adapter.clear_discovered_models();
        let models = adapter.list_models();
        // Falls back to bundled snapshot (20+ OpenAI models)
        assert!(models.len() == 12, "expected 12 fallback models, got {}", models.len());
    }

    #[test]
    fn test_empty_discovered_models_fallback() {
        let adapter = OpenAiAdapter::new(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            None,
        )
        .unwrap();

        adapter.set_discovered_models(vec![]);
        let models = adapter.list_models();
        // Empty discovered list falls back to bundled snapshot
        assert!(models.len() == 12, "expected 12 fallback models, got {}", models.len());
    }
}
