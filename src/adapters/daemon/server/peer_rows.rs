use super::{AttachServer, PeerRowMutation, commit_peer_rows};
use crate::adapters::daemon::response_modes::{
    DRAFT_INTERRUPTED_NOTICE, DraftPhase, PendingDraftController, rap_one_way_notice,
};
use crate::domain::models::{
    AgentId, Conversation, InteractionPolicySnapshot, ItemAddress, PeerId, PeerRowControl,
    PeerRowStage, ResponseMode, StorySummary,
};
#[cfg(feature = "a2a")]
use crate::domain::models::{SemanticMessageType, story_summary};

pub(super) fn peer_row_control(
    node_id: &AgentId,
    item_address: Option<ItemAddress>,
    stories: Option<StorySummary>,
    provenance: &InteractionPolicySnapshot,
    fallback_label: &str,
    stage: PeerRowStage,
    node_resolvable: bool,
) -> PeerRowControl {
    PeerRowControl {
        node_id: node_id.as_str().to_owned(),
        item_address,
        stories,
        message_type: provenance.message_type,
        sender_label: provenance
            .sender_label
            .clone()
            .unwrap_or_else(|| fallback_label.to_owned()),
        stage,
        node_resolvable,
    }
}

pub(super) struct PeerItemContext {
    pub(super) address: ItemAddress,
    pub(super) alias: Option<String>,
    pub(super) stories: Option<StorySummary>,
}

pub(super) async fn snapshot_peer_row_controls(
    conversation: &Conversation,
    pending_drafts: &PendingDraftController,
) -> std::collections::BTreeMap<String, PeerRowControl> {
    if conversation.peer_row_controls.is_empty() {
        return std::collections::BTreeMap::new();
    }
    let mut controls = std::collections::BTreeMap::new();
    for message in &conversation.messages {
        let Some(stored) = conversation.peer_row_controls.get(&message.id) else {
            continue;
        };
        let mut control = stored.clone();
        let phase = pending_drafts.phase(&control.node_id).await;
        control.node_resolvable = match (control.stage, phase) {
            (PeerRowStage::AwaitingOperator | PeerRowStage::Ready, Some(DraftPhase::Ready))
            | (PeerRowStage::Drafting, Some(DraftPhase::Drafting)) => true,
            (PeerRowStage::AwaitingOperator | PeerRowStage::Ready, None) => {
                control.item_address.is_none()
            }
            _ => false,
        };
        controls.insert(message.id.clone(), control);
    }
    controls
}

pub(super) fn rap_rooted_peer(node: &AgentId) -> Option<PeerId> {
    PeerId::parse(node.as_str().split('/').next()?).ok()
}

impl AttachServer {
    pub(super) async fn item_context_for_node(&self, node_id: &AgentId) -> Option<PeerItemContext> {
        #[cfg(feature = "a2a")]
        {
            let reader = self.room_journal_reader.as_ref()?;
            let entries = match reader.load_entries().await {
                Ok(entries) => entries,
                Err(error) => {
                    tracing::warn!(%error, node = %node_id, "loading journal for peer-row target failed");
                    return None;
                }
            };
            let projection =
                crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
            crate::adapters::a2a::exec::recipient_item_for_node(&projection, node_id).map(|item| {
                PeerItemContext {
                    stories: (item.message_type == SemanticMessageType::StoryAssignment)
                        .then(|| {
                            item.stories
                                .as_ref()
                                .map(|list| StorySummary(story_summary(list)))
                        })
                        .flatten(),
                    address: item.address,
                    alias: item.alias,
                }
            })
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = node_id;
            None
        }
    }
    /// Story 19.20: process-owned drafts cannot resume after daemon restart.
    pub(super) async fn settle_interrupted_drafts(&self) -> Result<(), String> {
        let candidates = {
            let conversation = self.conversation.lock().await;
            conversation
                .messages
                .iter()
                .filter_map(|message| {
                    let control = conversation.peer_row_controls.get(&message.id)?;
                    (control.stage == PeerRowStage::Drafting)
                        .then(|| (message.clone(), control.clone()))
                })
                .collect::<Vec<_>>()
        };
        let mut mutations = Vec::with_capacity(candidates.len());
        for (mut message, mut control) in candidates {
            if self.pending_drafts.phase(&control.node_id).await.is_some() {
                continue;
            }
            let node = AgentId::from_validated(&control.node_id);
            message.content = if rap_rooted_peer(&node).is_some() {
                rap_one_way_notice(ResponseMode::NotifyAndDraft)
            } else {
                DRAFT_INTERRUPTED_NOTICE.to_owned()
            };
            control.stage = PeerRowStage::Settled;
            control.node_resolvable = false;
            mutations.push(PeerRowMutation::Upsert {
                message,
                control: Some(control),
            });
        }
        if mutations.is_empty() {
            return Ok(());
        }
        commit_peer_rows(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            mutations,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        peer_row_control,
        tests::{
            RecordingJournal, attach_with_proof, journaled_server, mock_core,
            mock_core_with_storage, test_signer,
        },
    };
    use super::*;
    use crate::adapters::daemon::protocol::{
        AttachMode, ClientFrame, ConnectionTier, DaemonFrame, read_frame, write_frame,
    };
    use crate::domain::models::{
        Authorship, ChatMessage, ItemAddress, ItemId, MessageRole, RoomEvent, SendAuthorization,
    };
    use crate::domain::ports::RoomJournal;
    use std::sync::Arc;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;
    /// Story 19.20 AC6: restart settles only controls without a process-owned phase.
    #[tokio::test]
    async fn story19_20_interrupted_draft_settles_on_real_run() {
        use crate::adapters::daemon::protocol::{RecipientAction, RecipientActionOutcome};
        use crate::adapters::daemon::response_modes::DRAFTING_PLACEHOLDER;
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            for case in ["a2a-item", "itemless", "rap", "live-phase", "awaiting"] {
                let tmp = tempfile::tempdir().unwrap();
                let (core, storage) = mock_core(tmp.path(), vec![]);
                let peer = PeerId::from_public_key(&[95; 32]).unwrap();
                let node = AgentId::from_validated(if case == "rap" {
                    format!("{peer}/draft")
                } else {
                    format!("interrupted-{case}")
                });
                let row_id = format!("peer-response-{node}");
                let linked = case == "a2a-item";
                let address = ItemAddress::from_a2a_ingress(peer, ItemId::from_replay("ri_restart"));
                let stage = if case == "awaiting" {
                    PeerRowStage::AwaitingOperator
                } else {
                    PeerRowStage::Drafting
                };
                let mut persisted = Conversation {
                    id: format!("story19-20-{case}"),
                    ..Default::default()
                };
                persisted.messages.push(ChatMessage {
                    id: row_id.clone(),
                    role: MessageRole::Assistant,
                    synthetic: true,
                    content: DRAFTING_PLACEHOLDER.into(),
                    ..Default::default()
                });
                persisted.peer_row_controls.insert(row_id.clone(), peer_row_control(
                    &node, linked.then(|| address.clone()), None, &Default::default(),
                    "remote", stage, true,
                ));
                storage.save_conversation(&persisted).await.unwrap();
                let conversation = Arc::new(Mutex::new(
                    storage.load_conversation(&persisted.id).await.unwrap().unwrap(),
                ));
                let (domain_tx, _) = mpsc::unbounded_channel();
                let journal = Arc::new(RecordingJournal::default());
                if linked {
                    journal.record_event(RoomEvent::RecipientItemReceived { stories: None, address: address.clone(),
                    task: "restart-task".into(),
                    alias: None,
                    message_type: crate::domain::models::SemanticMessageType::BugReport,
                    authorship: Authorship::Unknown,
                    send_authorization: SendAuthorization::Unknown,
                    content: "bug report".into(),
                    in_reply_to: None,
                    capability_hash: None,
                    reply_capability_matched: false, }).await.unwrap();
                }
                let base_events = journal.events.lock().await.len();
                let server = journaled_server(core, conversation, domain_tx, journal.clone());
                if case == "live-phase" {
                    assert!(server.pending_drafts.begin(node.as_str()).await);
                }
                let should_settle = !matches!(case, "live-phase" | "awaiting");
                let mut first_revision = None;
                for boot in 0..2 {
                    let socket = tmp.path().join(format!("restart-{boot}.sock"));
                    let listener = UnixListener::bind(&socket).unwrap();
                    let shutdown = CancellationToken::new();
                    let (_, domain_rx) = mpsc::unbounded_channel();
                    let service = {
                        let server = server.clone();
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            server.run(listener, domain_rx, None, None, shutdown).await;
                        })
                    };
                    let mut client = UnixStream::connect(&socket).await.unwrap();
                    let ack = attach_with_proof(
                        &mut client, false, ConnectionTier::TrustedLocal, &test_signer(95),
                    ).await;
                    assert!(matches!(ack, Some(DaemonFrame::AttachAck {
                        granted_mode: AttachMode::ReadWrite, ..
                    })));
                    let reloaded = storage.load_conversation(&persisted.id).await.unwrap().unwrap();
                    let control = &reloaded.peer_row_controls[&row_id];
                    assert_eq!(control.stage, if should_settle { PeerRowStage::Settled } else { stage }, "{case}");
                    assert_eq!(control.node_resolvable, !should_settle, "{case}");
                    let expected = if !should_settle {
                        DRAFTING_PLACEHOLDER.to_owned()
                    } else if case == "rap" {
                        rap_one_way_notice(ResponseMode::NotifyAndDraft)
                    } else {
                        DRAFT_INTERRUPTED_NOTICE.to_owned()
                    };
                    assert_eq!(reloaded.messages.len(), 1, "{case}");
                    assert_eq!(reloaded.messages[0].content, expected, "{case}");
                    if let Some(revision) = first_revision {
                        assert_eq!(reloaded.peer_row_revision, revision, "second run is idempotent");
                    }
                    first_revision = Some(reloaded.peer_row_revision);
                    assert_eq!(journal.events.lock().await.len(), base_events, "empty restart journal delta");
                    assert!(server.inbound_results.lock().await.is_empty());
                    assert_eq!(server.pending_drafts.phase(node.as_str()).await.is_some(), case == "live-phase");
                    if linked && boot == 1 {
                        write_frame(&mut client, &ClientFrame::ReplyToRecipientItem {
                            item_id: "ri_restart".into(), text: "Direct reply after restart".into(),
                        }).await.unwrap();
                        loop {
                            if let Some(DaemonFrame::RecipientActionResult {
                                action: RecipientAction::Reply, outcome, ..
                            }) = read_frame::<_, DaemonFrame>(&mut client).await.unwrap() {
                                if cfg!(feature = "a2a") {
                                    assert!(matches!(outcome, RecipientActionOutcome::Applied { reply: Some(_) }));
                                } else {
                                    assert!(matches!(outcome, RecipientActionOutcome::Refused {
                                        error: crate::adapters::daemon::protocol::ProtocolError::Malformed(detail)
                                    } if detail.contains("needs the `a2a` feature")));
                                }
                                break;
                            }
                        }
                        let events = journal.events.lock().await;
                        assert_eq!(events.len(), base_events + usize::from(cfg!(feature = "a2a")));
                        if cfg!(feature = "a2a") {
                            assert!(matches!(&events[base_events], RoomEvent::RecipientItemReplied { content, .. }
                                if content == "Direct reply after restart"));
                        }
                    }
                    // This fixture reuses the server across boots. Observe connection
                    // cleanup before restarting, so its old writer slot cannot race
                    // the next boot's ReadWrite attach.
                    write_frame(&mut client, &ClientFrame::Detach).await.unwrap();
                    while read_frame::<_, DaemonFrame>(&mut client).await.unwrap().is_some() {}
                    shutdown.cancel();
                    drop(client);
                    service.await.unwrap();
                }
            }
        }).await.expect("interrupted draft restart cases complete");
    }

    /// Story 19.20 review: a refused settlement commit rolls back and the
    /// daemon still serves its attach socket (the row stays unsettled).
    #[tokio::test]
    async fn story19_20_failed_settlement_still_serves_attach() {
        use crate::adapters::daemon::response_modes::DRAFTING_PLACEHOLDER;
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::adapters::noop::NoOpMemory;
        use crate::domain::errors::StorageError;
        use crate::domain::ports::StoragePort;

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
            async fn save_conversation(&self, _conv: &Conversation) -> Result<(), StorageError> {
                Err(StorageError::IoError("disk full".into()))
            }
            async fn load_conversation(
                &self,
                id: &str,
            ) -> Result<Option<Conversation>, StorageError> {
                self.0.load_conversation(id).await
            }
            async fn list_conversations(
                &self,
            ) -> Result<Vec<crate::domain::models::ConversationSummary>, StorageError> {
                self.0.list_conversations().await
            }
        }

        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let tmp = tempfile::tempdir().unwrap();
            let inner: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tmp.path()),
                tmp.path().to_path_buf(),
            ));
            let node = AgentId::from_validated("interrupted-refused");
            let row_id = format!("peer-response-{node}");
            let mut persisted = Conversation {
                id: "story19-20-refused".into(),
                ..Default::default()
            };
            persisted.messages.push(ChatMessage {
                id: row_id.clone(),
                role: MessageRole::Assistant,
                synthetic: true,
                content: DRAFTING_PLACEHOLDER.into(),
                ..Default::default()
            });
            persisted.peer_row_controls.insert(
                row_id.clone(),
                peer_row_control(
                    &node,
                    None,
                    None,
                    &Default::default(),
                    "remote",
                    PeerRowStage::Drafting,
                    true,
                ),
            );
            inner.save_conversation(&persisted).await.unwrap();
            let before = inner
                .load_conversation(&persisted.id)
                .await
                .unwrap()
                .unwrap();
            let (core, _) = mock_core_with_storage(
                tmp.path(),
                vec![],
                Arc::new(NoOpMemory),
                Arc::new(RefusingSaves(inner.clone())),
            );
            let conversation = Arc::new(Mutex::new(before.clone()));
            let (domain_tx, _) = mpsc::unbounded_channel();
            let server = journaled_server(
                core,
                conversation.clone(),
                domain_tx,
                Arc::new(RecordingJournal::default()),
            );
            let socket = tmp.path().join("refused.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let shutdown = CancellationToken::new();
            let (_, domain_rx) = mpsc::unbounded_channel();
            let service = {
                let server = server.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    server.run(listener, domain_rx, None, None, shutdown).await;
                })
            };
            let mut client = UnixStream::connect(&socket).await.unwrap();
            let ack = attach_with_proof(
                &mut client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(96),
            )
            .await;
            assert!(
                matches!(
                    ack,
                    Some(DaemonFrame::AttachAck {
                        granted_mode: AttachMode::ReadWrite,
                        ..
                    })
                ),
                "a refused settlement still serves attach"
            );
            {
                let live = conversation.lock().await;
                assert_eq!(
                    live.peer_row_controls[&row_id].stage,
                    PeerRowStage::Drafting
                );
                assert_eq!(live.messages[0].content, DRAFTING_PLACEHOLDER);
                assert_eq!(live.peer_row_revision, before.peer_row_revision);
            }
            let reloaded = inner
                .load_conversation(&persisted.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                reloaded.peer_row_controls[&row_id].stage,
                PeerRowStage::Drafting
            );
            shutdown.cancel();
            drop(client);
            service.await.unwrap();
        })
        .await
        .expect("refused settlement case completes");
    }
}
