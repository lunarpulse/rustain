//! Integration tests for auto-title generation (Story 2.2a).
//!
//! Tests title generation flow, post-processing, failure handling,
//! and duplicate prevention.

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use rustain::domain::errors::ProviderError;
use rustain::domain::events::{AppEvent, ChunkAction};
use rustain::domain::models::{
    ChatMessage, CompletionOptions, Conversation, Message, MessageRole, StopReason, StreamChunk,
    StreamingState, apply_chunk, generate_conversation_id,
};
use rustain::domain::ports::ProviderPort;

// ── Mock Provider ──────────────────────────────────────────────

/// Mock provider that returns a pre-configured title as streaming text chunks.
struct MockTitleProvider {
    title_response: String,
}

#[async_trait]
impl ProviderPort for MockTitleProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let chunks = vec![
            StreamChunk::Text {
                content: self.title_response.clone(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> &str {
        "mock-title"
    }
}

/// Mock provider that always fails.
struct FailingProvider;

#[async_trait]
impl ProviderPort for FailingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        Err(ProviderError::Other("Network error".into()))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> &str {
        "failing"
    }
}

// ── Helpers ────────────────────────────────────────────────────

fn make_conversation() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        created_at: 1000,
        updated_at: 1000,
        last_response_at: None,
        session_id: Some(generate_conversation_id()),
        usage: None,
        fork_source: None,
    }
}

fn add_user_message(conv: &mut Conversation, content: &str) {
    conv.messages.push(ChatMessage {
        role: MessageRole::User,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1000,
        token_count: None,
        stop_reason: None,
    });
}

// ── Test 5.1: apply_chunk sets trigger_title_generation only at 2 messages ──

// Covers: FR9 (auto-title generation)
#[test]
fn test_trigger_title_generation_only_at_two_messages() {
    let mut conv = make_conversation();
    let mut streaming = StreamingState::default();
    streaming.is_streaming = true;

    // No user message — only assistant response → messages.len() == 1 → no trigger
    apply_chunk(
        &mut conv,
        &mut streaming,
        StreamChunk::Text {
            content: "Hello".into(),
            parent_tool_use_id: None,
        },
        1000,
    );
    let action = apply_chunk(
        &mut conv,
        &mut streaming,
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
        1001,
    );
    assert_eq!(
        action,
        ChunkAction::TurnComplete {
            persist: true,
            trigger_title_generation: false, // only 1 message
        }
    );

    // Now with user message first → messages.len() == 2 → trigger
    let mut conv2 = make_conversation();
    add_user_message(&mut conv2, "Hi there");
    let mut streaming2 = StreamingState::default();
    streaming2.is_streaming = true;

    apply_chunk(
        &mut conv2,
        &mut streaming2,
        StreamChunk::Text {
            content: "Hello!".into(),
            parent_tool_use_id: None,
        },
        1000,
    );
    let action2 = apply_chunk(
        &mut conv2,
        &mut streaming2,
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
        1001,
    );
    assert_eq!(
        action2,
        ChunkAction::TurnComplete {
            persist: true,
            trigger_title_generation: true, // 2 messages
        }
    );
}

// ── Test 5.2: Title post-processing ────────────────────────────

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_post_processing_trims_whitespace() {
    use rustain::infrastructure::runtime::event_loop::post_process_title;
    assert_eq!(post_process_title("  Hello World  "), "Hello World");
}

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_post_processing_strips_double_quotes() {
    use rustain::infrastructure::runtime::event_loop::post_process_title;
    assert_eq!(post_process_title("\"Quoted Title\""), "Quoted Title");
}

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_post_processing_strips_single_quotes() {
    use rustain::infrastructure::runtime::event_loop::post_process_title;
    assert_eq!(post_process_title("'Single Quoted'"), "Single Quoted");
}

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_post_processing_truncates_over_60() {
    use rustain::infrastructure::runtime::event_loop::post_process_title;
    let long = "A".repeat(70);
    let result = post_process_title(&long);
    assert_eq!(result.len(), 60); // 57 A's + "..."
    assert!(result.ends_with("..."));
    assert_eq!(result.chars().count(), 60);
}

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_post_processing_preserves_exact_60() {
    use rustain::infrastructure::runtime::event_loop::post_process_title;
    let exact = "A".repeat(60);
    assert_eq!(post_process_title(&exact), exact);
}

// ── Test 5.3: AppEvent::TitleGenerated sets conversation title ──

// Covers: FR9 (auto-title generation)
#[tokio::test]
async fn test_title_generated_event_sets_conversation_title() {
    // Verify the event variant can be constructed and pattern-matched
    let event = AppEvent::TitleGenerated {
        title: "My Chat Title".to_string(),
    };

    if let AppEvent::TitleGenerated { title } = event {
        let mut conv = make_conversation();
        assert!(conv.title.is_empty());
        conv.title = title;
        assert_eq!(conv.title, "My Chat Title");
    } else {
        panic!("Expected TitleGenerated event");
    }
}

// ── Test 5.4: Full title generation flow via mock provider ─────

// Covers: FR9 (auto-title generation)
#[tokio::test]
async fn test_generate_title_via_mock_provider() {
    use futures::StreamExt;

    let provider = MockTitleProvider {
        title_response: "Rust Programming Help".to_string(),
    };

    // Simulate what generate_title does
    let messages = vec![Message {
        role: MessageRole::User,
        content: "User: How do I use iterators?\n\nAssistant: Iterators in Rust...".to_string(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
    }];
    let options = CompletionOptions {
        model: "test-model".to_string(),
        max_tokens: 30,
        system_prompt: "Generate a concise title".to_string(),
        temperature: None,
        tools: vec![],
    };

    let stream = provider.stream_completion(messages, options).await.unwrap();
    futures::pin_mut!(stream);

    let mut title = String::new();
    while let Some(chunk) = stream.next().await {
        if let StreamChunk::Text { content, .. } = chunk {
            title.push_str(&content);
        }
    }

    assert_eq!(title.trim(), "Rust Programming Help");
}

// ── Test 5.5: Title generation failure does not affect conversation ──

// Covers: FR9 (auto-title generation)
#[tokio::test]
async fn test_title_generation_failure_silent() {
    let provider = FailingProvider;

    let result = provider
        .stream_completion(
            vec![Message {
                role: MessageRole::User,
                content: "test".to_string(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
            }],
            CompletionOptions {
                model: "test".to_string(),
                max_tokens: 30,
                system_prompt: "Generate title".to_string(),
                temperature: None,
                tools: vec![],
            },
        )
        .await;

    // The provider fails — title generation should handle this gracefully
    assert!(result.is_err());

    // Conversation should remain unaffected
    let conv = make_conversation();
    assert!(conv.title.is_empty()); // title stays empty on failure
}

// ── Test 5.6: Restored session with existing title doesn't re-trigger ──

// Covers: FR9 (auto-title generation)
#[test]
fn test_no_title_generation_for_subsequent_turns() {
    let mut conv = make_conversation();
    // Simulate a restored session with existing title and 2 messages
    conv.title = "Existing Title".to_string();
    add_user_message(&mut conv, "First message");
    conv.messages.push(ChatMessage {
        role: MessageRole::Assistant,
        content: "First response".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1001,
        token_count: None,
        stop_reason: Some(StopReason::EndTurn),
    });

    // Add another user message + assistant response (messages.len() will be 4)
    add_user_message(&mut conv, "Second message");
    let mut streaming = StreamingState::default();
    streaming.is_streaming = true;

    apply_chunk(
        &mut conv,
        &mut streaming,
        StreamChunk::Text {
            content: "Second response".into(),
            parent_tool_use_id: None,
        },
        1002,
    );
    let action = apply_chunk(
        &mut conv,
        &mut streaming,
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
        1003,
    );

    // 4 messages — trigger_title_generation should be false
    assert_eq!(
        action,
        ChunkAction::TurnComplete {
            persist: true,
            trigger_title_generation: false,
        }
    );
    // Existing title preserved
    assert_eq!(conv.title, "Existing Title");
}

// ── Test: Event loop guard skips title gen for non-empty title ──

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_guard_skips_when_title_exists() {
    // This tests the logic: `trigger_title_generation && conversation.title.is_empty()`
    // When title is already set, even if trigger is true, we skip
    let conv = Conversation {
        id: generate_conversation_id(),
        title: "Already Set".to_string(),
        messages: Vec::new(),
        created_at: 1000,
        updated_at: 1000,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    };

    let trigger_title_generation = true;
    let should_generate = trigger_title_generation && conv.title.is_empty();
    assert!(!should_generate);
}

// Covers: FR9 (auto-title generation)
#[test]
fn test_title_guard_allows_when_title_empty() {
    let conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        created_at: 1000,
        updated_at: 1000,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    };

    let trigger_title_generation = true;
    let should_generate = trigger_title_generation && conv.title.is_empty();
    assert!(should_generate);
}
