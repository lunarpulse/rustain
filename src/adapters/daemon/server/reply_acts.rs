//! Human-decided recipient reply acts and their durable turn boundary.

use super::{
    AttachMode, AttachServer, ConnectionTier, ProtocolError, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget,
};
#[cfg(feature = "a2a")]
use super::{
    PeerDraftResolutionOutcome, RECIPIENT_ITEM_REMOVED, RecipientReplyReceipt, commit_peer_rows,
};
use crate::domain::models::ReplyKind;

impl AttachServer {
    /// Story 19.18 `AC2` — the `/team reply` arm: record one reply turn
    /// answering a recipient item on THIS host. ⛔ No egress, no push, and no
    /// write on the answered item (`A2`, `A4`).
    ///
    /// Order: shared gate + load + find → legality (a tombstone is refused;
    /// a retracted item is answerable) → Story 19.22 verdict gates (`AC4a`: a
    /// verdict on an item without a story list, or an `Accept` on a list that
    /// assigns the operator no story, is refused; an empty `Accept` text
    /// normalizes to `Accepted.` before the empty-text refusal) → size →
    /// author-time scrub (`P13`) → (d′) a persisted pending card for this item
    /// is resolved as `WriteOwn` with the same text and kind (one act, one
    /// turn, the card clears) → otherwise one direct `RecipientItemReplied`
    /// carrying the kind. The confirmation frame is sent only after the turn
    /// is durable (`M25`); every refusal writes zero rows.
    pub(super) async fn reply_to_recipient_item(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: &str,
        text: String,
        verdict: Option<ReplyKind>,
    ) {
        let target = RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (mode, tier, &text, verdict);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Reply,
                target,
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(
                        "`/team reply` needs the `a2a` feature; this build was compiled without it."
                            .to_owned(),
                    ),
                },
            )
            .await;
        }
        #[cfg(feature = "a2a")]
        {
            use crate::domain::models::{
                Authorship, MAX_REPLY_BYTES, ReceivedStories, RecipientItemState, ReplyKind,
                RoomEvent, TurnAddress, VerdictOffer, claim_reply_turn_id,
                fresh_reply_turn_candidate,
            };

            let _reply_guard = self.peer_reply_serial.lock().await;
            let (item, projection, journal) =
                match self.find_recipient_item(mode, tier, "reply", item_id).await {
                    Ok(found) => found,
                    Err(error) => {
                        drop(_reply_guard);
                        self.send_recipient_action_result(
                            conn_id,
                            RecipientAction::Reply,
                            target,
                            RecipientActionOutcome::Refused { error },
                        )
                        .await;
                        return;
                    }
                };
            let refuse = |error| RecipientActionOutcome::Refused { error };
            if matches!(item.state, RecipientItemState::Removed { .. }) {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned())),
                )
                .await;
                return;
            }
            // Story 19.22 `AC4a` — the verdict gates: every refusal writes
            // zero rows. `verdict: None` reaches today's path byte-for-byte.
            if let Some(kind) = verdict {
                let offer = item
                    .stories
                    .as_ref()
                    .map_or(VerdictOffer::None, ReceivedStories::verdict_offer);
                let refusal = match (kind, offer) {
                    (_, VerdictOffer::None) => {
                        Some("a verdict answers an item that carries a story list")
                    }
                    (ReplyKind::Accept, VerdictOffer::DiscussOnly) => {
                        Some("accept answers an item that assigns you a story")
                    }
                    _ => None,
                };
                if let Some(message) = refusal {
                    drop(_reply_guard);
                    self.send_recipient_action_result(
                        conn_id,
                        RecipientAction::Reply,
                        target,
                        refuse(ProtocolError::Malformed(message.to_owned())),
                    )
                    .await;
                    return;
                }
            }
            // Story 19.22 `AC4b` — an empty Accept text is the `[y]` key path
            // and normalizes to the canonical statement BEFORE the empty-text
            // refusal; a Discuss still requires text.
            let text = match verdict {
                Some(ReplyKind::Accept) if text.trim().is_empty() => "Accepted.".to_owned(),
                _ => text,
            };
            if text.trim().is_empty() {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed("reply text is empty".to_owned())),
                )
                .await;
                return;
            }
            if text.len() > MAX_REPLY_BYTES {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed(format!(
                        "reply is longer than {MAX_REPLY_BYTES} bytes — shorten it"
                    ))),
                )
                .await;
                return;
            }
            let fragments =
                crate::domain::ports::InboundPeerRuntime::disclosure_forbidden_fragments(self)
                    .await;
            if crate::adapters::a2a::projection::withholds(&text, &self.core.workspace, &fragments)
            {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed(
                        "this reply names a host path, which peers are never shown — rephrase it"
                            .to_owned(),
                    )),
                )
                .await;
                return;
            }
            if let Some(node) = self.pending_card_for_item(&projection, &item).await {
                let outcome = match self
                    .resolve_peer_draft_locked(
                        &node,
                        crate::adapters::daemon::response_modes::DraftResolution::WriteOwn(
                            text,
                            verdict.unwrap_or(ReplyKind::Answer),
                        ),
                        Some(&projection),
                    )
                    .await
                {
                    Ok(PeerDraftResolutionOutcome::Applied { reply: Some(reply) }) => {
                        RecipientActionOutcome::Applied { reply: Some(reply) }
                    }
                    Ok(PeerDraftResolutionOutcome::Applied { reply: None }) => {
                        refuse(ProtocolError::Internal(
                            "recipient reply completed without a durable receipt".to_owned(),
                        ))
                    }
                    Ok(PeerDraftResolutionOutcome::Idempotent) => {
                        RecipientActionOutcome::Idempotent
                    }
                    Ok(PeerDraftResolutionOutcome::RefusedOneWay) => {
                        refuse(ProtocolError::PeerVerification(
                            crate::adapters::daemon::response_modes::RAP_ONE_WAY_REFUSAL.to_owned(),
                        ))
                    }
                    Err(error) => refuse(ProtocolError::Internal(error)),
                };
                drop(_reply_guard);
                self.send_recipient_action_result(conn_id, RecipientAction::Reply, target, outcome)
                    .await;
                return;
            }
            let turn = match claim_reply_turn_id(fresh_reply_turn_candidate(), |id| {
                projection.holds_reply_turn_id(id)
            }) {
                Ok(turn) => turn,
                Err(error) => {
                    drop(_reply_guard);
                    self.send_recipient_action_result(
                        conn_id,
                        RecipientAction::Reply,
                        target,
                        refuse(ProtocolError::Internal(error.to_string())),
                    )
                    .await;
                    return;
                }
            };
            let turn_id = turn.as_str().to_owned();
            let kind = verdict.unwrap_or(ReplyKind::Answer);
            let orphan_rows = self
                .orphan_response_mutations(&projection, &item, &text, Authorship::HumanWritten)
                .await;
            if let Err(error) = journal
                .record_event(RoomEvent::RecipientItemReplied {
                    address: TurnAddress::answering(&item.address, turn),
                    answers: item.address.item().clone(),
                    content: text,
                    kind,
                    authorship: Authorship::HumanWritten,
                    send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
                    via_node: None,
                    status: None,
                })
                .await
            {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Internal(error.to_string())),
                )
                .await;
                return;
            }
            if let Err(error) = commit_peer_rows(
                &self.peer_row_serial,
                &self.conversation,
                &self.registry,
                self.core.storage.as_ref(),
                orphan_rows,
            )
            .await
            {
                tracing::error!(error = %error, "durable recipient reply could not update its orphan card");
            }
            drop(_reply_guard);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Reply,
                target,
                RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: item_id.to_owned(),
                        turn_id,
                        kind,
                    }),
                },
            )
            .await;
        }
    }
    /// Record an independent local defer turn without touching the pending
    /// peer-response card or its controller. The normal item gate and journal
    /// projection remain the sole authority for addressing and state.
    pub(super) async fn defer_recipient_item(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: &str,
    ) {
        let target = RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (mode, tier);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Defer,
                target,
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(
                        "`/team defer` needs the `a2a` feature; this build was compiled without it."
                            .to_owned(),
                    ),
                },
            )
            .await;
        }
        #[cfg(feature = "a2a")]
        {
            use crate::domain::models::{RecipientItemState, ReplyKind};

            let _reply_guard = self.peer_reply_serial.lock().await;
            let (item, projection, journal) =
                match self.find_recipient_item(mode, tier, "defer", item_id).await {
                    Ok(found) => found,
                    Err(error) => {
                        drop(_reply_guard);
                        self.send_recipient_action_result(
                            conn_id,
                            RecipientAction::Defer,
                            target,
                            RecipientActionOutcome::Refused { error },
                        )
                        .await;
                        return;
                    }
                };
            let refuse = |error| RecipientActionOutcome::Refused { error };
            if matches!(item.state, RecipientItemState::Removed { .. }) {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Defer,
                    target,
                    refuse(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned())),
                )
                .await;
                return;
            }
            let turn = match crate::domain::models::claim_reply_turn_id(
                crate::domain::models::fresh_reply_turn_candidate(),
                |id| projection.holds_reply_turn_id(id),
            ) {
                Ok(turn) => turn,
                Err(error) => {
                    drop(_reply_guard);
                    self.send_recipient_action_result(
                        conn_id,
                        RecipientAction::Defer,
                        target,
                        refuse(ProtocolError::Internal(error.to_string())),
                    )
                    .await;
                    return;
                }
            };
            let turn_id = turn.as_str().to_owned();
            if let Err(error) = journal
                .record_event(crate::domain::models::RoomEvent::RecipientItemReplied {
                    address: crate::domain::models::TurnAddress::answering(&item.address, turn),
                    answers: item.address.item().clone(),
                    content: "Busy — defer 30m".to_owned(),
                    kind: ReplyKind::Defer { minutes: 30 },
                    authorship: crate::domain::models::Authorship::HumanWritten,
                    send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
                    via_node: None,
                    status: None,
                })
                .await
            {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Defer,
                    target,
                    refuse(ProtocolError::Internal(error.to_string())),
                )
                .await;
                return;
            }
            drop(_reply_guard);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Defer,
                target,
                RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: item_id.to_owned(),
                        turn_id,
                        kind: ReplyKind::Defer { minutes: 30 },
                    }),
                },
            )
            .await;
        }
    }

    /// Share exactly the arrival-time preview, or the canonical empty statement
    /// for Decline. All eligibility refusals precede any durable mutation.
    pub(super) async fn share_recipient_status(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: &str,
        decline: bool,
    ) {
        let target = RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        #[cfg(not(feature = "a2a"))]
        let result = {
            let _ = decline;
            super::recipient_command_gate(mode, tier, "reply", true).and_then(|()| {
                Err(ProtocolError::Malformed(
                    "status sharing needs the `a2a` feature; this build was compiled without it."
                        .to_owned(),
                ))
            })
        };
        #[cfg(feature = "a2a")]
        let result = {
            use crate::domain::models::{
                Authorship, RecipientItemState, ReplyKind, RoomEvent, SemanticMessageType,
                SendAuthorization, StatusStatement, TurnAddress, claim_reply_turn_id,
                fresh_reply_turn_candidate,
            };

            let _reply_guard = self.peer_reply_serial.lock().await;
            async {
                let (item, projection, journal) =
                    self.find_recipient_item(mode, tier, "reply", item_id).await?;
                if matches!(item.state, RecipientItemState::Removed { .. }) {
                    return Err(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned()));
                }
                if item.message_type != SemanticMessageType::StatusRequest {
                    return Err(ProtocolError::Malformed(
                        "status sharing answers status_request items only".to_owned(),
                    ));
                }
                let statement = {
                    let conversation = self.conversation.lock().await;
                    conversation.messages.iter().find_map(|message| {
                        let control = conversation.peer_row_controls.get(&message.id)?;
                        if control.item_address.as_ref() != Some(&item.address) {
                            return None;
                        }
                        control.status.as_ref().map(|preview| {
                            if decline {
                                StatusStatement::nothing()
                            } else {
                                preview.statement.clone()
                            }
                        })
                    })
                }
                .ok_or_else(|| {
                    ProtocolError::Malformed(
                        "no status statement was prepared for this item".to_owned(),
                    )
                })?;

                // A durable turn left by a failed card save must replay the
                // resolution, not short-circuit before completing its task.
                if let Some(node) = self.pending_card_for_item(&projection, &item).await {
                    return match self
                        .resolve_peer_draft_locked(
                            &node,
                            crate::adapters::daemon::response_modes::DraftResolution::ShareStatus(
                                statement,
                            ),
                            Some(&projection),
                        )
                        .await
                    {
                        Ok(PeerDraftResolutionOutcome::Applied { reply: Some(reply) }) => {
                            Ok(RecipientActionOutcome::Applied { reply: Some(reply) })
                        }
                        Ok(PeerDraftResolutionOutcome::Applied { reply: None }) => {
                            Err(ProtocolError::Internal(
                                "status sharing completed without a durable receipt".to_owned(),
                            ))
                        }
                        Ok(PeerDraftResolutionOutcome::Idempotent) => {
                            Ok(RecipientActionOutcome::Idempotent)
                        }
                        Ok(PeerDraftResolutionOutcome::RefusedOneWay) => {
                            Err(ProtocolError::PeerVerification(
                                crate::adapters::daemon::response_modes::RAP_ONE_WAY_REFUSAL
                                    .to_owned(),
                            ))
                        }
                        Err(error) => Err(ProtocolError::Internal(error)),
                    };
                }
                let snapshot = projection.snapshot();
                if let Some(existing) = status_reply_for_item(&snapshot, &item) {
                    if existing.status.as_ref() != Some(&statement) {
                        return Err(ProtocolError::Malformed(
                            "status was already shared for this item".to_owned(),
                        ));
                    }
                    // A failed orphan card save, or a restart between a live
                    // append and its card save, left the row awaiting: settle
                    // it now, so the equal retry is idempotent and repairs.
                    let orphan_rows = self
                        .orphan_response_mutations(
                            &projection,
                            &item,
                            &existing.content,
                            Authorship::AgentComposed,
                        )
                        .await;
                    if !orphan_rows.is_empty()
                        && let Err(error) = commit_peer_rows(
                            &self.peer_row_serial,
                            &self.conversation,
                            &self.registry,
                            self.core.storage.as_ref(),
                            orphan_rows,
                        )
                        .await
                    {
                        tracing::error!(error = %error, "durable status reply could not update its orphan card");
                    }
                    return Ok(RecipientActionOutcome::Idempotent);
                }
                let turn = claim_reply_turn_id(fresh_reply_turn_candidate(), |id| {
                    projection.holds_reply_turn_id(id)
                })
                .map_err(|error| ProtocolError::Internal(error.to_string()))?;
                let receipt = RecipientReplyReceipt {
                    item_id: item_id.to_owned(),
                    turn_id: turn.as_str().to_owned(),
                    kind: ReplyKind::Status,
                };
                let content = statement.text();
                let orphan_rows = self
                    .orphan_response_mutations(
                        &projection, &item, &content, Authorship::AgentComposed,
                    )
                    .await;
                journal
                    .record_event(RoomEvent::RecipientItemReplied {
                        address: TurnAddress::answering(&item.address, turn),
                        answers: item.address.item().clone(),
                        content,
                        kind: ReplyKind::Status,
                        status: Some(statement.to_wire()),
                        authorship: Authorship::AgentComposed,
                        send_authorization: SendAuthorization::HumanDecided,
                        via_node: None,
                    })
                    .await
                    .map_err(|error| ProtocolError::Internal(error.to_string()))?;
                if let Err(error) = commit_peer_rows(
                    &self.peer_row_serial,
                    &self.conversation,
                    &self.registry,
                    self.core.storage.as_ref(),
                    orphan_rows,
                )
                .await
                {
                    tracing::error!(error = %error, "durable status reply could not update its orphan card");
                }
                Ok(RecipientActionOutcome::Applied { reply: Some(receipt) })
            }
            .await
        };
        let outcome = result.unwrap_or_else(|error| RecipientActionOutcome::Refused { error });
        self.send_recipient_action_result(conn_id, RecipientAction::ShareStatus, target, outcome)
            .await;
    }
}

#[cfg(feature = "a2a")]
pub(super) fn status_reply_for_item<'a>(
    snapshot: &'a crate::adapters::policy::recipient_item::RecipientItemSnapshot,
    item: &crate::domain::models::RecipientItemView,
) -> Option<&'a crate::domain::models::ReplyTurnView> {
    snapshot.replies.values().find(|turn| {
        turn.address.principal() == item.address.principal()
            && &turn.answers == item.address.item()
            && turn.kind == crate::domain::models::ReplyKind::Status
    })
}

#[cfg(all(test, feature = "a2a"))]
mod tests {
    use super::super::tests::{
        RecordingJournal, journaled_server, mock_core, mock_core_with_storage,
    };
    use super::super::{Conn, Conversation, PeerRowStage};
    use super::*;
    use crate::adapters::a2a::exec::{SubmitterKey, mint_inbound_node_id};
    use crate::adapters::daemon::protocol::{ClientFrame, DaemonFrame, ReplyVerdict};
    use crate::adapters::daemon::response_modes::{AWAITING_RESPONSE_PLACEHOLDER, DraftPhase};
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::NoOpMemory;
    use crate::domain::errors::StorageError;
    use crate::domain::models::{
        AgentId, Authorship, ItemAddress, ItemId, NodeState, PolicyPrincipal, ReceivedStories,
        ReceivedStory, ReplyKind, RoomEvent, SemanticMessageType, SendAuthorization,
        StatusDisclosure, StatusStatement, StoryHolder, StoryKey,
    };
    use crate::domain::ports::{
        InboundPeerRuntime, InboundPeerTask, PeerResponsePolicy, RoomJournal, StoragePort,
    };
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{Mutex, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    const ITEM_ID: &str = "ri_status_request";

    struct StatusFixture {
        server: Arc<AttachServer>,
        journal: Arc<RecordingJournal>,
        storage: Arc<dyn StoragePort>,
        frames: mpsc::Receiver<DaemonFrame>,
        node: AgentId,
        address: ItemAddress,
        assignment: ItemAddress,
        status: watch::Receiver<NodeState>,
    }

    impl StatusFixture {
        async fn next_action(&mut self) -> RecipientActionOutcome {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let DaemonFrame::RecipientActionResult {
                        action: RecipientAction::ShareStatus,
                        target: RecipientActionTarget::Item { .. },
                        outcome,
                    } = self
                        .frames
                        .recv()
                        .await
                        .expect("the status result queue stays open")
                    {
                        return outcome;
                    }
                }
            })
            .await
            .expect("the status act returns a correlated result")
        }

        async fn request(
            &mut self,
            item_id: &str,
            decline: bool,
            mode: AttachMode,
            tier: ConnectionTier,
        ) -> RecipientActionOutcome {
            self.server
                .handle_client_frame_tiered(
                    ClientFrame::ShareRecipientStatus {
                        item_id: item_id.to_owned(),
                        decline,
                    },
                    mode,
                    tier,
                    1,
                )
                .await;
            self.next_action().await
        }

        async fn share(&mut self, decline: bool) -> RecipientActionOutcome {
            self.request(
                ITEM_ID,
                decline,
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
            )
            .await
        }

        async fn turns(&self) -> Vec<RoomEvent> {
            self.journal
                .events
                .lock()
                .await
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        RoomEvent::RecipientItemReplied {
                            kind: ReplyKind::Status,
                            ..
                        }
                    )
                })
                .cloned()
                .collect()
        }

        async fn orphan(&self) {
            self.server.pending_drafts.abandon(self.node.as_str()).await;
            self.server.node_tree.deregister(&self.node).await;
        }

        fn row_id(&self) -> String {
            format!("peer-response-{}", self.node)
        }

        async fn next_reply(&mut self) -> RecipientActionOutcome {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let DaemonFrame::RecipientActionResult {
                        action: RecipientAction::Reply,
                        target: RecipientActionTarget::Item { .. },
                        outcome,
                    } = self
                        .frames
                        .recv()
                        .await
                        .expect("the reply result queue stays open")
                    {
                        return outcome;
                    }
                }
            })
            .await
            .expect("the reply act returns a correlated result")
        }

        async fn reply(
            &mut self,
            item_id: &str,
            text: &str,
            verdict: Option<ReplyVerdict>,
        ) -> RecipientActionOutcome {
            self.server
                .handle_client_frame_tiered(
                    ClientFrame::ReplyToRecipientItem {
                        item_id: item_id.to_owned(),
                        text: text.to_owned(),
                        verdict,
                    },
                    AttachMode::ReadWrite,
                    ConnectionTier::TrustedLocal,
                    1,
                )
                .await;
            self.next_reply().await
        }

        async fn reply_turns(&self) -> Vec<RoomEvent> {
            self.journal
                .events
                .lock()
                .await
                .iter()
                .filter(|event| matches!(event, RoomEvent::RecipientItemReplied { .. }))
                .cloned()
                .collect()
        }
    }

    fn listed_you() -> ReceivedStories {
        ReceivedStories::Listed {
            stories: vec![ReceivedStory {
                key: StoryKey::parse("S7-09").unwrap(),
                holder: StoryHolder::You,
            }],
        }
    }

    fn listed_no_holder() -> ReceivedStories {
        ReceivedStories::Listed {
            stories: vec![ReceivedStory {
                key: StoryKey::parse("S7-07").unwrap(),
                holder: StoryHolder::NoHolder,
            }],
        }
    }

    async fn fixture(workspace: &Path) -> StatusFixture {
        fixture_with(
            workspace,
            StatusDisclosure::Minimal,
            true,
            SemanticMessageType::StatusRequest,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn fixture_with(
        workspace: &Path,
        level: StatusDisclosure,
        held: bool,
        message_type: SemanticMessageType,
        storage: Option<Arc<dyn StoragePort>>,
        started_stories: Option<ReceivedStories>,
    ) -> StatusFixture {
        let (core, storage) = match storage {
            Some(storage) => {
                mock_core_with_storage(workspace, vec![], Arc::new(NoOpMemory), storage)
            }
            None => mock_core(workspace, vec![]),
        };
        let submitter = SubmitterKey::loopback();
        let peer = submitter.pseudonymous_peer_id();
        let node = mint_inbound_node_id(&submitter, "status-task");
        let address = ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay(ITEM_ID));
        let assignment = ItemAddress::from_a2a_ingress(
            peer.clone(),
            ItemId::from_replay("ri_status_assignment"),
        );
        let journal = Arc::new(RecordingJournal::default());
        if held {
            journal
                .record_event(RoomEvent::RecipientItemReceived {
                    address: assignment.clone(),
                    task: "assignment-task".to_owned(),
                    alias: None,
                    message_type: SemanticMessageType::StoryAssignment,
                    content: "private assignment body".to_owned(),
                    stories: Some(ReceivedStories::Listed {
                        stories: vec![ReceivedStory {
                            key: StoryKey::parse("S7-03").unwrap(),
                            holder: StoryHolder::You,
                        }],
                    }),
                    in_reply_to: None,
                    capability_hash: None,
                    reply_capability_matched: false,
                    authorship: Authorship::Unknown,
                    send_authorization: SendAuthorization::Unknown,
                })
                .await
                .unwrap();
        }
        journal
            .record_event(RoomEvent::RecipientItemReceived {
                address: address.clone(),
                task: "status-task".to_owned(),
                alias: None,
                message_type,
                content: "requester text must not choose shared fields".to_owned(),
                stories: started_stories,
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            })
            .await
            .unwrap();
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = journaled_server(
            core,
            Arc::new(Mutex::new(Conversation {
                id: "status-acts".to_owned(),
                ..Default::default()
            })),
            domain_tx,
            journal.clone(),
        );
        let status = server
            .start(
                InboundPeerTask {
                    node_id: node.clone(),
                    principal: PolicyPrincipal::Pseudonymous(peer),
                    text: "requester text must not choose shared fields".to_owned(),
                    subagent_type: "a2a-status-test".to_owned(),
                    response_policy: PeerResponsePolicy {
                        status_disclosure: level,
                        provenance: crate::domain::models::InteractionPolicySnapshot {
                            message_type,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(*status.borrow(), NodeState::Waiting);
        let (tx, frames) = mpsc::channel(64);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        StatusFixture {
            server,
            journal,
            storage,
            frames,
            node,
            address,
            assignment,
            status,
        }
    }

    fn minimal_statement() -> StatusStatement {
        StatusStatement::from_wire(&serde_json::json!({
            "level": "minimal", "stories": [{"key": "S7-03"}],
        }))
        .unwrap()
    }

    fn refused(outcome: RecipientActionOutcome) -> ProtocolError {
        match outcome {
            RecipientActionOutcome::Refused { error } => error,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_share_gates_in_order_and_every_refusal_writes_zero_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = fixture(tmp.path()).await;
        for decline in [false, true] {
            let before = fixture.journal.events.lock().await.len();
            assert_eq!(
                refused(
                    fixture
                        .request(
                            "ri_unknown",
                            decline,
                            AttachMode::ReadOnly,
                            ConnectionTier::TrustedLocal,
                        )
                        .await
                ),
                ProtocolError::ReadOnly,
            );
            assert_eq!(
                refused(
                    fixture
                        .request(
                            "ri_unknown",
                            decline,
                            AttachMode::ReadWrite,
                            ConnectionTier::Peer,
                        )
                        .await
                ),
                ProtocolError::PeerVerification(
                    "recipient reply is same-host trusted-local only".to_owned(),
                ),
            );
            assert_eq!(fixture.journal.events.lock().await.len(), before);
        }
        for case in ["missing", "removed", "type", "preview", "control"] {
            let tmp = tempfile::tempdir().unwrap();
            let message_type = if matches!(case, "removed" | "type") {
                SemanticMessageType::Consultation
            } else {
                SemanticMessageType::StatusRequest
            };
            let mut fixture = fixture_with(
                tmp.path(),
                StatusDisclosure::Minimal,
                true,
                message_type,
                None,
                None,
            )
            .await;
            if case == "removed" {
                fixture
                    .journal
                    .record_event(RoomEvent::RecipientItemRemoved {
                        address: fixture.address.clone(),
                    })
                    .await
                    .unwrap();
            }
            if matches!(case, "preview" | "control") {
                let mut conversation = fixture.server.conversation.lock().await;
                if case == "control" {
                    conversation.peer_row_controls.remove(&fixture.row_id());
                } else {
                    conversation
                        .peer_row_controls
                        .get_mut(&fixture.row_id())
                        .unwrap()
                        .status = None;
                }
            }
            let before = fixture.journal.events.lock().await.len();
            let expected = match case {
                "missing" => "recipient item not found",
                "removed" => "recipient item already removed",
                "type" => "status sharing answers status_request items only",
                _ => "no status statement was prepared for this item",
            };
            for decline in [false, true] {
                let item_id = if case == "missing" {
                    "ri_unknown"
                } else {
                    ITEM_ID
                };
                assert_eq!(
                    refused(
                        fixture
                            .request(
                                item_id,
                                decline,
                                AttachMode::ReadWrite,
                                ConnectionTier::TrustedLocal,
                            )
                            .await
                    ),
                    ProtocolError::Malformed(expected.to_owned()),
                    "{case}",
                );
                assert_eq!(fixture.journal.events.lock().await.len(), before, "{case}");
                assert_eq!(*fixture.status.borrow(), NodeState::Waiting);
            }
        }
    }

    #[tokio::test]
    async fn status_share_uses_the_stored_snapshot_and_completes_the_live_task() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = fixture(tmp.path()).await;
        fixture
            .journal
            .record_event(RoomEvent::RecipientItemRemoved {
                address: fixture.assignment.clone(),
            })
            .await
            .unwrap();
        assert!(matches!(
            fixture.share(false).await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Status,
                    ..
                }),
            },
        ));
        let turns = fixture.turns().await;
        assert_eq!(turns.len(), 1);
        let RoomEvent::RecipientItemReplied {
            content,
            kind,
            status,
            authorship,
            send_authorization,
            via_node,
            ..
        } = &turns[0]
        else {
            unreachable!()
        };
        let statement = minimal_statement();
        assert_eq!(content, &statement.text());
        assert_eq!(*kind, ReplyKind::Status);
        assert_eq!(status.as_ref(), Some(&statement.to_wire()));
        assert_eq!(*authorship, Authorship::AgentComposed);
        assert_eq!(*send_authorization, SendAuthorization::HumanDecided);
        assert_eq!(via_node.as_ref(), Some(&fixture.node));
        assert_eq!(*fixture.status.borrow(), NodeState::Completed);
        let result = fixture.server.take_result(&fixture.node).await.unwrap();
        assert_eq!(result.text, statement.text());
        assert_eq!(result.authorship, Authorship::AgentComposed);
        assert_eq!(result.send_authorization, SendAuthorization::HumanDecided);
        let persisted = fixture
            .storage
            .load_conversation("status-acts")
            .await
            .unwrap()
            .unwrap();
        let row_id = fixture.row_id();
        assert_eq!(
            persisted.peer_row_controls[&row_id].stage,
            PeerRowStage::Settled
        );
        assert!(!persisted.peer_row_controls[&row_id].node_resolvable);
        assert_eq!(
            persisted
                .messages
                .iter()
                .find(|row| row.id == row_id)
                .unwrap()
                .content,
            statement.text(),
        );
    }

    #[tokio::test]
    async fn decline_level_none_and_no_held_stories_share_identical_canonical_payloads() {
        let mut payloads = Vec::new();
        for (level, held, decline) in [
            (StatusDisclosure::Minimal, true, true),
            (StatusDisclosure::None, true, false),
            (StatusDisclosure::Minimal, false, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut fixture = fixture_with(
                tmp.path(),
                level,
                held,
                SemanticMessageType::StatusRequest,
                None,
                None,
            )
            .await;
            assert!(matches!(
                fixture.share(decline).await,
                RecipientActionOutcome::Applied { .. }
            ));
            let turns = fixture.turns().await;
            let RoomEvent::RecipientItemReplied {
                content,
                kind,
                status,
                authorship,
                send_authorization,
                ..
            } = &turns[0]
            else {
                unreachable!()
            };
            assert_eq!(content, "No status shared.");
            assert_eq!(status.as_ref(), Some(&serde_json::json!({"stories": []})));
            payloads.push(
                serde_json::to_vec(&(content, kind, status, authorship, send_authorization))
                    .unwrap(),
            );
        }
        assert_eq!(payloads[0], payloads[1]);
        assert_eq!(payloads[0], payloads[2]);
    }

    #[tokio::test]
    async fn concurrent_orphan_shares_append_once_replace_the_row_and_refuse_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = fixture(tmp.path()).await;
        fixture.orphan().await;
        let frame = ClientFrame::ShareRecipientStatus {
            item_id: ITEM_ID.to_owned(),
            decline: false,
        };
        tokio::join!(
            fixture.server.handle_client_frame_tiered(
                frame.clone(),
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            ),
            fixture.server.handle_client_frame_tiered(
                frame,
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            ),
        );
        let outcomes = [fixture.next_action().await, fixture.next_action().await];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RecipientActionOutcome::Applied { .. },))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RecipientActionOutcome::Idempotent,))
                .count(),
            1
        );
        let turns = fixture.turns().await;
        assert_eq!(turns.len(), 1);
        assert!(matches!(
            &turns[0],
            RoomEvent::RecipientItemReplied { via_node: None, .. }
        ));
        let persisted = fixture
            .storage
            .load_conversation("status-acts")
            .await
            .unwrap()
            .unwrap();
        let row_id = fixture.row_id();
        let row = persisted
            .messages
            .iter()
            .find(|row| row.id == row_id)
            .unwrap();
        assert_eq!(row.content, minimal_statement().text());
        assert_eq!(row.authorship, Authorship::AgentComposed);
        assert_eq!(row.send_authorization, SendAuthorization::HumanDecided);
        assert_eq!(
            persisted.peer_row_controls[&row_id].stage,
            PeerRowStage::Settled
        );
        assert!(!persisted.peer_row_controls[&row_id].node_resolvable);
        assert!(fixture.server.take_result(&fixture.node).await.is_none());
        assert!(
            fixture
                .server
                .node_tree
                .status_rx(&fixture.node)
                .await
                .is_none()
        );
        let before = fixture.journal.events.lock().await.len();
        assert_eq!(
            refused(fixture.share(true).await),
            ProtocolError::Malformed("status was already shared for this item".to_owned()),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), before);
    }

    struct FailNextSave {
        inner: Arc<dyn StoragePort>,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl StoragePort for FailNextSave {
        async fn read_snapshot(
            &self,
            conversation_id: &str,
            checkpoint: crate::domain::models::checkpoint::CheckpointId,
            path: &Path,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            self.inner
                .read_snapshot(conversation_id, checkpoint, path)
                .await
        }

        async fn save_conversation(&self, conversation: &Conversation) -> Result<(), StorageError> {
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(StorageError::IoError(
                    "injected card persistence failure".to_owned(),
                ));
            }
            self.inner.save_conversation(conversation).await
        }

        async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
            self.inner.load_conversation(id).await
        }

        async fn list_conversations(
            &self,
        ) -> Result<Vec<crate::domain::models::ConversationSummary>, StorageError> {
            self.inner.list_conversations().await
        }
    }

    #[tokio::test]
    async fn status_share_retry_after_append_before_card_save_completes_once() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(FailNextSave {
            inner: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tmp.path()),
                tmp.path().to_path_buf(),
            )),
            fail: AtomicBool::new(false),
        });
        let mut fixture = fixture_with(
            tmp.path(),
            StatusDisclosure::Minimal,
            true,
            SemanticMessageType::StatusRequest,
            Some(storage.clone()),
            None,
        )
        .await;
        storage.fail.store(true, Ordering::SeqCst);
        assert!(matches!(
            refused(fixture.share(false).await),
            ProtocolError::Internal(error) if error.contains("injected card persistence failure"),
        ));
        assert!(
            !storage.fail.load(Ordering::SeqCst),
            "the card save reached the injected failure"
        );
        assert_eq!(
            fixture.turns().await.len(),
            1,
            "the turn was already durable"
        );
        assert!(fixture.journal.events.lock().await.iter().any(|event| matches!(
            event, RoomEvent::PeerDraftResolved { node, sent: true, .. } if node == &fixture.node,
        )));
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Ready),
        );
        assert_eq!(*fixture.status.borrow(), NodeState::Waiting);
        assert!(fixture.server.take_result(&fixture.node).await.is_none());
        let row_id = fixture.row_id();
        for conversation in [
            fixture.server.conversation.lock().await.clone(),
            fixture
                .storage
                .load_conversation("status-acts")
                .await
                .unwrap()
                .unwrap(),
        ] {
            assert_eq!(
                conversation.peer_row_controls[&row_id].stage,
                PeerRowStage::AwaitingOperator
            );
            assert_eq!(
                conversation
                    .messages
                    .iter()
                    .find(|row| row.id == row_id)
                    .unwrap()
                    .content,
                AWAITING_RESPONSE_PLACEHOLDER,
            );
        }
        let before_conflict = fixture.journal.events.lock().await.len();
        assert_eq!(
            refused(fixture.share(true).await),
            ProtocolError::Internal("status was already shared for this item".to_owned()),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), before_conflict);
        assert!(matches!(
            fixture.share(false).await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Status,
                    ..
                }),
            },
        ));
        assert_eq!(
            fixture.turns().await.len(),
            1,
            "retry reuses the one Status turn"
        );
        assert_eq!(*fixture.status.borrow(), NodeState::Completed);
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Settled),
        );
        let result = fixture.server.take_result(&fixture.node).await.unwrap();
        assert_eq!(result.text, minimal_statement().text());
        assert_eq!(result.authorship, Authorship::AgentComposed);
        assert_eq!(result.send_authorization, SendAuthorization::HumanDecided);
        let persisted = fixture
            .storage
            .load_conversation("status-acts")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.peer_row_controls[&row_id].stage,
            PeerRowStage::Settled
        );
        assert!(!persisted.peer_row_controls[&row_id].node_resolvable);
        assert_eq!(
            persisted
                .messages
                .iter()
                .find(|row| row.id == row_id)
                .unwrap()
                .content,
            minimal_statement().text(),
        );
        let before_duplicate = fixture.journal.events.lock().await.len();
        assert!(matches!(
            fixture.share(false).await,
            RecipientActionOutcome::Idempotent
        ));
        assert_eq!(fixture.journal.events.lock().await.len(), before_duplicate);
        assert!(
            fixture
                .server
                .node_tree
                .status_rx(&fixture.node)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn orphan_share_retry_after_failed_card_save_settles_the_row_once() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(FailNextSave {
            inner: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tmp.path()),
                tmp.path().to_path_buf(),
            )),
            fail: AtomicBool::new(false),
        });
        let mut fixture = fixture_with(
            tmp.path(),
            StatusDisclosure::Minimal,
            true,
            SemanticMessageType::StatusRequest,
            Some(storage.clone()),
            None,
        )
        .await;
        fixture.orphan().await;
        let row_id = fixture.row_id();
        storage.fail.store(true, Ordering::SeqCst);
        assert!(matches!(
            fixture.share(false).await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Status,
                    ..
                }),
            },
        ));
        assert!(
            !storage.fail.load(Ordering::SeqCst),
            "the orphan card save reached the injected failure"
        );
        assert_eq!(fixture.turns().await.len(), 1);
        let awaiting = fixture.server.conversation.lock().await.clone();
        assert_eq!(
            awaiting.peer_row_controls[&row_id].stage,
            PeerRowStage::AwaitingOperator
        );
        let before_retry = fixture.journal.events.lock().await.len();
        assert!(matches!(
            fixture.share(false).await,
            RecipientActionOutcome::Idempotent
        ));
        assert_eq!(fixture.journal.events.lock().await.len(), before_retry);
        assert_eq!(fixture.turns().await.len(), 1);
        for conversation in [
            fixture.server.conversation.lock().await.clone(),
            fixture
                .storage
                .load_conversation("status-acts")
                .await
                .unwrap()
                .unwrap(),
        ] {
            assert_eq!(
                conversation.peer_row_controls[&row_id].stage,
                PeerRowStage::Settled
            );
            assert!(!conversation.peer_row_controls[&row_id].node_resolvable);
            let row = conversation
                .messages
                .iter()
                .find(|row| row.id == row_id)
                .unwrap();
            assert_eq!(row.content, minimal_statement().text());
            assert_eq!(row.authorship, Authorship::AgentComposed);
        }
        assert_eq!(
            refused(fixture.share(true).await),
            ProtocolError::Malformed("status was already shared for this item".to_owned()),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), before_retry);
    }

    #[tokio::test]
    async fn live_status_replay_compares_kind_and_status_not_only_content() {
        use crate::domain::models::{TurnAddress, claim_reply_turn_id, fresh_reply_turn_candidate};
        for (kind, status) in [
            (ReplyKind::Answer, None),
            (
                ReplyKind::Status,
                Some(StatusStatement::nothing().to_wire()),
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut fixture = fixture(tmp.path()).await;
            let turn = claim_reply_turn_id(fresh_reply_turn_candidate(), |_| false).unwrap();
            fixture
                .journal
                .record_event(RoomEvent::RecipientItemReplied {
                    address: TurnAddress::answering(&fixture.address, turn),
                    answers: fixture.address.item().clone(),
                    content: minimal_statement().text(),
                    kind,
                    status,
                    authorship: Authorship::AgentComposed,
                    send_authorization: SendAuthorization::HumanDecided,
                    via_node: Some(fixture.node.clone()),
                })
                .await
                .unwrap();
            let before = fixture.journal.events.lock().await.len();
            assert!(matches!(
                fixture.share(false).await,
                RecipientActionOutcome::Refused { .. }
            ));
            assert_eq!(fixture.journal.events.lock().await.len(), before);
            assert_eq!(*fixture.status.borrow(), NodeState::Waiting);
            assert_eq!(
                fixture
                    .server
                    .pending_drafts
                    .phase(fixture.node.as_str())
                    .await,
                Some(DraftPhase::Ready),
            );
        }
    }

    #[tokio::test]
    async fn live_status_resolution_reuses_an_item_status_from_a_different_act() {
        use crate::domain::models::{TurnAddress, claim_reply_turn_id, fresh_reply_turn_candidate};
        for equal in [true, false] {
            let tmp = tempfile::tempdir().unwrap();
            let mut fixture = fixture(tmp.path()).await;
            let statement = if equal {
                minimal_statement()
            } else {
                StatusStatement::nothing()
            };
            let turn = claim_reply_turn_id(fresh_reply_turn_candidate(), |_| false).unwrap();
            let turn_id = turn.as_str().to_owned();
            fixture
                .journal
                .record_event(RoomEvent::RecipientItemReplied {
                    address: TurnAddress::answering(&fixture.address, turn),
                    answers: fixture.address.item().clone(),
                    content: statement.text(),
                    kind: ReplyKind::Status,
                    status: Some(statement.to_wire()),
                    authorship: Authorship::AgentComposed,
                    send_authorization: SendAuthorization::HumanDecided,
                    via_node: None,
                })
                .await
                .unwrap();
            let before = fixture.journal.events.lock().await.len();
            let outcome = fixture.share(false).await;
            if equal {
                assert!(matches!(outcome, RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        kind: ReplyKind::Status, turn_id: reused, ..
                    }),
                } if reused == turn_id));
                assert_eq!(*fixture.status.borrow(), NodeState::Completed);
                assert_eq!(
                    fixture
                        .server
                        .take_result(&fixture.node)
                        .await
                        .unwrap()
                        .text,
                    minimal_statement().text(),
                );
            } else {
                assert_eq!(
                    refused(outcome),
                    ProtocolError::Internal("status was already shared for this item".to_owned()),
                );
                assert_eq!(fixture.journal.events.lock().await.len(), before);
                assert_eq!(*fixture.status.borrow(), NodeState::Waiting);
            }
            assert_eq!(fixture.turns().await.len(), 1);
        }
    }

    /// Story 19.22 `AC4a` — the verdict gate order after `find_recipient_item`
    /// and the removed check, every refusal writing zero rows, plus the
    /// direct-path positives: an empty Accept normalizes to `Accepted.`, and a
    /// plain reply (`verdict: None`) stays an `Answer`.
    #[tokio::test]
    async fn verdict_gates_refuse_in_order_and_write_zero_rows() {
        // A verdict on an item that carries no story list is refused for both
        // kinds, and the removed check precedes the verdict gates.
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = fixture_with(
            tmp.path(),
            StatusDisclosure::Minimal,
            false,
            SemanticMessageType::Consultation,
            None,
            None,
        )
        .await;
        fixture.orphan().await;
        let before = fixture.journal.events.lock().await.len();
        for verdict in [Some(ReplyVerdict::Accept), Some(ReplyVerdict::Discuss)] {
            assert_eq!(
                refused(fixture.reply(ITEM_ID, "a reply", verdict).await),
                ProtocolError::Malformed(
                    "a verdict answers an item that carries a story list".to_owned(),
                ),
            );
            assert_eq!(fixture.journal.events.lock().await.len(), before);
        }
        fixture
            .journal
            .record_event(RoomEvent::RecipientItemRemoved {
                address: fixture.address.clone(),
            })
            .await
            .unwrap();
        let before = fixture.journal.events.lock().await.len();
        assert_eq!(
            refused(
                fixture
                    .reply(ITEM_ID, "a reply", Some(ReplyVerdict::Accept))
                    .await
            ),
            ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned()),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), before);

        // An Accept on a list that assigns the operator no story is refused;
        // an empty Discuss still hits the existing empty-text refusal; a
        // Discuss with text is the DiscussOnly path and records one Discuss
        // turn without a via_node.
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = fixture_with(
            tmp.path(),
            StatusDisclosure::Minimal,
            false,
            SemanticMessageType::StoryAssignment,
            None,
            Some(listed_no_holder()),
        )
        .await;
        fixture.orphan().await;
        let before = fixture.journal.events.lock().await.len();
        assert_eq!(
            refused(
                fixture
                    .reply(ITEM_ID, "concerns", Some(ReplyVerdict::Accept))
                    .await
            ),
            ProtocolError::Malformed("accept answers an item that assigns you a story".to_owned()),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), before);
        assert_eq!(
            refused(
                fixture
                    .reply(ITEM_ID, "   ", Some(ReplyVerdict::Discuss))
                    .await
            ),
            ProtocolError::Malformed("reply text is empty".to_owned()),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), before);
        assert!(matches!(
            fixture
                .reply(ITEM_ID, "release it", Some(ReplyVerdict::Discuss))
                .await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Discuss,
                    ..
                }),
            },
        ));
        let turns = fixture.reply_turns().await;
        assert_eq!(turns.len(), 1);
        assert!(matches!(
            &turns[0],
            RoomEvent::RecipientItemReplied {
                kind: ReplyKind::Discuss,
                via_node: None,
                ..
            }
        ));

        // On an item that assigns the operator a story, an empty Accept
        // normalizes to `Accepted.` and a plain reply stays an Answer.
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = fixture_with(
            tmp.path(),
            StatusDisclosure::Minimal,
            false,
            SemanticMessageType::StoryAssignment,
            None,
            Some(listed_you()),
        )
        .await;
        fixture.orphan().await;
        assert!(matches!(
            fixture.reply(ITEM_ID, "", Some(ReplyVerdict::Accept)).await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Accept,
                    ..
                }),
            },
        ));
        assert!(matches!(
            fixture.reply(ITEM_ID, "noted", None).await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Answer,
                    ..
                }),
            },
        ));
        let turns = fixture.reply_turns().await;
        assert_eq!(turns.len(), 2);
        let contents: Vec<(&str, &ReplyKind)> = turns
            .iter()
            .map(|event| match event {
                RoomEvent::RecipientItemReplied { content, kind, .. } => (content.as_str(), kind),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            contents,
            [
                ("Accepted.", &ReplyKind::Accept),
                ("noted", &ReplyKind::Answer),
            ],
        );
    }

    /// Story 19.22 Task 4 failure injection — a durable Accept turn whose card
    /// save then fails leaves the pending card awaiting; retrying the same
    /// content and kind replays exactly the one turn and settles the card,
    /// while the same content as an `Answer` or a `Discuss` verdict is refused
    /// and does not settle it.
    #[tokio::test]
    async fn live_accept_retry_after_failed_card_save_replays_once_and_other_kinds_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(FailNextSave {
            inner: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tmp.path()),
                tmp.path().to_path_buf(),
            )),
            fail: AtomicBool::new(false),
        });
        let mut fixture = fixture_with(
            tmp.path(),
            StatusDisclosure::Minimal,
            false,
            SemanticMessageType::StoryAssignment,
            Some(storage.clone()),
            Some(listed_you()),
        )
        .await;
        storage.fail.store(true, Ordering::SeqCst);
        assert!(matches!(
            refused(
                fixture
                    .reply(ITEM_ID, "", Some(ReplyVerdict::Accept))
                    .await
            ),
            ProtocolError::Internal(error) if error.contains("injected card persistence failure"),
        ));
        assert!(
            !storage.fail.load(Ordering::SeqCst),
            "the card save reached the injected failure"
        );
        let turns = fixture.reply_turns().await;
        assert_eq!(turns.len(), 1, "the Accept turn was already durable");
        let turn_id = match &turns[0] {
            RoomEvent::RecipientItemReplied {
                address,
                content,
                kind,
                authorship,
                via_node,
                ..
            } => {
                assert_eq!(content, "Accepted.");
                assert_eq!(*kind, ReplyKind::Accept);
                assert_eq!(*authorship, Authorship::HumanWritten);
                assert_eq!(via_node.as_ref(), Some(&fixture.node));
                address.turn().as_str().to_owned()
            }
            _ => unreachable!(),
        };
        assert_eq!(*fixture.status.borrow(), NodeState::Waiting);
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Ready),
        );
        assert!(fixture.server.take_result(&fixture.node).await.is_none());
        let row_id = fixture.row_id();
        for conversation in [
            fixture.server.conversation.lock().await.clone(),
            fixture
                .storage
                .load_conversation("status-acts")
                .await
                .unwrap()
                .unwrap(),
        ] {
            assert_eq!(
                conversation.peer_row_controls[&row_id].stage,
                PeerRowStage::AwaitingOperator
            );
            assert_eq!(
                conversation
                    .messages
                    .iter()
                    .find(|row| row.id == row_id)
                    .unwrap()
                    .content,
                AWAITING_RESPONSE_PLACEHOLDER,
            );
        }
        let settled_events = fixture.journal.events.lock().await.len();

        // The same content as an Answer is refused and does not settle the card.
        assert_eq!(
            refused(fixture.reply(ITEM_ID, "Accepted.", None).await),
            ProtocolError::Internal(
                "this card was already answered with a recorded reply; only that same answer can \
                 be resent"
                    .to_owned()
            ),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), settled_events);
        assert_eq!(fixture.reply_turns().await.len(), 1);
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Ready),
        );
        assert_eq!(*fixture.status.borrow(), NodeState::Waiting);

        // The same content as a Discuss verdict is refused the same way.
        assert_eq!(
            refused(
                fixture
                    .reply(ITEM_ID, "Accepted.", Some(ReplyVerdict::Discuss))
                    .await
            ),
            ProtocolError::Internal(
                "this card was already answered with a recorded reply; only that same answer can \
                 be resent"
                    .to_owned()
            ),
        );
        assert_eq!(fixture.journal.events.lock().await.len(), settled_events);
        assert_eq!(fixture.reply_turns().await.len(), 1);
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Ready),
        );

        // Retrying the same content and kind replays exactly the one turn and
        // settles the card.
        assert!(matches!(
            fixture.reply(ITEM_ID, "", Some(ReplyVerdict::Accept)).await,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Accept,
                    turn_id: replayed,
                    ..
                }),
            } if replayed == turn_id,
        ));
        assert_eq!(
            fixture.reply_turns().await.len(),
            1,
            "retry reuses the one Accept turn"
        );
        assert_eq!(*fixture.status.borrow(), NodeState::Completed);
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Settled),
        );
        let result = fixture.server.take_result(&fixture.node).await.unwrap();
        assert_eq!(result.text, "Accepted.");
        assert_eq!(result.authorship, Authorship::HumanWritten);
        assert_eq!(result.send_authorization, SendAuthorization::HumanDecided);
        let persisted = fixture
            .storage
            .load_conversation("status-acts")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.peer_row_controls[&row_id].stage,
            PeerRowStage::Settled
        );
        assert!(!persisted.peer_row_controls[&row_id].node_resolvable);
        assert_eq!(
            persisted
                .messages
                .iter()
                .find(|row| row.id == row_id)
                .unwrap()
                .content,
            "Accepted.",
        );
    }
}

#[cfg(all(test, not(feature = "a2a")))]
#[tokio::test]
async fn status_share_without_a2a_keeps_the_reply_gate_and_never_writes_a_turn() {
    use super::tests::{RecordingJournal, journaled_server, mock_core};
    use super::{Conn, Conversation};
    use crate::adapters::daemon::protocol::{ClientFrame, DaemonFrame};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, watch};

    let tmp = tempfile::tempdir().unwrap();
    let (core, _) = mock_core(tmp.path(), vec![]);
    let journal = Arc::new(RecordingJournal::default());
    let (domain_tx, _) = mpsc::unbounded_channel();
    let server = journaled_server(
        core,
        Arc::new(Mutex::new(Conversation::default())),
        domain_tx,
        journal.clone(),
    );
    let (tx, mut frames) = mpsc::channel(8);
    server.registry.lock().await.conns.push(Conn {
        id: 1,
        tx,
        terminal_tx: watch::channel(None).0,
        mode: AttachMode::ReadWrite,
        tier: ConnectionTier::TrustedLocal,
        approval_ready: true,
    });
    for (mode, tier, expected) in [
        (
            AttachMode::ReadOnly,
            ConnectionTier::TrustedLocal,
            ProtocolError::ReadOnly,
        ),
        (
            AttachMode::ReadWrite,
            ConnectionTier::Peer,
            ProtocolError::PeerVerification(
                "recipient reply is same-host trusted-local only".to_owned(),
            ),
        ),
        (
            AttachMode::ReadWrite,
            ConnectionTier::TrustedLocal,
            ProtocolError::Malformed(
                "status sharing needs the `a2a` feature; this build was compiled without it."
                    .to_owned(),
            ),
        ),
    ] {
        server
            .handle_client_frame_tiered(
                ClientFrame::ShareRecipientStatus {
                    item_id: "ri_unknown".to_owned(),
                    decline: false,
                },
                mode,
                tier,
                1,
            )
            .await;
        assert!(matches!(
            frames.try_recv().unwrap(),
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::ShareStatus,
                outcome: RecipientActionOutcome::Refused { error }, ..
            } if error == expected,
        ));
        assert!(journal.events.lock().await.is_empty());
    }
}
