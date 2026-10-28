//! Operator assistant-turn folding. The daemon is the sole persister of a
//! daemon-hosted conversation: attach clients fold forwarded chunks for their
//! view only, so the committed transcript, including operator-private provider
//! replay state (Story 20.1d1b), is written here.
//!
//! Story 20.1d4a T2: every provider response is committed as its own assistant
//! message, with its visible text, reasoning, tool calls (by the provider's
//! ids), replay items and one response span, so a restarted daemon rebuilds
//! the history the turn loop sent in-turn. Results attach to their call when
//! they arrive and are saved at once.
use super::*;
use crate::domain::models::{
    ProviderReplayItem, ResponseSpanBuilder, ToolCallInfo, ToolResultInfo, WriteDiffState,
};

/// The open provider response of the operator turn. `text` is the channel
/// reply's prose fold (tool lines included) and nothing else reads it; the
/// span, calls and replay items are the response's own record. Replay never
/// enters `text`, the channel response or the observer stream. `committed`
/// holds the ids of the messages this operator turn has committed so far: a
/// later result attaches only to their calls, never to an earlier turn's or a
/// peer row's.
#[derive(Default)]
pub(super) struct AssistantTurnBuffer {
    text: String,
    span: ResponseSpanBuilder,
    calls: Vec<ToolCallInfo>,
    replay: Vec<ProviderReplayItem>,
    committed: Vec<String>,
}

impl AssistantTurnBuffer {
    fn clear(&mut self) {
        self.text.clear();
        self.span = ResponseSpanBuilder::default();
        self.calls.clear();
        self.replay.clear();
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.span.is_empty()
            && self.calls.is_empty()
            && self.replay.is_empty()
            && self.committed.is_empty()
    }

    fn call(&mut self, id: &str, name: &str, input: &serde_json::Value) {
        self.calls.push(ToolCallInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            input: input.clone(),
            result: None,
            started_at_ms: None,
            completed_at_ms: None,
            status: None,
            provider_id: Some(id.to_owned()),
        });
        self.span.call();
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
            turn.span.replay();
            return true;
        }
        if chunk.is_operator_private() {
            self.retain_operator_private_chunk(chunk, turn).await;
            self.operator_turn_active.store(true, Ordering::Release);
            return true;
        }
        match chunk {
            StreamChunk::Text { content, .. } => {
                self.operator_turn_active.store(true, Ordering::Release);
                turn.text.push_str(content);
                turn.span.text(content);
            }
            StreamChunk::Thinking { content, .. } => {
                turn.span.reasoning(content);
            }
            StreamChunk::TurnComplete { stop_reason } => {
                self.operator_turn_active.store(false, Ordering::Release);
                if *stop_reason == StopReason::Cancelled {
                    // A cancelled turn has no committed assistant answer. In
                    // particular, an aborted inbound turn can have already
                    // streamed text, which must not become the prefix of the
                    // next turn's answer. Responses already committed keep
                    // their calls and results; an interrupted proposal call
                    // stays as unanswered audit evidence.
                    self.retain_interrupted_proposals(turn).await;
                    turn.clear();
                } else {
                    self.commit_assistant_turn(turn, stop_reason).await;
                    turn.clear();
                    self.turn_complete.notify_waiters();
                }
                if *stop_reason != StopReason::ToolUse {
                    // The operator turn is over: its results have all arrived.
                    turn.committed.clear();
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
            StreamChunk::ToolUse { id, name, input } => {
                turn.text
                    .push_str(&format!("\n[tool use: {name} (id: {id})]\n"));
                turn.call(id, name, input);
                self.operator_turn_active.store(true, Ordering::Release);
            }
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
                diff,
                images,
                ..
            } => {
                turn.text.push_str(&format!("[tool result: {content}]\n"));
                self.attach_tool_result(turn, id, content, *is_error, diff, images)
                    .await;
                self.operator_turn_active.store(true, Ordering::Release);
            }
            _ => {}
        }
        false
    }

    /// Retain exact proposal evidence without folding its private arguments or
    /// result into the public prose transcript: the proposal call joins its
    /// own response, in emission order. Snapshots project these tools out; the
    /// stored conversation and model continuation remain complete.
    async fn retain_operator_private_chunk(
        &self,
        chunk: &StreamChunk,
        turn: &mut AssistantTurnBuffer,
    ) {
        match chunk {
            StreamChunk::ToolUse { id, name, input } => turn.call(id, name, input),
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
                diff,
                images,
                ..
            } => {
                self.attach_tool_result(turn, id, content, *is_error, diff, images)
                    .await;
            }
            _ => {}
        }
    }

    /// An interrupted response's proposal calls stay in the stored history as
    /// unanswered audit evidence (Story 19.21b), one standalone message each as
    /// before 20.1d4a. Unanswered calls are never sent to the model.
    async fn retain_interrupted_proposals(&self, turn: &mut AssistantTurnBuffer) {
        let mut proposals = turn
            .calls
            .extract_if(.., |call| {
                call.name == crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME
            })
            .peekable();
        if proposals.peek().is_none() {
            return;
        }
        let mut conversation = self.conversation.lock().await;
        let now = crate::domain::models::session_meta::now_unix();
        conversation
            .messages
            .extend(proposals.map(|call| ChatMessage {
                id: generate_message_id(),
                role: MessageRole::Assistant,
                tool_calls: vec![call],
                created_at: now,
                ..Default::default()
            }));
        conversation.updated_at = now;
        if let Err(error) = self.core.storage.save_conversation(&conversation).await {
            tracing::warn!(%error, "persisting interrupted proposal evidence failed");
        }
    }

    /// Attach a result to the latest unanswered call with that provider id: in
    /// the open response, else in a message this operator turn committed, which
    /// is saved at once so a restart never separates them. A result that
    /// matches no such call is dropped, as before.
    async fn attach_tool_result(
        &self,
        turn: &mut AssistantTurnBuffer,
        id: &str,
        content: &str,
        is_error: bool,
        diff: &WriteDiffState,
        images: &[crate::domain::models::ImageAttachment],
    ) {
        let result = ToolResultInfo {
            content: content.to_owned(),
            is_error,
            images: images.to_vec(),
            diff: diff.clone(),
        };
        let unanswered = |call: &&mut ToolCallInfo| call.wire_id() == id && call.result.is_none();
        if let Some(call) = turn.calls.iter_mut().rev().find(unanswered) {
            call.result = Some(result);
            return;
        }
        let mut conversation = self.conversation.lock().await;
        let Some(call) = conversation
            .messages
            .iter_mut()
            .rev()
            .filter(|message| turn.committed.contains(&message.id))
            .flat_map(|message| message.tool_calls.iter_mut().rev())
            .find(unanswered)
        else {
            return;
        };
        call.result = Some(result);
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
        if let Err(error) = self.core.storage.save_conversation(&conversation).await {
            tracing::warn!(%error, "persisting a tool result failed");
        }
    }

    /// Append the open response as one assistant message (origin-tagged) and
    /// persist it (AC4). Replay items become operator-private content blocks,
    /// so the next turn's `message_builder` replays them, also after a daemon
    /// restart. The channel response stays the first non-empty prose fold.
    async fn commit_assistant_turn(
        &self,
        turn: &mut AssistantTurnBuffer,
        stop_reason: &StopReason,
    ) {
        if !(turn.span.is_empty() && turn.calls.is_empty() && turn.replay.is_empty()) {
            let span = turn.span.finish();
            let mut content_blocks = Vec::with_capacity(turn.replay.len() + 2);
            if let Some(reasoning) = &span.reasoning {
                content_blocks.push(ContentBlockType::Thinking(reasoning.clone()));
            }
            content_blocks.extend(turn.replay.drain(..).map(ContentBlockType::ProviderReplay));
            let content = span.text.clone();
            content_blocks.push(ContentBlockType::ResponseSpan(span));
            let id = generate_message_id();
            turn.committed.push(id.clone());
            let origin = *self.active_channel_origin.lock().await;
            let mut conv = self.conversation.lock().await;
            conv.messages.push(ChatMessage {
                id,
                role: MessageRole::Assistant,
                content,
                content_blocks,
                tool_calls: std::mem::take(&mut turn.calls),
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
        }
        if turn.text.is_empty() {
            return;
        }
        if let Some(tx) = self.pending_channel_response_tx.lock().await.take() {
            let _ = tx.send(turn.text.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::mock_core;
    use super::*;
    use crate::domain::models::{ImageAttachment, ResponseSpan, team_proposal};

    fn server(workspace: &std::path::Path) -> (Arc<AttachServer>, Arc<dyn StoragePort>) {
        let (core, storage) = mock_core(workspace, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "buffer".into(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = tokio::sync::mpsc::unbounded_channel();
        (AttachServer::new(core, conversation, domain_tx), storage)
    }

    fn text(content: &str) -> StreamChunk {
        StreamChunk::Text {
            content: content.into(),
            parent_tool_use_id: None,
        }
    }

    fn tool_use(id: &str, name: &str) -> StreamChunk {
        StreamChunk::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({"id": id}),
        }
    }

    fn tool_result(id: &str, content: &str, images: Vec<ImageAttachment>) -> StreamChunk {
        StreamChunk::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error: false,
            operator_private: false,
            diff: WriteDiffState::NotAWrite,
            images,
        }
    }

    fn complete(stop_reason: StopReason) -> StreamChunk {
        StreamChunk::TurnComplete { stop_reason }
    }

    fn replay(marker: &str) -> StreamChunk {
        StreamChunk::ProviderReplay {
            item: ProviderReplayItem {
                provider: "p".into(),
                model: "m".into(),
                payload: Arc::new(serde_json::json!({ "m": marker })),
                scope: Some("s".into()),
            },
        }
    }

    async fn fold(server: &AttachServer, turn: &mut AssistantTurnBuffer, chunks: Vec<StreamChunk>) {
        for chunk in chunks {
            server.fold_operator_chunk(&chunk, turn).await;
        }
    }

    fn spans(message: &ChatMessage) -> Vec<&ResponseSpan> {
        message
            .content_blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlockType::ResponseSpan(span) => Some(span),
                _ => None,
            })
            .collect()
    }

    fn results(message: &ChatMessage) -> Vec<Option<String>> {
        message
            .tool_calls
            .iter()
            .map(|call| call.result.as_ref().map(|r| r.content.clone()))
            .collect()
    }

    /// Story 20.1d4a T2: one message per response, calls by provider id, the
    /// results saved when they arrive, an empty-text response kept, and the
    /// channel reply equal to the first response's prose fold.
    #[tokio::test]
    async fn each_response_commits_its_own_message_and_results_are_saved_on_arrival() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, storage) = server(tmp.path());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        *server.pending_channel_response_tx.lock().await = Some(reply_tx);
        let image = ImageAttachment {
            media_type: "image/png".into(),
            data: "UE5H".into(),
        };
        let mut turn = AssistantTurnBuffer::default();
        fold(
            &server,
            &mut turn,
            vec![
                StreamChunk::Thinking {
                    content: "T".into(),
                    parent_tool_use_id: None,
                },
                text("V"),
                tool_use("A", "img"),
                tool_use("B", "echo"),
                replay("r1"),
                complete(StopReason::ToolUse),
                tool_result("B", "b", vec![]),
                tool_result("A", "a", vec![image.clone()]),
            ],
        )
        .await;
        assert_eq!(
            reply_rx.await.unwrap(),
            "V\n[tool use: img (id: A)]\n\n[tool use: echo (id: B)]\n"
        );
        let saved = storage.load_conversation("buffer").await.unwrap().unwrap();
        assert_eq!(saved.messages.len(), 1);
        assert_eq!(
            results(&saved.messages[0]),
            vec![Some("a".into()), Some("b".into())],
            "results attached and saved as they arrived, before the next commit"
        );

        fold(
            &server,
            &mut turn,
            vec![
                tool_use("C", "echo"),
                complete(StopReason::ToolUse),
                tool_result("C", "c", vec![]),
                text("done"),
                complete(StopReason::EndTurn),
            ],
        )
        .await;
        assert!(turn.is_empty());
        let conversation = server.conversation.lock().await;
        let rows = &conversation.messages;
        assert_eq!(rows.len(), 3, "one message per response");
        assert_eq!(rows[0].content, "V");
        assert_eq!(
            rows[0].content_blocks[0],
            ContentBlockType::Thinking("T".into())
        );
        assert_eq!(
            spans(&rows[0]),
            vec![&ResponseSpan {
                text: "V".into(),
                reasoning: Some("T".into()),
                tool_calls: 2,
                replay: 1,
            }]
        );
        let ids: Vec<_> = rows[0].tool_calls.iter().map(|c| c.wire_id()).collect();
        assert_eq!(ids, ["A", "B"]);
        assert_eq!(
            rows[0].tool_calls[0].result.as_ref().unwrap().images,
            vec![image]
        );
        assert_eq!(rows[1].content, "", "an empty-text response is kept");
        assert_eq!(spans(&rows[1])[0].tool_calls, 1);
        assert_eq!(results(&rows[1]), vec![Some("c".into())]);
        assert_eq!(rows[2].content, "done");
        assert!(rows.iter().all(|row| !row.content.contains("[tool")));
    }

    /// `Cancelled` discards only the open response; committed responses keep
    /// their calls and results.
    #[tokio::test]
    async fn cancel_discards_only_the_open_response() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _storage) = server(tmp.path());
        let mut turn = AssistantTurnBuffer::default();
        fold(
            &server,
            &mut turn,
            vec![
                tool_use("A", "echo"),
                complete(StopReason::ToolUse),
                tool_result("A", "a", vec![]),
                text("partial"),
                tool_use("B", "echo"),
                complete(StopReason::Cancelled),
            ],
        )
        .await;
        assert!(turn.is_empty());
        let conversation = server.conversation.lock().await;
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(results(&conversation.messages[0]), vec![Some("a".into())]);
    }

    /// An interrupted response's proposal call stays as unanswered audit
    /// evidence (19.21b); its other calls and text are discarded.
    #[tokio::test]
    async fn cancel_keeps_an_interrupted_proposal_call_unanswered() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _storage) = server(tmp.path());
        let mut turn = AssistantTurnBuffer::default();
        fold(
            &server,
            &mut turn,
            vec![
                text("partial"),
                tool_use("A", "echo"),
                tool_use("P", team_proposal::TEAM_PROPOSAL_TOOL_NAME),
                complete(StopReason::Cancelled),
            ],
        )
        .await;
        assert!(turn.is_empty());
        let conversation = server.conversation.lock().await;
        assert_eq!(conversation.messages.len(), 1);
        let row = &conversation.messages[0];
        let ids: Vec<_> = row.tool_calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["P"]);
        assert_eq!(results(row), vec![None]);
        assert!(row.content.is_empty() && spans(row).is_empty());
    }

    /// A recycled provider id attaches each result to its own response; a
    /// result that matches no call is dropped, as before.
    #[tokio::test]
    async fn a_recycled_id_attaches_per_response_and_an_unmatched_result_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _storage) = server(tmp.path());
        let mut turn = AssistantTurnBuffer::default();
        fold(
            &server,
            &mut turn,
            vec![
                tool_use("X", "echo"),
                complete(StopReason::ToolUse),
                tool_result("X", "first", vec![]),
                tool_use("X", "echo"),
                complete(StopReason::ToolUse),
                tool_result("X", "second", vec![]),
                tool_result("nobody", "dropped", vec![]),
                complete(StopReason::EndTurn),
            ],
        )
        .await;
        let conversation = server.conversation.lock().await;
        let rows = &conversation.messages;
        assert_eq!(rows.len(), 2);
        assert_eq!(results(&rows[0]), vec![Some("first".into())]);
        assert_eq!(results(&rows[1]), vec![Some("second".into())]);
        assert!(
            rows.iter()
                .all(|row| !serde_json::to_string(row).unwrap().contains("dropped"))
        );
    }

    /// Review RF-03: a result attaches only to a call this operator turn
    /// committed. An earlier turn's unanswered call and a peer display row
    /// appended while the tool ran keep no result, even with the same id.
    #[tokio::test]
    async fn a_result_attaches_only_to_this_turns_committed_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _storage) = server(tmp.path());
        let mut turn = AssistantTurnBuffer::default();
        fold(
            &server,
            &mut turn,
            vec![
                tool_use("X", "echo"),
                tool_use("Y", "echo"),
                complete(StopReason::ToolUse),
                complete(StopReason::EndTurn),
                tool_use("X", "echo"),
                complete(StopReason::ToolUse),
            ],
        )
        .await;
        server.conversation.lock().await.messages.push(ChatMessage {
            id: "peer-row".into(),
            role: MessageRole::Assistant,
            tool_calls: vec![ToolCallInfo {
                id: "X".into(),
                name: "echo".into(),
                input: serde_json::json!({}),
                result: None,
                started_at_ms: None,
                completed_at_ms: None,
                status: None,
                provider_id: None,
            }],
            history_source: crate::domain::models::HistorySource::PeerDisplay,
            ..Default::default()
        });
        fold(
            &server,
            &mut turn,
            vec![
                tool_result("X", "mine", vec![]),
                tool_result("Y", "stray", vec![]),
                complete(StopReason::EndTurn),
            ],
        )
        .await;
        assert!(turn.is_empty());
        let conversation = server.conversation.lock().await;
        let rows = &conversation.messages;
        assert_eq!(rows.len(), 3);
        assert_eq!(results(&rows[0]), vec![None, None], "an earlier turn");
        assert_eq!(results(&rows[1]), vec![Some("mine".into())]);
        assert_eq!(results(&rows[2]), vec![None], "a peer row");
    }

    /// The proposal call is never fanned out, joins its own response in
    /// emission order, and every transcript projection drops it.
    #[tokio::test]
    async fn the_proposal_call_joins_its_own_response_and_stays_private() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _storage) = server(tmp.path());
        let mut turn = AssistantTurnBuffer::default();
        let proposal = tool_use("P", team_proposal::TEAM_PROPOSAL_TOOL_NAME);
        assert!(
            !server
                .fold_operator_chunk(&tool_use("A", "echo"), &mut turn)
                .await
        );
        assert!(
            server.fold_operator_chunk(&proposal, &mut turn).await,
            "never fanned out"
        );
        let mut private = tool_result("P", "proposal shown", vec![]);
        if let StreamChunk::ToolResult {
            operator_private, ..
        } = &mut private
        {
            *operator_private = true;
        }
        fold(
            &server,
            &mut turn,
            vec![
                complete(StopReason::ToolUse),
                tool_result("A", "a", vec![]),
                private.clone(),
            ],
        )
        .await;
        assert!(server.fold_operator_chunk(&private, &mut turn).await);
        let conversation = server.conversation.lock().await;
        let row = &conversation.messages[0];
        let names: Vec<_> = row.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["echo", team_proposal::TEAM_PROPOSAL_TOOL_NAME]);
        assert_eq!(spans(row)[0].tool_calls, 2);
        assert_eq!(results(row)[1].as_deref(), Some("proposal shown"));
        assert!(
            !row.content.contains("proposal"),
            "private calls never fold into prose"
        );
        let public = crate::infrastructure::runtime::event_bus::public_transcript_message(row);
        assert_eq!(public.tool_calls.len(), 1);
        assert!(
            !serde_json::to_string(&public)
                .unwrap()
                .contains("proposal shown")
        );
    }
}
