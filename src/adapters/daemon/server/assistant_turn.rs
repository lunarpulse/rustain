//! Operator assistant-turn folding. The daemon is the sole persister of a
//! daemon-hosted conversation: attach clients fold forwarded chunks for their
//! view only, so the committed transcript, including operator-private provider
//! replay state (Story 20.1d1b), is written here.
use super::*;
use crate::domain::models::ProviderReplayItem;

/// The open operator turn: the visible transcript text and the provider replay
/// items its successful completion carried. Replay never enters `text`, the
/// channel response or the observer stream.
#[derive(Default)]
pub(super) struct AssistantTurnBuffer {
    text: String,
    replay: Vec<ProviderReplayItem>,
}

impl AssistantTurnBuffer {
    fn clear(&mut self) {
        self.text.clear();
        self.replay.clear();
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty() && self.replay.is_empty()
    }
}

impl AttachServer {
    /// Fold one operator provider chunk into the open turn. Returns `true` for an
    /// operator-private chunk, which is retained here and never fanned out.
    pub(super) async fn fold_operator_chunk(
        &self,
        chunk: &StreamChunk,
        turn: &mut AssistantTurnBuffer,
    ) -> bool {
        if let StreamChunk::ProviderReplay { item } = chunk {
            self.operator_turn_active.store(true, Ordering::Release);
            turn.replay.push(item.clone());
            return true;
        }
        if chunk.is_operator_private() {
            self.retain_operator_private_chunk(chunk).await;
            self.operator_turn_active.store(true, Ordering::Release);
            return true;
        }
        match chunk {
            StreamChunk::Text { content, .. } => {
                self.operator_turn_active.store(true, Ordering::Release);
                turn.text.push_str(content);
            }
            StreamChunk::TurnComplete { stop_reason } => {
                self.operator_turn_active.store(false, Ordering::Release);
                if *stop_reason == StopReason::Cancelled {
                    // A cancelled turn has no committed assistant answer. In
                    // particular, an aborted inbound turn can have already
                    // streamed text, which must not become the prefix of the
                    // next turn's answer.
                    turn.clear();
                } else {
                    self.commit_assistant_turn(turn, stop_reason).await;
                    turn.clear();
                    self.turn_complete.notify_waiters();
                }
                if let Some(router) = &self.urgency_router {
                    let queued = router.idle_queue_snapshot().await;
                    let count = queued.len();
                    match self.surface_queued_interactions(queued).await {
                        Ok(()) => router.drain_idle_queue(count).await,
                        Err(error) => tracing::error!(
                            %error,
                            "failed to surface queued peer interactions; will retry at next idle"
                        ),
                    }
                }
            }
            StreamChunk::ToolUse { id, name, .. } => {
                turn.text
                    .push_str(&format!("\n[tool use: {name} (id: {id})]\n"));
                self.operator_turn_active.store(true, Ordering::Release);
            }
            StreamChunk::ToolResult { content, .. } => {
                turn.text.push_str(&format!("[tool result: {content}]\n"));
                self.operator_turn_active.store(true, Ordering::Release);
            }
            _ => {}
        }
        false
    }

    /// Retain exact proposal evidence without folding its private arguments or
    /// result into the public prose transcript. Snapshots project these tools
    /// out; the stored conversation and model continuation remain complete.
    async fn retain_operator_private_chunk(&self, chunk: &StreamChunk) {
        use crate::domain::models::{ToolCallInfo, ToolResultInfo};
        let mut conversation = self.conversation.lock().await;
        match chunk {
            StreamChunk::ToolUse { id, name, input } => {
                conversation.messages.push(ChatMessage {
                    id: generate_message_id(),
                    role: MessageRole::Assistant,
                    tool_calls: vec![ToolCallInfo {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        result: None,
                        started_at_ms: None,
                        completed_at_ms: None,
                        status: None,
                    }],
                    created_at: crate::domain::models::session_meta::now_unix(),
                    ..Default::default()
                });
            }
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
                diff,
                ..
            } => {
                if let Some(tool) = conversation
                    .messages
                    .iter_mut()
                    .rev()
                    .flat_map(|message| message.tool_calls.iter_mut())
                    .find(|tool| {
                        tool.id == *id
                            && tool.name
                                == crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME
                    })
                {
                    tool.result = Some(ToolResultInfo {
                        content: content.clone(),
                        is_error: *is_error,
                        diff: diff.clone(),
                    });
                }
            }
            _ => return,
        }
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
        if let Err(error) = self.core.storage.save_conversation(&conversation).await {
            tracing::warn!(%error, "persisting operator-private tool evidence failed");
        }
    }

    /// Append the accumulated assistant message (origin-tagged) and persist (AC4).
    /// Replay items become operator-private content blocks, so the next turn's
    /// `message_builder` replays them, also after a daemon restart; the channel
    /// response stays the plain text.
    async fn commit_assistant_turn(
        &self,
        turn: &mut AssistantTurnBuffer,
        stop_reason: &StopReason,
    ) {
        if turn.text.is_empty() {
            return;
        }
        let origin = *self.active_channel_origin.lock().await;
        let mut conv = self.conversation.lock().await;
        conv.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: turn.text.clone(),
            content_blocks: turn
                .replay
                .drain(..)
                .map(ContentBlockType::ProviderReplay)
                .collect(),
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(stop_reason.clone()),
            synthetic: false,
            images: vec![],
            origin,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });
        let now = crate::domain::models::session_meta::now_unix();
        conv.updated_at = now;
        conv.last_response_at = Some(now);
        if let Err(e) = self.core.storage.save_conversation(&conv).await {
            tracing::warn!(error = %e, "daemon: persisting conversation after turn failed");
        }
        drop(conv);
        if let Some(tx) = self.pending_channel_response_tx.lock().await.take() {
            let _ = tx.send(turn.text.clone());
        }
    }
}
