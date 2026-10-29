//! Local TUI interrupted-turn display policy.
//!
//! Runtime retires the owning producer before calling this seam. The reducer
//! receives its terminal exactly once, but its output is a current-tab display,
//! not a durable turn. Daemon, ACP and ask retain their own terminal policies.

use crate::domain::clock::Clock;
use crate::domain::models::conversation::generate_message_id;
use crate::domain::models::{
    ChatMessage, ContentBlockType, Conversation, StopReason, StreamChunk, StreamingState,
    ToolResultInfo,
};
use crate::domain::services::reducer::{ReducerState, reduce};

/// Close and drain the reducer synchronously, retaining already-visible parts.
/// An idle close must not call reduce: its terminal arm constructs an empty turn.
pub fn finalize_streaming_turn(
    streaming: &mut StreamingState,
    conversation: &mut Conversation,
    reducer: &mut ReducerState,
    clock: &dyn Clock,
) {
    let mut partial = if reducer.open_turn.is_some() {
        reduce(
            reducer,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled,
            },
            clock,
        );
        // Reuse the real turn-to-row projection so flushed prose, reasoning,
        // tool results and provider call identities are not lost at interruption.
        let mut display = Conversation::default();
        let mut reasoning = Vec::new();
        if let Some(mut turn) = reducer.committed_turn.take() {
            // The legacy mirror concatenates reasoning into prose. Keep the
            // operator's thinking presentation distinct in an unbacked display,
            // moving it out before that compatibility projection runs.
            for part in &mut turn.parts {
                if let crate::domain::models::TurnPart::Reasoning { text, .. } = part {
                    if !text.is_empty() {
                        reasoning.push(ContentBlockType::Thinking(std::mem::take(text)));
                    }
                }
            }
            display.turns.push(turn);
        }
        display.rebuild_messages_mirror();
        display.messages.pop().map(|mut row| {
            row.content_blocks.extend(reasoning);
            row
        })
    } else if streaming.is_streaming
        && (!streaming.current_text_buffer.is_empty()
            || !streaming.active_tool_calls.is_empty()
            || !streaming.thinking_buffer.is_empty()
            || streaming
                .current_blocks
                .iter()
                .any(|block| matches!(block, ContentBlockType::Thinking(_))))
    {
        // Preserve adapter-only visible state too (e.g. an early local render).
        let mut blocks = std::mem::take(&mut streaming.current_blocks);
        if !streaming.thinking_buffer.is_empty() {
            blocks.push(ContentBlockType::Thinking(std::mem::take(
                &mut streaming.thinking_buffer,
            )));
        }
        Some(ChatMessage {
            content: std::mem::take(&mut streaming.current_text_buffer),
            content_blocks: blocks,
            tool_calls: streaming
                .active_tool_calls
                .drain()
                .map(|(_, call)| call)
                .collect(),
            ..Default::default()
        })
    } else {
        None
    };

    if let Some(message) = &mut partial {
        // Opaque replay and response ownership never outlive the cancelled
        // reducer turn, even in the temporary display.
        message.content_blocks.retain(|block| {
            !matches!(
                block,
                ContentBlockType::ProviderReplay(_) | ContentBlockType::ResponseSpan(_)
            )
        });
        let visible = !message.content.is_empty()
            || !message.tool_calls.is_empty()
            || message
                .content_blocks
                .iter()
                .any(|block| matches!(block, ContentBlockType::Thinking(text) if !text.is_empty()));
        if visible {
            message.id = generate_message_id();
            message.stop_reason = Some(StopReason::Cancelled);
            message.created_at = clock.wall_now_ms() / 1000;
            message
                .content_blocks
                .push(ContentBlockType::EphemeralDisplay);
            for call in &mut message.tool_calls {
                if call.result.is_none() {
                    call.result = Some(ToolResultInfo {
                        content: "[aborted]".into(),
                        is_error: true,
                        diff: crate::domain::models::WriteDiffState::NotAWrite,
                        images: Vec::new(),
                    });
                    call.completed_at_ms = Some(clock.wall_now_ms().max(0) as u64);
                    call.status = Some("Cancelled".into());
                }
            }
            conversation
                .messages
                .push(partial.take().expect("visible partial"));
        }
    }
    if let Some(usage) = reducer.pending_usage.take() {
        conversation.usage = Some(usage);
    }
    // Normally consumed above. Never leave an old terminal for the next turn.
    reducer.committed_turn.take();
    *streaming = StreamingState::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{MessageRole, ToolCallInfo};

    fn finalize_mirror(streaming: &mut StreamingState, conversation: &mut Conversation) {
        let clock = crate::domain::clock::SystemClock::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        finalize_streaming_turn(streaming, conversation, &mut reducer, &clock);
    }

    fn streaming_with(text: &str) -> StreamingState {
        StreamingState {
            is_streaming: true,
            current_text_buffer: text.to_string(),
            ..Default::default()
        }
    }

    fn unfinished_tool_call(id: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({}),
            result: None,
            started_at_ms: Some(1),
            completed_at_ms: None,
            status: None,
            provider_id: None,
        }
    }

    /// Current-tab visibility is preserved without promoting a partial into
    /// session storage or the next model request.
    #[test]
    fn partial_reply_is_folded_into_the_conversation_rather_than_discarded() {
        let mut streaming = streaming_with("half an answer");
        let mut conversation = Conversation::default();

        finalize_mirror(&mut streaming, &mut conversation);

        assert_eq!(conversation.messages.len(), 1, "the partial must be kept");
        let msg = &conversation.messages[0];
        assert_eq!(msg.content, "half an answer");
        assert!(matches!(msg.role, MessageRole::Assistant));
        assert_eq!(msg.stop_reason, Some(StopReason::Cancelled));
        assert!(msg.is_ephemeral_display());
        assert!(
            crate::domain::models::conversation::PersistedConversation::from_conversation(
                &conversation,
            )
            .messages
            .is_empty(),
            "a partial remains visible, never durable",
        );
        assert!(
            streaming.current_text_buffer.is_empty(),
            "the buffer must be moved, not copied"
        );
        assert!(!streaming.is_streaming);
    }

    /// AC12 — a tool call still running when the turn ends is recorded as
    /// aborted, so the transcript never implies it completed.
    #[test]
    fn unfinished_tool_calls_are_recorded_as_aborted() {
        let mut streaming = streaming_with("");
        streaming
            .active_tool_calls
            .insert("t1".to_string(), unfinished_tool_call("t1"));
        let mut conversation = Conversation::default();

        finalize_mirror(&mut streaming, &mut conversation);

        let msg = conversation
            .messages
            .first()
            .expect("an in-flight tool call alone must still produce a message");
        let tc = msg
            .tool_calls
            .first()
            .expect("the aborted tool call must ride along");
        let result = tc.result.as_ref().expect("result must be filled in");
        assert_eq!(result.content, "[aborted]");
        assert!(result.is_error);
        assert!(tc.completed_at_ms.is_some());
        assert!(streaming.active_tool_calls.is_empty());
    }

    /// A quit with nothing in flight must not invent an empty assistant turn.
    #[test]
    fn nothing_in_flight_pushes_no_message() {
        let mut streaming = streaming_with("");
        let mut conversation = Conversation::default();

        finalize_mirror(&mut streaming, &mut conversation);

        assert!(conversation.messages.is_empty());
        assert!(!streaming.is_streaming);
    }
    #[test]
    fn reducer_close_drains_usage_replay_and_liveness_and_is_idempotent() {
        let clock = crate::domain::clock::MockClock::at_wall_ms(123_000);
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut streaming = StreamingState::default();
        let mut conversation = Conversation::default();
        let replay = crate::domain::models::ProviderReplayItem {
            provider: "xai".into(),
            model: "m".into(),
            payload: std::sync::Arc::new(serde_json::json!({"encrypted_content":"CANCEL-REPLAY"})),
            scope: Some("scope".into()),
        };
        for chunk in [
            StreamChunk::Text {
                content: "before tools".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::ToolUse {
                id: "wire".into(),
                name: "echo".into(),
                input: serde_json::json!({}),
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::ToolUse,
            },
            StreamChunk::ToolResult {
                id: "wire".into(),
                content: "visible result".into(),
                is_error: false,
                operator_private: false,
                diff: crate::domain::models::WriteDiffState::NotAWrite,
                images: Vec::new(),
            },
            StreamChunk::Thinking {
                content: "visible reasoning".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::Text {
                content: "after tools".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::ProviderReplay { item: replay },
            StreamChunk::ToolUse {
                id: "unresolved".into(),
                name: "held".into(),
                input: serde_json::json!({}),
            },
            StreamChunk::Usage {
                usage: crate::domain::models::UsageInfo {
                    input_tokens: 17,
                    output_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
        ] {
            reduce(&mut reducer, chunk, &clock);
        }
        reducer.set_progress("unresolved", 1, 2);
        reducer.set_tail("unresolved", "running".into());
        finalize_streaming_turn(&mut streaming, &mut conversation, &mut reducer, &clock);
        assert!(reducer.open_turn.is_none());
        assert!(reducer.committed_turn.is_none());
        assert!(reducer.open_prose.is_none());
        assert!(reducer.pending_invocations.is_empty());
        assert!(reducer.pending_usage.is_none());
        let live = reducer.liveness();
        assert!(live.active_tool_name.is_none() && live.progress.is_none() && live.tail.is_none());
        assert_eq!(conversation.usage.as_ref().unwrap().input_tokens, 17);
        assert!(conversation.turns.is_empty());
        let row = &conversation.messages[0];
        assert!(row.content.contains("before tools") && row.content.contains("after tools"));
        assert!(
            row.content_blocks
                .contains(&ContentBlockType::Thinking("visible reasoning".into()))
        );
        assert_eq!(
            row.tool_calls[0].result.as_ref().unwrap().content,
            "visible result"
        );
        assert_eq!(
            row.tool_calls[1].result.as_ref().unwrap().content,
            "[aborted]"
        );
        assert!(
            !serde_json::to_string(row)
                .unwrap()
                .contains("CANCEL-REPLAY")
        );
        let id = row.id.clone();
        finalize_streaming_turn(&mut streaming, &mut conversation, &mut reducer, &clock);
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].id, id);
        reduce(
            &mut reducer,
            StreamChunk::Text {
                content: "fresh".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        assert_ne!(reducer.open_turn.as_ref().unwrap().id.0, id);
        assert!(reducer.pending_invocations.is_empty());
    }

    #[test]
    fn reasoning_only_is_visible_but_empty_and_replay_only_make_no_row() {
        for mode in 0..3 {
            let clock = crate::domain::clock::MockClock::at_wall_ms(123_000);
            let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
            if mode == 1 {
                reduce(
                    &mut reducer,
                    StreamChunk::Thinking {
                        content: "reasoning".into(),
                        parent_tool_use_id: None,
                    },
                    &clock,
                );
            } else if mode == 2 {
                reduce(
                    &mut reducer,
                    StreamChunk::ProviderReplay {
                        item: crate::domain::models::ProviderReplayItem {
                            provider: "xai".into(),
                            model: "m".into(),
                            payload: std::sync::Arc::new(
                                serde_json::json!({"private":"replay-only"}),
                            ),
                            scope: None,
                        },
                    },
                    &clock,
                );
            }
            let mut conversation = Conversation::default();
            let mut streaming = StreamingState::default();
            finalize_streaming_turn(&mut streaming, &mut conversation, &mut reducer, &clock);
            assert_eq!(conversation.messages.len(), usize::from(mode == 1));
            assert!(conversation.turns.is_empty());
            assert!(reducer.open_turn.is_none() && reducer.committed_turn.is_none());
        }
    }
}
