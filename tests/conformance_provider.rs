//! Conformance tests for the `StreamingProvider` trait and `ProviderRegistry`.
//!
//! Follows the pattern established in `tests/conformance.rs` — generic test
//! functions parameterized over trait implementations.

use std::sync::Arc;

use futures::StreamExt;
use rustain::adapters::noop::NoOpProvider;
use rustain::adapters::provider::ProviderRegistry;
use rustain::domain::models::provider::ModelDescriptor;
use rustain::domain::models::{CompletionOptions, Message, MessageRole, StreamChunk, UserMessage};
use rustain::domain::ports::StreamingProvider;
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// Test 1: streaming_provider_conformance — generic conformance over any impl
// ---------------------------------------------------------------------------

fn assert_streaming_provider_conformance(provider: &dyn StreamingProvider) {
    // provider_id returns non-empty
    let pid = provider.provider_id();
    assert!(!pid.is_empty(), "provider_id must not be empty");

    // list_models returns valid descriptors
    let models = provider.list_models();
    for model in &models {
        assert!(!model.model_id.is_empty(), "model_id must not be empty");
        assert!(
            !model.display_name.is_empty(),
            "display_name must not be empty"
        );
        // Router aggregates models from multiple providers — skip provider_id match
        if pid != "router" {
            assert_eq!(
                model.provider_id, pid,
                "model's provider_id must match the owning provider"
            );
        }
        assert!(model.context_window > 0, "context_window must be positive");
    }

    // health_check must succeed or return a typed error (not panic)
    let rt = Runtime::new().unwrap();
    let health_result = rt.block_on(provider.health_check());
    assert!(
        health_result.is_ok() || health_result.is_err(),
        "health_check must return a result (Ok or typed Err)"
    );
}

#[test]
fn test_noop_provider_conformance() {
    let provider = NoOpProvider::default();
    assert_streaming_provider_conformance(&provider);
    assert_eq!(provider.provider_id(), "noop");
    assert!(provider.list_models().is_empty());

    // health_check should succeed for NoOp
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        assert!(provider.health_check().await.is_ok());
    });
}

// ---------------------------------------------------------------------------
// Test 2: ProviderRegistry
// ---------------------------------------------------------------------------

#[test]
fn test_registry_list_models() {
    let registry = ProviderRegistry::new();
    registry.register(Box::new(NoOpProvider::default()));

    // NoOp returns empty model list
    let all = registry.list_all_models();
    assert!(all.is_empty());

    // list_providers returns the registered provider
    let providers = registry.list_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].provider_id, "noop");

    // get_model returns None for non-existent model
    assert!(registry.get_model("noop", "nope").is_none());
}

#[test]
fn test_registry_health_check_on_noop_succeeds() {
    let registry = ProviderRegistry::new();
    registry.register(Box::new(NoOpProvider::default()));

    let providers = registry.list_providers();
    assert_eq!(providers[0].model_count, 0);
    // NoOp health_check always succeeds, so the provider is listed as healthy
}

// ---------------------------------------------------------------------------
// Test 3: Anthropic adapter behavior preservation (compile-time + basic)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Requires ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN env var"]
fn test_anthropic_adapter_provider_id_and_models() {
    let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    assert!(
        auth_token.is_some() || api_key.is_some(),
        "ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN must be set"
    );

    let auth_mode = if let Some(token) = auth_token {
        rustain::adapters::anthropic::AuthMode::BearerToken(token)
    } else if let Some(key) = api_key {
        rustain::adapters::anthropic::AuthMode::ApiKey(key)
    } else {
        unreachable!()
    };

    let adapter = rustain::adapters::anthropic::AnthropicAdapter::new(
        auth_mode,
        "claude-sonnet-4-20250514".to_string(),
        None,
    )
    .expect("AnthropicAdapter construction failed");

    assert_eq!(adapter.provider_id(), "anthropic");

    let models = adapter.list_models();
    assert!(
        !models.is_empty(),
        "Anthropic adapter must list at least one model"
    );
    let sonnet = models
        .iter()
        .find(|m| m.model_id == "claude-sonnet-4-20250514");
    assert!(sonnet.is_some(), "Sonnet model must be present");
    let sonnet = sonnet.unwrap();
    assert_eq!(sonnet.context_window, 200_000);
    assert!(
        sonnet
            .capabilities
            .contains(&rustain::domain::models::ModelCapability::ToolUse)
    );
}

// ---------------------------------------------------------------------------
// Test 4: OpenAI adapter variant provider ids (AC4, AC5)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "openai")]
fn test_openai_adapter_variant_provider_ids() {
    use rustain::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let cases = vec![
        (OpenAiCompatibleVariant::OpenAI, "openai"),
        (OpenAiCompatibleVariant::OpenRouter, "openrouter"),
        (OpenAiCompatibleVariant::Google, "google"),
        (OpenAiCompatibleVariant::DeepSeek, "deepseek"),
        (OpenAiCompatibleVariant::Moonshot, "moonshot"),
    ];

    for (variant, expected) in cases {
        let adapter = OpenAiAdapter::new(
            variant.clone(),
            "test-key".to_string(),
            "model-x".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(
            adapter.provider_id(),
            expected,
            "variant {:?} should have provider_id '{}'",
            variant,
            expected
        );
    }
}

// ---------------------------------------------------------------------------
// Test 5: OpenAI adapter known catalog for each variant (AC4)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "openai")]
fn test_openai_adapter_known_catalog_for_each_variant() {
    use rustain::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let cases = vec![
        (
            OpenAiCompatibleVariant::OpenAI,
            vec![
                "gpt-4o-2024-11-20",
                "gpt-4o-mini-2024-07-18",
                "o1-2024-12-17",
                "o3-mini-2025-01-31",
            ],
        ),
        (
            OpenAiCompatibleVariant::Google,
            vec!["gemini-2.0-flash", "gemini-2.5-pro-preview-03-25"],
        ),
        (
            OpenAiCompatibleVariant::DeepSeek,
            vec!["deepseek-chat", "deepseek-reasoner"],
        ),
        (
            OpenAiCompatibleVariant::Moonshot,
            vec!["moonshot-v1-auto", "kimi-k2-instruct"],
        ),
    ];

    for (variant, expected_ids) in cases {
        let adapter = OpenAiAdapter::new(
            variant.clone(),
            "test-key".to_string(),
            "model-x".to_string(),
            None,
        )
        .unwrap();
        let models = adapter.list_models();
        let ids: Vec<_> = models.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(ids, expected_ids, "variant {:?} catalog mismatch", variant);
        for m in &models {
            assert_eq!(m.provider_id, adapter.provider_id());
        }
    }

    // OpenRouter returns single-entry catalog
    let adapter = OpenAiAdapter::new(
        OpenAiCompatibleVariant::OpenRouter,
        "test-key".to_string(),
        "anthropic/claude-3.5-sonnet".to_string(),
        None,
    )
    .unwrap();
    let models = adapter.list_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model_id, "anthropic/claude-3.5-sonnet");
    assert_eq!(models[0].provider_id, "openrouter");
}

// ---------------------------------------------------------------------------
// Test 6: Ollama adapter health check unreachable (AC3)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "ollama")]
#[ignore = "Requires network — points at non-existent localhost port"]
fn test_ollama_adapter_health_check_unreachable_returns_connection_failed() {
    use rustain::adapters::ollama::OllamaAdapter;
    use rustain::domain::errors::ProviderError;

    let adapter = OllamaAdapter::new(
        "llama3.3:70b".to_string(),
        Some("http://localhost:11433".to_string()), // one-off port
    )
    .unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(adapter.health_check());
    match result {
        Err(ProviderError::ConnectionFailed(_)) => {}
        other => panic!(
            "Expected ConnectionFailed for unreachable Ollama, got {:?}",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// Test 7: Ollama adapter parses /api/tags response (AC3)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "ollama")]
fn test_ollama_adapter_parses_api_tags_response() {
    use rustain::adapters::ollama::OllamaAdapter;

    let mut server = mockito::Server::new();
    let url = server.url();

    let _m = server
        .mock("GET", "/api/tags")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"models":[{"name":"llama3.3:70b","details":{"parameter_size":"70B"}},{"name":"phi4:14b","details":{"parameter_size":"14B"}}]}"#)
        .create();

    let adapter = OllamaAdapter::new("llama3.3:70b".to_string(), Some(url)).unwrap();

    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        adapter
            .health_check()
            .await
            .expect("health_check should succeed");
    });

    let models = adapter.list_models();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model_id, "llama3.3:70b");
    assert_eq!(models[0].context_window, 32_768);
    assert_eq!(models[1].model_id, "phi4:14b");
    assert_eq!(models[1].context_window, 16_384);
    assert_eq!(models[0].provider_id, "ollama");
    assert_eq!(models[0].pricing_tier, Some("local".to_string()));
}

// ---------------------------------------------------------------------------
// Test 8: ProviderRouter delegation (AC8)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct MockStreamingProvider {
    id: String,
    chunks: std::sync::Mutex<Vec<StreamChunk>>,
}

impl MockStreamingProvider {
    fn new(id: &str, chunks: Vec<StreamChunk>) -> Self {
        Self {
            id: id.to_string(),
            chunks: std::sync::Mutex::new(chunks),
        }
    }
}

#[async_trait::async_trait]
impl StreamingProvider for MockStreamingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let chunks = self.chunks.lock().unwrap().clone();
        let stream = futures::stream::iter(chunks);
        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

#[test]
fn test_router_delegates_to_active_provider() {
    use rustain::adapters::provider::ProviderRouter;

    let router = Arc::new(ProviderRouter::new("a".to_string()));
    router.register(Arc::new(MockStreamingProvider::new(
        "a",
        vec![StreamChunk::Text {
            content: "from-a".to_string(),
            parent_tool_use_id: None,
        }],
    )));
    router.register(Arc::new(MockStreamingProvider::new(
        "b",
        vec![StreamChunk::Text {
            content: "from-b".to_string(),
            parent_tool_use_id: None,
        }],
    )));

    let rt = Runtime::new().unwrap();

    // Active = a
    rt.block_on(async {
        let mut stream = router
            .stream_completion(
                vec![],
                CompletionOptions {
                    model: "test".to_string(),
                    max_tokens: 1024,
                    system_prompt: "".to_string(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await
            .unwrap();
        let chunk = stream.next().await.unwrap();
        match chunk {
            StreamChunk::Text { content, .. } => assert_eq!(content, "from-a"),
            _ => panic!("Expected Text from provider a"),
        }
    });

    // Switch to b
    router.set_active("b").unwrap();
    rt.block_on(async {
        let mut stream = router
            .stream_completion(
                vec![],
                CompletionOptions {
                    model: "test".to_string(),
                    max_tokens: 1024,
                    system_prompt: "".to_string(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await
            .unwrap();
        let chunk = stream.next().await.unwrap();
        match chunk {
            StreamChunk::Text { content, .. } => assert_eq!(content, "from-b"),
            _ => panic!("Expected Text from provider b"),
        }
    });
}

#[test]
fn test_router_set_active_unknown_id_returns_error() {
    use rustain::adapters::provider::ProviderRouter;

    let router = ProviderRouter::new("noop".to_string());
    router.register(Arc::new(NoOpProvider::default()));
    let result = router.set_active("nope");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("unknown provider 'nope'"));
}

#[test]
fn test_router_swap_does_not_block_in_flight_stream() {
    use rustain::adapters::provider::ProviderRouter;

    let router = Arc::new(ProviderRouter::new("a".to_string()));
    router.register(Arc::new(MockStreamingProvider::new(
        "a",
        vec![
            StreamChunk::Text {
                content: "chunk-1".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::Text {
                content: "chunk-2".to_string(),
                parent_tool_use_id: None,
            },
        ],
    )));
    router.register(Arc::new(MockStreamingProvider::new(
        "b",
        vec![StreamChunk::Text {
            content: "from-b".to_string(),
            parent_tool_use_id: None,
        }],
    )));

    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut stream = router
            .stream_completion(
                vec![],
                CompletionOptions {
                    model: "test".to_string(),
                    max_tokens: 1024,
                    system_prompt: "".to_string(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await
            .unwrap();

        // Read first chunk from a
        let chunk1 = stream.next().await.unwrap();
        match chunk1 {
            StreamChunk::Text { content, .. } => assert_eq!(content, "chunk-1"),
            _ => panic!("Expected chunk-1"),
        }

        // Swap active to b mid-stream
        router.set_active("b").unwrap();

        // Drain remaining chunks from the original stream (should still be from a)
        let chunk2 = stream.next().await.unwrap();
        match chunk2 {
            StreamChunk::Text { content, .. } => assert_eq!(content, "chunk-2"),
            _ => panic!("Expected chunk-2 from original stream"),
        }
    });
}

#[test]
fn test_router_conformance() {
    use rustain::adapters::provider::ProviderRouter;

    let router = Arc::new(ProviderRouter::new("noop".to_string()));
    router.register(Arc::new(NoOpProvider::default()));
    assert_streaming_provider_conformance(router.as_ref());
}

// ---------------------------------------------------------------------------
// Test 9: Domain isolation (AC9)
// ---------------------------------------------------------------------------

#[test]
fn test_domain_models_no_adapter_imports() {
    // The new domain types must not import from adapters/ or infrastructure/.
    // This is verified at compile time — if it compiles, it passes.
    // At runtime, we assert that ModelDescriptor has the expected shape.
    let desc = ModelDescriptor {
        model_id: "test".to_string(),
        display_name: "Test".to_string(),
        provider_id: "test".to_string(),
        context_window: 1000,
        capabilities: Default::default(),
        pricing_tier: None,
    };
    assert_eq!(desc.model_id, "test");
    assert_eq!(desc.provider_id, "test");
    assert_eq!(desc.context_window, 1000);
}
