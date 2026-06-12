//! Integration tests for the Anthropic streaming pipeline.
//! Tests: SseLineBuffer -> StreamTransformer -> apply_chunk -> ChunkAction flow.

#[cfg(feature = "anthropic")]
mod streaming_integration {
    use std::collections::VecDeque;

    use rustain::adapters::anthropic::AuthMode;
    use rustain::domain::events::ChunkAction;
    use rustain::domain::models::{
        ChatMessage, CompletionOptions, Conversation, Message, MessageRole, StopReason,
        StreamChunk, StreamingPhase, StreamingState, UsageInfo, generate_conversation_id,
    };
    use rustain::domain::services::reducer::{apply_chunk_for_tests, test_reducer_state};

    /// Helper: create a fresh conversation.
    fn make_conversation() -> Conversation {
        Conversation {
            id: generate_conversation_id(),
            title: String::new(),
            messages: Vec::new(),
            turns: Vec::new(),
            created_at: 1000,
            updated_at: 1000,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        }
    }

    /// Helper: create streaming state.
    fn make_streaming() -> StreamingState {
        let (_reducer, _clock) = test_reducer_state(1000);
        StreamingState {
            is_streaming: true,
            ..StreamingState::default()
        }
    }

    // ─── Task 9.1: SseLineBuffer tests (beyond inline tests) ─────────────

    // Covers: FR1 (streaming)
    #[test]
    fn test_sse_buffer_rapid_succession_events() {
        use rustain::adapters::anthropic::sse::SseLineBuffer;

        let mut buf = SseLineBuffer::new();
        // Multiple events in a single feed
        let input = b"event: ping\ndata: {}\n\nevent: ping\ndata: {}\n\nevent: ping\ndata: {}\n\n";
        let frames = buf.feed(input);
        assert_eq!(frames.len(), 3);
    }

    // Covers: FR1 (streaming)
    #[test]
    fn test_sse_buffer_interleaved_partial_feeds() {
        use rustain::adapters::anthropic::sse::SseLineBuffer;

        let mut buf = SseLineBuffer::new();
        // Feed byte-by-byte
        let input = "event: test\ndata: hello\n\n";
        let mut frames = Vec::new();
        for byte in input.bytes() {
            frames.extend(buf.feed(&[byte]));
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "test");
        assert_eq!(frames[0].data, "hello");
    }

    // ─── Task 9.2: SSE-to-StreamChunk mapping ───────────────────────────

    // Covers: FR1 (streaming), FR2 (content blocks)
    #[test]
    fn test_full_sse_to_stream_chunk_text_flow() {
        use rustain::adapters::anthropic::sse::SseLineBuffer;
        use rustain::adapters::anthropic::stream::StreamTransformer;

        let mut buf = SseLineBuffer::new();
        let mut transformer = StreamTransformer::new();

        // Simulate real SSE data
        let raw = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":15}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

        let frames = buf.feed(raw);
        let mut all_chunks = Vec::new();
        for frame in &frames {
            all_chunks.extend(transformer.transform(frame));
        }

        // Should have: Usage, Text("Hello"), Text(" world"), Usage (from message_delta), TurnComplete
        let text_chunks: Vec<_> = all_chunks
            .iter()
            .filter(|c| matches!(c, StreamChunk::Text { .. }))
            .collect();
        assert_eq!(text_chunks.len(), 2);

        let turn_completes: Vec<_> = all_chunks
            .iter()
            .filter(|c| matches!(c, StreamChunk::TurnComplete { .. }))
            .collect();
        assert_eq!(turn_completes.len(), 1);
        match &turn_completes[0] {
            StreamChunk::TurnComplete { stop_reason } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            _ => unreachable!(),
        }
    }

    // ─── Task 9.3: apply_chunk integration ──────────────────────────────

    // Covers: FR1 (streaming), FR2 (content blocks)
    #[test]
    fn test_apply_chunk_full_turn_sequence() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();
        let (mut reducer, clock) = test_reducer_state(1000);

        // Add user message
        conv.messages.push(ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: "What is 2+2?".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 999,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
        });

        // Usage
        let a1 = apply_chunk_for_tests(
            &mut conv,
            &mut streaming,
            &mut reducer,
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 10,
                    output_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
            &clock,
        );
        assert_eq!(a1, ChunkAction::None);

        // Text chunks
        let a2 = apply_chunk_for_tests(
            &mut conv,
            &mut streaming,
            &mut reducer,
            StreamChunk::Text {
                content: "The answer ".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        assert_eq!(a2, ChunkAction::NeedsRedraw);

        let a3 = apply_chunk_for_tests(
            &mut conv,
            &mut streaming,
            &mut reducer,
            StreamChunk::Text {
                content: "is 4.".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        assert_eq!(a3, ChunkAction::NeedsRedraw);

        // Updated usage
        apply_chunk_for_tests(
            &mut conv,
            &mut streaming,
            &mut reducer,
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 10,
                    output_tokens: 8,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
            &clock,
        );

        // Turn complete
        let a4 = apply_chunk_for_tests(
            &mut conv,
            &mut streaming,
            &mut reducer,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );
        assert_eq!(
            a4,
            ChunkAction::TurnComplete {
                persist: true,
                trigger_title_generation: true, // 2 messages
            }
        );

        // Verify final conversation state
        assert_eq!(conv.messages.len(), 2);
        let assistant_msg = &conv.messages[1];
        assert_eq!(assistant_msg.role, MessageRole::Assistant);
        assert_eq!(assistant_msg.content, "The answer is 4.");
        assert_eq!(assistant_msg.token_count, Some(8));
        assert!(!streaming.is_streaming);
        assert_eq!(streaming.phase, StreamingPhase::Idle);
    }

    // ─── Task 9.7: MockProvider integration ─────────────────────────────

    // Covers: FR1 (streaming), FR2 (content blocks)
    #[test]
    fn test_mock_provider_apply_chunk_sequence() {
        // Simulate a complete turn with pre-defined StreamChunks
        let chunks: VecDeque<StreamChunk> = VecDeque::from(vec![
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 20,
                    output_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
            StreamChunk::Text {
                content: "Hello!".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 20,
                    output_tokens: 3,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ]);

        let mut conv = make_conversation();
        conv.messages.push(ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: "Hi".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 999,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
        });
        let mut streaming = make_streaming();
        let (mut reducer, clock) = test_reducer_state(1000);

        let mut actions = Vec::new();
        for chunk in chunks {
            actions.push(apply_chunk_for_tests(
                &mut conv,
                &mut streaming,
                &mut reducer,
                chunk,
                &clock,
            ));
        }

        assert_eq!(actions[0], ChunkAction::None); // Usage
        assert_eq!(actions[1], ChunkAction::NeedsRedraw); // Text
        assert_eq!(actions[2], ChunkAction::None); // Usage update
        assert_eq!(
            actions[3],
            ChunkAction::TurnComplete {
                persist: true,
                trigger_title_generation: true,
            }
        );

        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].content, "Hello!");
    }

    // ─── Task 9.4: AnthropicRequest construction ────────────────────────

    // Covers: FR1 (streaming), NFR11 (no API keys logged)
    #[test]
    fn test_anthropic_request_json_structure() {
        use rustain::adapters::anthropic::types::AnthropicRequest;

        let messages = vec![Message {
            role: MessageRole::User,
            content: "What is Rust?".into(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
        }];
        let options = CompletionOptions {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4096,
            system_prompt: "Be concise.".into(),
            temperature: Some(0.5),
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));
        let json = serde_json::to_value(&req).unwrap();

        assert_eq!(json["model"], "claude-sonnet-4-6");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"], "Be concise.");
        assert_eq!(json["stream"], true);
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"][0]["type"], "text");
        assert_eq!(json["messages"][0]["content"][0]["text"], "What is Rust?");
    }

    // ─── Task 9.5: Abort test ───────────────────────────────────────────

    // Covers: FR3 (abort)
    #[tokio::test]
    async fn test_anthropic_adapter_abort_succeeds() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::ports::StreamingProvider;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".into()),
            "claude-sonnet-4-6".into(),
            None,
        )
        .unwrap();

        // Abort with no active task should succeed
        let result = adapter.abort().await;
        assert!(result.is_ok());
    }

    // ─── Task 9.6: HTTP error mapping ───────────────────────────────────

    // Covers: FR14 (retry/backoff)
    #[tokio::test]
    async fn test_anthropic_adapter_401_returns_auth_error() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::errors::ProviderError;
        use rustain::domain::ports::StreamingProvider;

        // Use a mock server that returns 401
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .with_status(401)
            .with_body(r#"{"error":{"type":"authentication_error","message":"Invalid API Key"}}"#)
            .create_async()
            .await;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("invalid-key".into()),
            "claude-sonnet-4-6".into(),
            Some(server.url()),
        )
        .unwrap();

        let result = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "hi".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 1024,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        match result {
            Ok(_) => panic!("Expected error"),
            Err(ProviderError::AuthenticationFailed) => {}
            Err(other) => panic!("Expected AuthenticationFailed, got {:?}", other),
        }
        mock.assert_async().await;
    }

    // Covers: FR14 (retry/backoff)
    #[tokio::test]
    async fn test_anthropic_adapter_429_returns_rate_limit_error() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::errors::ProviderError;
        use rustain::domain::ports::StreamingProvider;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .with_status(429)
            .with_header("retry-after", "30")
            .with_body(r#"{"error":{"type":"rate_limit_error","message":"Rate limited"}}"#)
            .create_async()
            .await;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".into()),
            "claude-sonnet-4-6".into(),
            Some(server.url()),
        )
        .unwrap();

        let result = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "hi".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 1024,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        match result {
            Ok(_) => panic!("Expected error"),
            Err(ProviderError::RateLimited { retry_after_ms }) => {
                assert_eq!(retry_after_ms, Some(30_000));
            }
            Err(other) => panic!("Expected RateLimited, got {:?}", other),
        }
        mock.assert_async().await;
    }

    // Covers: FR14 (retry/backoff)
    #[tokio::test]
    async fn test_anthropic_adapter_500_returns_connection_error() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::errors::ProviderError;
        use rustain::domain::ports::StreamingProvider;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .with_status(500)
            .with_body("Internal Server Error")
            .create_async()
            .await;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".into()),
            "claude-sonnet-4-6".into(),
            Some(server.url()),
        )
        .unwrap();

        let result = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "hi".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 1024,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        match result {
            Ok(_) => panic!("Expected error"),
            Err(ProviderError::ConnectionFailed(msg)) => {
                assert!(msg.contains("500"));
            }
            Err(other) => panic!("Expected ConnectionFailed, got {:?}", other),
        }
        mock.assert_async().await;
    }

    // ─── Task 9.8: Provider conformance with mock HTTP ──────────────────

    // Covers: FR1 (streaming), FR2 (content blocks)
    #[tokio::test]
    async fn test_anthropic_adapter_streaming_with_mock_server() {
        use futures::StreamExt;
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::ports::StreamingProvider;

        let mut server = mockito::Server::new_async().await;
        let sse_body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi there!\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

        let mock = server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_body)
            .create_async()
            .await;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".into()),
            "claude-sonnet-4-6".into(),
            Some(server.url()),
        )
        .unwrap();

        let stream_result = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "Hello".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 1024,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => panic!("Expected Ok stream, got error: {:?}", e),
        };
        futures::pin_mut!(stream);

        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk);
        }

        // Verify we got text and turn complete
        let has_text = chunks.iter().any(|c| matches!(c, StreamChunk::Text { .. }));
        let has_complete = chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::TurnComplete { .. }));
        assert!(has_text, "Expected at least one Text chunk");
        assert!(has_complete, "Expected TurnComplete chunk");

        // Feed through apply_chunk and verify conversation state
        let mut conv = make_conversation();
        conv.messages.push(ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: "Hello".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 999,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
        });
        let mut streaming = make_streaming();
        let (mut reducer, clock) = test_reducer_state(1000);

        for chunk in chunks {
            apply_chunk_for_tests(&mut conv, &mut streaming, &mut reducer, chunk, &clock);
        }

        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].content, "Hi there!");
        assert_eq!(conv.messages[1].role, MessageRole::Assistant);
        assert!(!streaming.is_streaming);

        mock.assert_async().await;
    }

    // ─── Review fix: run_turn synthesizes TurnComplete on stream disconnect ──

    // Covers: FR1 (streaming), FR14 (retry/backoff)
    #[tokio::test]
    async fn test_run_turn_emits_error_on_stream_disconnect() {
        use futures::stream;
        use rustain::domain::events::AppEvent;
        use rustain::domain::ports::StreamingProvider;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        /// A mock provider that streams Text chunks without a TurnComplete.
        struct DisconnectingProvider;

        #[async_trait::async_trait]
        impl StreamingProvider for DisconnectingProvider {
            async fn stream_completion(
                &self,
                _messages: Vec<Message>,
                _options: CompletionOptions,
            ) -> Result<
                futures::stream::BoxStream<'static, StreamChunk>,
                rustain::domain::errors::ProviderError,
            > {
                // Stream that emits text then ends abruptly — no TurnComplete
                let chunks = vec![StreamChunk::Text {
                    content: "partial response".into(),
                    parent_tool_use_id: None,
                }];
                Ok(Box::pin(stream::iter(chunks)))
            }

            async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
                Ok(())
            }

            fn provider_id(&self) -> String {
                "mock-disconnect".to_string()
            }

            fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
                vec![]
            }

            async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
                Ok(())
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider: Arc<dyn StreamingProvider> = Arc::new(DisconnectingProvider);

        let security = Arc::new(rustain::adapters::noop::NoOpSecurity);
        let tools = Arc::new(rustain::adapters::noop::NoOpToolSet);
        let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
            16,
            Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
        );
        let tool_scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
            security.clone(),
            tools.clone(),
            approval_runtime,
            16,
        );
        rustain::infrastructure::runtime::turn::run_turn(
            provider,
            vec![Message {
                role: MessageRole::User,
                content: "hi".into(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
            }],
            CompletionOptions {
                model: "test".into(),
                max_tokens: 100,
                system_prompt: String::new(),
                temperature: None,
                tools: vec![],
            },
            tx,
            security,
            tools,
            tool_scheduler,
            "test-conv-id".to_string(),
            Arc::new(rustain::adapters::noop::NoOpStorage),
            make_conversation(),
            None,
            tokio_util::sync::CancellationToken::new(),
            Arc::new(rustain::adapters::noop::NoOpUsageLedger)
                as Arc<dyn rustain::domain::ports::UsageLedgerPort>,
            rustain::domain::services::model_router::ResolvedModel {
                model: "test".into(),
                tier: rustain::domain::models::router::ModelTier::CheapAgentic,
                escalation_reason: rustain::domain::models::router::EscalationReason::None,
            },
            None,
            0,
            None,
            "sess-test".into(),
        )
        .await;

        // Collect all events
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        // Should have: Text, Error (synthetic), TurnComplete (synthetic)
        assert!(
            events.len() >= 3,
            "Expected at least 3 events, got {}",
            events.len()
        );

        let has_error = events.iter().any(|e| {
            matches!(
                e,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::Error { .. },
                    ..
                }
            )
        });
        assert!(
            has_error,
            "Expected synthetic Error chunk on stream disconnect"
        );

        let has_turn_complete = events.iter().any(|e| {
            matches!(
                e,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::TurnComplete {
                        stop_reason: StopReason::Cancelled
                    },
                    ..
                }
            )
        });
        assert!(
            has_turn_complete,
            "Expected synthetic TurnComplete(Cancelled) on stream disconnect"
        );
    }

    // ─── Review fix: empty messages returns error ───────────────────────

    // Covers: FR1 (streaming)
    #[tokio::test]
    async fn test_anthropic_adapter_empty_messages_returns_error() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::errors::ProviderError;
        use rustain::domain::ports::StreamingProvider;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".into()),
            "claude-sonnet-4-6".into(),
            None,
        )
        .unwrap();

        let result = adapter
            .stream_completion(
                vec![], // Empty messages
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 1024,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        match result {
            Err(ProviderError::Other(msg)) => {
                assert!(
                    msg.contains("empty"),
                    "Error should mention empty messages: {}",
                    msg
                );
            }
            Err(other) => panic!(
                "Expected ProviderError::Other for empty messages, got {:?}",
                other
            ),
            Ok(_) => panic!("Expected error for empty messages, got Ok"),
        }
    }

    // ─── Task 9.9: Live smoke test ──────────────────────────────────────

    // Covers: FR1 (streaming)
    #[tokio::test]
    #[ignore] // Only run manually: cargo test test_anthropic_live_streaming -- --ignored
    async fn test_anthropic_live_streaming() {
        use futures::StreamExt;
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::ports::StreamingProvider;

        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .expect("ANTHROPIC_API_KEY must be set for live test");

        let adapter =
            AnthropicAdapter::new(AuthMode::ApiKey(api_key), "claude-sonnet-4-6".into(), None)
                .unwrap();

        let stream = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "Say exactly: hello".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 100,
                    system_prompt: "Respond with exactly what the user asks.".into(),
                    temperature: Some(0.0),
                    tools: vec![],
                },
            )
            .await;

        let stream = match stream {
            Ok(s) => s,
            Err(e) => panic!("stream_completion should succeed: {:?}", e),
        };
        futures::pin_mut!(stream);

        let mut has_text = false;
        let mut has_complete = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Text { .. } => has_text = true,
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                } => has_complete = true,
                _ => {}
            }
        }

        assert!(has_text, "Expected at least one Text chunk from live API");
        assert!(has_complete, "Expected TurnComplete(EndTurn) from live API");
    }

    // ─── Story 2.0: Provider config tests ──────────────────────────────

    // Covers: FR1 (streaming), NFR11 (no API keys logged)
    #[tokio::test]
    async fn test_api_key_auth_sends_x_api_key_header() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::ports::StreamingProvider;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-api-key-123")
            .match_header("anthropic-version", "2023-06-01")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
            .create_async()
            .await;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-api-key-123".into()),
            "claude-sonnet-4-6".into(),
            Some(server.url()),
        )
        .unwrap();

        let result = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "hi".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 100,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        assert!(result.is_ok(), "Request should succeed with ApiKey auth");
        mock.assert_async().await;
    }

    // Covers: FR1 (streaming), NFR11 (no API keys logged)
    #[tokio::test]
    async fn test_bearer_token_auth_sends_authorization_header() {
        use rustain::adapters::anthropic::AnthropicAdapter;
        use rustain::domain::ports::StreamingProvider;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("authorization", "Bearer my-bearer-token-xyz")
            .match_header("anthropic-version", "2023-06-01")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
            .create_async()
            .await;

        let adapter = AnthropicAdapter::new(
            AuthMode::BearerToken("my-bearer-token-xyz".into()),
            "glm-4.7".into(),
            Some(server.url()),
        )
        .unwrap();

        let result = adapter
            .stream_completion(
                vec![Message {
                    role: MessageRole::User,
                    content: "hi".into(),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                }],
                CompletionOptions {
                    model: "glm-4.7".into(),
                    max_tokens: 100,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: vec![],
                },
            )
            .await;

        assert!(
            result.is_ok(),
            "Request should succeed with BearerToken auth"
        );
        mock.assert_async().await;
    }

    // Covers: FR1 (streaming)
    #[test]
    fn test_custom_base_url_passed_through() {
        use rustain::adapters::anthropic::AnthropicAdapter;

        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("test-key".into()),
            "claude-sonnet-4-6".into(),
            Some("https://api.z.ai/api/anthropic".into()),
        )
        .unwrap();

        let debug = format!("{:?}", adapter);
        assert!(
            debug.contains("https://api.z.ai/api/anthropic"),
            "Custom base URL should appear in debug output"
        );
    }

    // Covers: FR1 (streaming)
    #[test]
    fn test_model_override_reflected_in_adapter() {
        use rustain::adapters::anthropic::AnthropicAdapter;

        let adapter = AnthropicAdapter::new(
            AuthMode::BearerToken("token".into()),
            "glm-4.7".into(),
            None,
        )
        .unwrap();

        let debug = format!("{:?}", adapter);
        assert!(
            debug.contains("glm-4.7"),
            "Overridden model name should appear in debug output"
        );
    }

    // Covers: NFR11 (no API keys logged)
    #[test]
    fn test_debug_shows_auth_mode_and_base_url() {
        use rustain::adapters::anthropic::AnthropicAdapter;

        // ApiKey mode
        let adapter = AnthropicAdapter::new(
            AuthMode::ApiKey("secret".into()),
            "claude-sonnet-4-6".into(),
            None,
        )
        .unwrap();
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("ApiKey(***)"));
        assert!(debug.contains("api.anthropic.com"));
        assert!(!debug.contains("secret"));

        // BearerToken mode with custom URL
        let adapter = AnthropicAdapter::new(
            AuthMode::BearerToken("secret".into()),
            "glm-4.7".into(),
            Some("https://api.z.ai/api/anthropic".into()),
        )
        .unwrap();
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("BearerToken(***)"));
        assert!(debug.contains("api.z.ai"));
        assert!(!debug.contains("secret"));
    }
}
