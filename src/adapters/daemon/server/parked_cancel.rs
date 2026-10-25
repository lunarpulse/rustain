//! Durable cancellation settlement for process-owned parked inbound responses.
use super::*;
use crate::adapters::daemon::response_modes::{self, DraftPhase, PendingDraftController};

pub(super) struct ParkedResponseContext {
    pub(super) conversation: Arc<Mutex<Conversation>>,
    pub(super) registry: Arc<Mutex<ConnRegistry>>,
    pub(super) storage: Arc<dyn StoragePort>,
    pub(super) node_tree: crate::infrastructure::subagent::NodeTree,
    pub(super) pending_drafts: Arc<PendingDraftController>,
    pub(super) peer_reply_serial: Arc<Mutex<()>>,
    pub(super) peer_row_serial: Arc<Mutex<()>>,
}

impl AttachServer {
    pub(super) fn parked_response_context(&self) -> ParkedResponseContext {
        ParkedResponseContext {
            conversation: self.conversation.clone(),
            registry: self.registry.clone(),
            storage: self.core.storage.clone(),
            node_tree: self.node_tree.clone(),
            pending_drafts: self.pending_drafts.clone(),
            peer_reply_serial: self.peer_reply_serial.clone(),
            peer_row_serial: self.peer_row_serial.clone(),
        }
    }
}

impl ParkedResponseContext {
    pub(super) fn spawn(self, node: AgentId, cancel: CancellationToken) {
        tokio::spawn(self.watch_parked_response(node, cancel));
    }

    async fn watch_parked_response(self, node: AgentId, cancel: CancellationToken) {
        let mut status = self.node_tree.status_rx(&node).await;
        let terminal_or_closed = async {
            if let Some(status) = status.as_mut() {
                loop {
                    if status.borrow().is_terminal() {
                        break;
                    }
                    if status.changed().await.is_err() {
                        break;
                    }
                }
            }
        };
        tokio::select! {
            () = cancel.cancelled() => {}
            () = terminal_or_closed => {}
        }
        if let Err(error) = self.settle(&node).await {
            tracing::error!(%error, %node, "persisting parked peer cancellation failed; response remains answerable");
        }
    }

    async fn settle(&self, node: &AgentId) -> Result<(), String> {
        // The controller phase is the linearization point shared with every
        // operator resolution, including status/retro and WriteOwn.
        let _reply_guard = self.peer_reply_serial.lock().await;
        let _row_guard = self.peer_row_serial.lock().await;
        if self.pending_drafts.phase(node.as_str()).await != Some(DraftPhase::Ready) {
            return Ok(());
        }
        let row_id = format!("peer-response-{node}");
        let pending = {
            let conversation = self.conversation.lock().await;
            conversation
                .messages
                .iter()
                .find(|row| row.id == row_id)
                .zip(conversation.peer_row_controls.get(&row_id))
                .filter(|(_, control)| {
                    control.node_id == node.as_str()
                        && matches!(
                            control.stage,
                            PeerRowStage::AwaitingOperator | PeerRowStage::Ready
                        )
                })
                .map(|(row, control)| (row.clone(), control.clone()))
        };
        let Some((mut row, mut control)) = pending else {
            return Ok(());
        };
        row.content = response_modes::PARKED_CANCELLED_NOTICE.to_owned();
        control.stage = PeerRowStage::Settled;
        control.node_resolvable = false;
        // We already hold peer_row_serial. The locking wrapper would deadlock.
        commit_peer_rows_serialized(
            &self.conversation,
            &self.registry,
            self.storage.as_ref(),
            vec![PeerRowMutation::Upsert {
                message: row,
                control: Some(control),
            }],
        )
        .await?;
        // Committed: the card is settled. Node cleanup can no longer fail the
        // settlement, and a lost race with cascade_kill must still deregister.
        self.pending_drafts.abandon(node.as_str()).await;
        let waiting = self
            .node_tree
            .status_rx(node)
            .await
            .is_some_and(|status| *status.borrow() == NodeState::Waiting);
        if waiting
            && let Err(error) = self
                .node_tree
                .try_set_state(node, NodeState::Cancelled)
                .await
        {
            tracing::warn!(%error, %node, "settled parked peer node did not move to Cancelled");
        }
        self.node_tree.deregister(node).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::NoOpMemory;
    use crate::domain::errors::StorageError;

    struct RefusingSaves(Arc<dyn StoragePort>);

    #[async_trait::async_trait]
    impl StoragePort for RefusingSaves {
        async fn read_snapshot(
            &self,
            _conversation_id: &str,
            _checkpoint: crate::domain::models::checkpoint::CheckpointId,
            _path: &std::path::Path,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(None)
        }
        async fn save_conversation(
            &self,
            _conversation: &Conversation,
        ) -> Result<(), StorageError> {
            Err(StorageError::IoError("disk full".into()))
        }
        async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
            self.0.load_conversation(id).await
        }
        async fn list_conversations(
            &self,
        ) -> Result<Vec<crate::domain::models::ConversationSummary>, StorageError> {
            self.0.list_conversations().await
        }
    }

    #[tokio::test]
    async fn refused_save_keeps_ready_card_and_waiting_node_answerable() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(dir.path()),
            dir.path().to_path_buf(),
        ));
        let node = AgentId::from_validated("parked-cancel-refused");
        let row_id = format!("peer-response-{node}");
        let mut persisted = Conversation {
            id: "parked-cancel-refused".into(),
            ..Default::default()
        };
        persisted.messages.push(ChatMessage {
            id: row_id.clone(),
            role: MessageRole::Assistant,
            synthetic: true,
            content: response_modes::AWAITING_RESPONSE_PLACEHOLDER.into(),
            ..Default::default()
        });
        persisted.peer_row_controls.insert(
            row_id.clone(),
            peer_row_control(
                &node,
                None,
                None,
                None,
                &Default::default(),
                "peer",
                PeerRowStage::AwaitingOperator,
                true,
            ),
        );
        inner.save_conversation(&persisted).await.unwrap();
        let (core, _) = mock_core_with_storage(
            dir.path(),
            vec![],
            Arc::new(NoOpMemory),
            Arc::new(RefusingSaves(inner.clone())),
        );
        let conversation = Arc::new(Mutex::new(persisted.clone()));
        let (domain_tx, _) = mpsc::unbounded_channel();
        let server = journaled_server(
            core,
            conversation.clone(),
            domain_tx,
            Arc::new(RecordingJournal::default()),
        );
        let (commands, _receiver) = mpsc::channel(1);
        let (status, _) = tokio::sync::watch::channel(NodeState::Created);
        let (_, metrics) =
            tokio::sync::watch::channel(crate::domain::models::AgentMetrics::default());
        server
            .node_tree
            .register_peer(
                node.clone(),
                crate::infrastructure::subagent::AgentHandle {
                    agent_id: node.clone(),
                    token: crate::domain::models::CapabilityTokenId::nil(),
                    command_tx: commands,
                    cancel_token: CancellationToken::new(),
                    depth: 0,
                    subagent_type: "a2a-inbound".into(),
                    spawned_at: 0,
                    status,
                    metrics,
                    isolated: false,
                    mailbox_budget: crate::infrastructure::subagent::MailboxBudget::new(),
                },
            )
            .await
            .unwrap();
        server
            .node_tree
            .try_set_state(&node, NodeState::Running)
            .await
            .unwrap();
        server
            .node_tree
            .try_set_state(&node, NodeState::Waiting)
            .await
            .unwrap();
        assert!(server.pending_drafts.begin(node.as_str()).await);
        assert!(server.pending_drafts.complete(node.as_str(), "").await);
        let error = server
            .parked_response_context()
            .settle(&node)
            .await
            .unwrap_err();
        assert!(error.contains("disk full"), "{error}");
        assert_eq!(
            server.pending_drafts.phase(node.as_str()).await,
            Some(DraftPhase::Ready)
        );
        assert_eq!(
            *server.node_tree.status_rx(&node).await.unwrap().borrow(),
            NodeState::Waiting
        );
        let live = conversation.lock().await;
        assert_eq!(
            live.messages[0].content,
            response_modes::AWAITING_RESPONSE_PLACEHOLDER
        );
        assert_eq!(
            live.peer_row_controls[&row_id].stage,
            PeerRowStage::AwaitingOperator
        );
        assert!(live.peer_row_controls[&row_id].node_resolvable);
        assert_eq!(live.peer_row_revision, persisted.peer_row_revision);
        drop(live);
        let durable = inner
            .load_conversation(&persisted.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.messages[0].content, persisted.messages[0].content);
        assert_eq!(durable.peer_row_controls, persisted.peer_row_controls);
    }
}
