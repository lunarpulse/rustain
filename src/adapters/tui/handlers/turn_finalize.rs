//! Turn-finalization handler — cross-epic utility.
//!
//! `finalize_streaming_turn` folds an in-flight streaming turn into the
//! conversation instead of discarding it: unfinished tool calls are marked
//! `[aborted]`, the partial assistant reply is appended with
//! `StopReason::Cancelled`, the turn `JoinHandle` is aborted, and streaming
//! state is reset.
//!
//! 2 call sites in `event_loop.rs` dispatch arms: `InputAction::CancelOrQuit`
//! (AC12) and `InputAction::Quit` (Story 19.3, code review D3).
//!
//! WHY ONE BODY: the Quit arm had no finalization at all until 19.3's code
//! review. Because the partial reply lives in `StreamingState` and never in
//! `Conversation`, and shutdown persists the conversation with
//! `clean_exit = true`, a quit mid-stream silently dropped the response *and*
//! suppressed the crash-recovery prompt that would otherwise have surfaced it.
//! Re-inlining either copy re-opens that gap.

use crate::domain::models::conversation::generate_conversation_id;
use crate::domain::models::{
    ChannelKind, ChatMessage, Conversation, MessageRole, StopReason, StreamingPhase,
    StreamingState, ToolResultInfo,
};

/// Finalize an in-flight streaming turn, preserving whatever the provider
/// already produced.
///
/// Callers are responsible for the `streaming.is_streaming` guard and for any
/// caller-specific follow-up (the cancel path additionally drains the turn
/// queue and returns focus to the input box; the quit path does neither).
pub fn finalize_streaming_turn(
    streaming: &mut StreamingState,
    conversation: &mut Conversation,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
) {
    // AC12: finalize active tool calls with [aborted] before clearing.
    for (_, tc) in streaming.active_tool_calls.iter_mut() {
        if tc.result.is_none() {
            tc.result = Some(ToolResultInfo {
                content: "[aborted]".to_string(),
                is_error: true,
            });
            tc.completed_at_ms =
                Some(crate::domain::models::session_meta::now_unix() as u64 * 1000);
        }
    }

    // Preserve the partial response rather than discarding it.
    if !streaming.current_text_buffer.is_empty() || !streaming.active_tool_calls.is_empty() {
        let content = std::mem::take(&mut streaming.current_text_buffer);
        conversation.messages.push(ChatMessage {
            id: generate_conversation_id(),
            role: MessageRole::Assistant,
            content,
            content_blocks: std::mem::take(&mut streaming.current_blocks),
            tool_calls: streaming
                .active_tool_calls
                .drain()
                .map(|(_, v)| v)
                .collect(),
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(StopReason::Cancelled),
            synthetic: false,
            images: vec![],
            origin: ChannelKind::Terminal,
            authorship: Default::default(),
            retracted_at_ms: None,
        });
    }

    if let Some(handle) = active_turn.take() {
        handle.abort();
    }

    streaming.is_streaming = false;
    streaming.phase = StreamingPhase::Idle;
    streaming.current_blocks.clear();
    streaming.active_tool_calls.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ToolCallInfo;

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
        }
    }

    /// Story 19.3 code review D3 — the defect this function exists to prevent.
    /// Before the fix, `InputAction::Quit` left the partial reply in
    /// `StreamingState`, shutdown persisted only `Conversation`, and the
    /// response was gone. Deleting the fold below turns this test red.
    #[test]
    fn partial_reply_is_folded_into_the_conversation_rather_than_discarded() {
        let mut streaming = streaming_with("half an answer");
        let mut conversation = Conversation::default();
        let mut active_turn = None;

        finalize_streaming_turn(&mut streaming, &mut conversation, &mut active_turn);

        assert_eq!(conversation.messages.len(), 1, "the partial must be kept");
        let msg = &conversation.messages[0];
        assert_eq!(msg.content, "half an answer");
        assert!(matches!(msg.role, MessageRole::Assistant));
        assert_eq!(msg.stop_reason, Some(StopReason::Cancelled));
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
        let mut active_turn = None;

        finalize_streaming_turn(&mut streaming, &mut conversation, &mut active_turn);

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
        let mut active_turn = None;

        finalize_streaming_turn(&mut streaming, &mut conversation, &mut active_turn);

        assert!(conversation.messages.is_empty());
        assert!(!streaming.is_streaming);
    }
}
