//! Story 19.25 `AC4` — the human-decided retrospective reply act.
//!
//! The gates are this act's own; the durable tail (live card, orphan row,
//! idempotent replay) is the share tail it shares with Story 19.24's status
//! act ([`AttachServer::settle_shared_reply`], `D11`).

use super::{
    AttachMode, AttachServer, ConnectionTier, ProtocolError, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget,
};
#[cfg(feature = "a2a")]
use super::{RECIPIENT_ITEM_REMOVED, reply_acts::SharedReply};
use crate::domain::models::RetroField;

impl AttachServer {
    /// Share exactly the arrival-time retrospective manifest minus the
    /// fields the operator left out, or the canonical empty contribution for
    /// Decline (`D3`). Gates, in order: serial lock + item gate (a removed
    /// item is refused) → a `retro_request` item → a persisted preview → a
    /// Decline carries no field list. Every refusal writes zero rows.
    pub(super) async fn share_recipient_retro(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: &str,
        leave_out: &[RetroField],
        decline: bool,
    ) {
        let target = RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        #[cfg(not(feature = "a2a"))]
        let result = {
            let _ = (leave_out, decline);
            super::recipient_command_gate(mode, tier, "reply", true).and_then(|()| {
                Err(ProtocolError::Malformed(
                    "retrospective sharing needs the `a2a` feature; this build was compiled \
                     without it."
                        .to_owned(),
                ))
            })
        };
        #[cfg(feature = "a2a")]
        let result = {
            use crate::domain::models::{
                RecipientItemState, RetroContribution, RetroFieldSet, SemanticMessageType,
            };

            let _reply_guard = self.peer_reply_serial.lock().await;
            async {
                let (item, projection, journal) = self
                    .find_recipient_item(mode, tier, "reply", item_id)
                    .await?;
                if matches!(item.state, RecipientItemState::Removed { .. }) {
                    return Err(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned()));
                }
                if item.message_type != SemanticMessageType::RetroRequest {
                    return Err(ProtocolError::Malformed(
                        "retrospective replies answer retro_request items only".to_owned(),
                    ));
                }
                let stored = {
                    let conversation = self.conversation.lock().await;
                    conversation.messages.iter().find_map(|message| {
                        let control = conversation.peer_row_controls.get(&message.id)?;
                        if control.item_address.as_ref() != Some(&item.address) {
                            return None;
                        }
                        control
                            .retro
                            .as_ref()
                            .map(|preview| preview.contribution.clone())
                    })
                }
                .ok_or_else(|| {
                    ProtocolError::Malformed(
                        "no retrospective summary was prepared for this item".to_owned(),
                    )
                })?;
                if decline && !leave_out.is_empty() {
                    return Err(ProtocolError::Malformed(
                        "decline takes no field list".to_owned(),
                    ));
                }
                let contribution = if decline {
                    RetroContribution::nothing()
                } else {
                    stored.without(&leave_out.iter().copied().collect::<RetroFieldSet>())
                };
                self.settle_shared_reply(
                    item_id,
                    &item,
                    &projection,
                    journal.as_ref(),
                    SharedReply::Retro(contribution),
                )
                .await
            }
            .await
        };
        let outcome = result.unwrap_or_else(|error| RecipientActionOutcome::Refused { error });
        self.send_recipient_action_result(conn_id, RecipientAction::ShareRetro, target, outcome)
            .await;
    }
}

#[cfg(all(test, feature = "a2a"))]
mod tests {
    use super::super::PeerRowStage;
    use super::super::reply_acts::tests::{
        FailNextSave, ITEM_ID, StatusFixture, fixture_with, refused,
    };
    use super::*;
    use crate::adapters::daemon::protocol::{ClientFrame, DaemonFrame, RecipientReplyReceipt};
    use crate::adapters::daemon::response_modes::{AWAITING_RESPONSE_PLACEHOLDER, DraftPhase};
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::domain::models::{
        Authorship, NodeState, RETRO_REPLY_TEXT, ReplyKind, RetroContribution, RetroPreview,
        RoomEvent, SemanticMessageType, SendAuthorization, StatusDisclosure, TurnAddress,
        claim_reply_turn_id, fresh_reply_turn_candidate,
    };
    use crate::domain::ports::{InboundPeerRuntime, RoomJournal, StoragePort};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A manifest the journal cannot produce (every entry is recorded at
    /// time 0), so each assertion proves the act shares the stored snapshot.
    fn stored() -> RetroContribution {
        RetroContribution {
            received: BTreeMap::from([("consultation".to_owned(), 3)]),
            reply_minutes: vec![4, 25, 90],
            ack_minutes: vec![60],
        }
    }

    fn row_id(fixture: &StatusFixture) -> String {
        format!("peer-response-{}", fixture.node)
    }

    async fn retro_fixture_with(
        workspace: &Path,
        message_type: SemanticMessageType,
        storage: Option<Arc<dyn StoragePort>>,
        preview: Option<RetroPreview>,
    ) -> StatusFixture {
        let fixture = fixture_with(
            workspace,
            StatusDisclosure::None,
            false,
            message_type,
            storage,
            None,
        )
        .await;
        let row_id = row_id(&fixture);
        let mut conversation = fixture.server.conversation.lock().await;
        let control = conversation.peer_row_controls.get_mut(&row_id).unwrap();
        assert_eq!(
            control.retro.is_some(),
            message_type == SemanticMessageType::RetroRequest,
            "only a retro_request wait card is prepared at arrival",
        );
        if control.retro.is_some() {
            control.retro = preview;
        }
        drop(conversation);
        fixture
    }

    async fn retro_fixture(
        workspace: &Path,
        storage: Option<Arc<dyn StoragePort>>,
    ) -> StatusFixture {
        retro_fixture_with(
            workspace,
            SemanticMessageType::RetroRequest,
            storage,
            Some(RetroPreview {
                contribution: stored(),
                nothing_in_window: false,
            }),
        )
        .await
    }

    fn failing_storage(workspace: &Path) -> Arc<FailNextSave> {
        Arc::new(FailNextSave {
            inner: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(workspace),
                workspace.to_path_buf(),
            )),
            fail: AtomicBool::new(false),
        })
    }

    async fn request(
        fixture: &mut StatusFixture,
        item_id: &str,
        leave_out: &[RetroField],
        decline: bool,
        mode: AttachMode,
        tier: ConnectionTier,
    ) -> RecipientActionOutcome {
        fixture
            .server
            .handle_client_frame_tiered(
                ClientFrame::ShareRecipientRetro {
                    item_id: item_id.to_owned(),
                    leave_out: leave_out.to_vec(),
                    decline,
                },
                mode,
                tier,
                1,
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let DaemonFrame::RecipientActionResult {
                    action: RecipientAction::ShareRetro,
                    target: RecipientActionTarget::Item { .. },
                    outcome,
                } = fixture
                    .frames
                    .recv()
                    .await
                    .expect("the retrospective result queue stays open")
                {
                    return outcome;
                }
            }
        })
        .await
        .expect("the retrospective act returns a correlated result")
    }

    async fn share(
        fixture: &mut StatusFixture,
        leave_out: &[RetroField],
        decline: bool,
    ) -> RecipientActionOutcome {
        request(
            fixture,
            ITEM_ID,
            leave_out,
            decline,
            AttachMode::ReadWrite,
            ConnectionTier::TrustedLocal,
        )
        .await
    }

    async fn retro_turns(fixture: &StatusFixture) -> Vec<RoomEvent> {
        fixture
            .journal
            .events
            .lock()
            .await
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RoomEvent::RecipientItemReplied {
                        kind: ReplyKind::Retro,
                        ..
                    }
                )
            })
            .cloned()
            .collect()
    }

    async fn rows(fixture: &StatusFixture) -> usize {
        fixture.journal.events.lock().await.len()
    }

    fn applied_retro(outcome: &RecipientActionOutcome) -> bool {
        matches!(
            outcome,
            RecipientActionOutcome::Applied {
                reply: Some(RecipientReplyReceipt {
                    kind: ReplyKind::Retro,
                    ..
                }),
            }
        )
    }

    async fn assert_settled_card(fixture: &StatusFixture) {
        let row_id = row_id(fixture);
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
            assert_eq!(row.content, RETRO_REPLY_TEXT);
            assert_eq!(row.authorship, Authorship::AgentComposed);
            assert_eq!(row.send_authorization, SendAuthorization::HumanDecided);
        }
    }

    #[tokio::test]
    async fn retro_share_gates_in_order_and_every_refusal_writes_zero_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = retro_fixture(tmp.path(), None).await;
        for decline in [false, true] {
            let before = rows(&fixture).await;
            assert_eq!(
                refused(
                    request(
                        &mut fixture,
                        "ri_unknown",
                        &[],
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
                    request(
                        &mut fixture,
                        "ri_unknown",
                        &[],
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
            assert_eq!(rows(&fixture).await, before);
        }
        for case in [
            "missing",
            "removed",
            "type",
            "preview",
            "decline-with-fields",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let message_type = if case == "type" {
                SemanticMessageType::Consultation
            } else {
                SemanticMessageType::RetroRequest
            };
            let preview = (case != "preview").then(|| RetroPreview {
                contribution: stored(),
                nothing_in_window: false,
            });
            let mut fixture = retro_fixture_with(tmp.path(), message_type, None, preview).await;
            if case == "removed" {
                fixture
                    .journal
                    .record_event(RoomEvent::RecipientItemRemoved {
                        address: fixture.address.clone(),
                    })
                    .await
                    .unwrap();
            }
            let before = rows(&fixture).await;
            let (item_id, expected) = match case {
                "missing" => ("ri_unknown", "recipient item not found"),
                "removed" => (ITEM_ID, "recipient item already removed"),
                "type" => (
                    ITEM_ID,
                    "retrospective replies answer retro_request items only",
                ),
                "preview" => (
                    ITEM_ID,
                    "no retrospective summary was prepared for this item",
                ),
                _ => (ITEM_ID, "decline takes no field list"),
            };
            // Each earlier gate wins over a decline that also lists fields.
            for (leave_out, decline) in [
                (&[][..], false),
                (&[][..], true),
                (&[RetroField::ReplyTimes][..], true),
            ] {
                if case == "decline-with-fields" && !(decline && !leave_out.is_empty()) {
                    continue;
                }
                assert_eq!(
                    refused(
                        request(
                            &mut fixture,
                            item_id,
                            leave_out,
                            decline,
                            AttachMode::ReadWrite,
                            ConnectionTier::TrustedLocal,
                        )
                        .await
                    ),
                    ProtocolError::Malformed(expected.to_owned()),
                    "{case}",
                );
                assert_eq!(rows(&fixture).await, before, "{case}");
                assert_eq!(*fixture.status.borrow(), NodeState::Waiting, "{case}");
            }
        }
    }

    #[tokio::test]
    async fn retro_share_with_redactions_records_the_stored_snapshot_minus_left_out_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = retro_fixture(tmp.path(), None).await;
        let outcome = share(&mut fixture, &[RetroField::ReplyTimes], false).await;
        assert!(applied_retro(&outcome), "{outcome:?}");
        let turns = retro_turns(&fixture).await;
        assert_eq!(turns.len(), 1);
        let RoomEvent::RecipientItemReplied {
            content,
            status,
            retro,
            authorship,
            send_authorization,
            via_node,
            ..
        } = &turns[0]
        else {
            unreachable!()
        };
        assert_eq!(content, RETRO_REPLY_TEXT);
        assert_eq!(status, &None);
        assert_eq!(
            retro.as_ref(),
            Some(&serde_json::json!({"received": {"consultation": 3}, "ackTimes": [60]})),
        );
        assert_eq!(*authorship, Authorship::AgentComposed);
        assert_eq!(*send_authorization, SendAuthorization::HumanDecided);
        assert_eq!(via_node.as_ref(), Some(&fixture.node));
        assert_eq!(*fixture.status.borrow(), NodeState::Completed);
        let result = fixture.server.take_result(&fixture.node).await.unwrap();
        assert_eq!(result.text, RETRO_REPLY_TEXT);
        assert_eq!(result.authorship, Authorship::AgentComposed);
        assert_eq!(result.send_authorization, SendAuthorization::HumanDecided);
        assert_settled_card(&fixture).await;

        // Control: a plain Share carries every stored field.
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = retro_fixture(tmp.path(), None).await;
        assert!(applied_retro(&share(&mut fixture, &[], false).await));
        let turns = retro_turns(&fixture).await;
        let RoomEvent::RecipientItemReplied { retro, .. } = &turns[0] else {
            unreachable!()
        };
        assert_eq!(retro.as_ref(), Some(&stored().to_wire()));
    }

    #[tokio::test]
    async fn retro_decline_nothing_and_every_field_left_out_record_identical_turns() {
        let mut payloads = Vec::new();
        for (preview, leave_out, decline) in [
            (stored(), &[][..], true),
            (stored(), &RetroField::ALL[..], false),
            (RetroContribution::nothing(), &[][..], false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let nothing_in_window = preview.is_nothing();
            let mut fixture = retro_fixture_with(
                tmp.path(),
                SemanticMessageType::RetroRequest,
                None,
                Some(RetroPreview {
                    contribution: preview,
                    nothing_in_window,
                }),
            )
            .await;
            assert!(applied_retro(
                &share(&mut fixture, leave_out, decline).await
            ));
            let turns = retro_turns(&fixture).await;
            assert_eq!(turns.len(), 1);
            let RoomEvent::RecipientItemReplied {
                content,
                kind,
                status,
                retro,
                authorship,
                send_authorization,
                ..
            } = &turns[0]
            else {
                unreachable!()
            };
            assert_eq!(content, RETRO_REPLY_TEXT);
            assert_eq!(retro.as_ref(), Some(&serde_json::json!({})));
            payloads.push(
                serde_json::to_vec(&(content, kind, status, retro, authorship, send_authorization))
                    .unwrap(),
            );
        }
        assert_eq!(payloads[0], payloads[1]);
        assert_eq!(payloads[0], payloads[2]);
    }

    #[tokio::test]
    async fn retro_share_is_one_turn_per_item_and_refusals_never_mutate() {
        // A settled live card: the same share is idempotent, a different one
        // is refused, and neither writes a row.
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = retro_fixture(tmp.path(), None).await;
        assert!(applied_retro(
            &share(&mut fixture, &[RetroField::AckTimes], false).await
        ));
        let before = rows(&fixture).await;
        assert!(matches!(
            share(&mut fixture, &[RetroField::AckTimes], false).await,
            RecipientActionOutcome::Idempotent
        ));
        for (leave_out, decline) in [(&[][..], false), (&[][..], true)] {
            assert_eq!(
                refused(share(&mut fixture, leave_out, decline).await),
                ProtocolError::Malformed(
                    "a retrospective reply was already sent for this item".to_owned(),
                ),
            );
        }
        assert_eq!(rows(&fixture).await, before);
        assert_eq!(retro_turns(&fixture).await.len(), 1);
        assert_settled_card(&fixture).await;

        // A Retro turn recorded by another node or act still answers the
        // item: the live card replays only that same contribution.
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = retro_fixture(tmp.path(), None).await;
        let declined = RetroContribution::nothing();
        let turn = claim_reply_turn_id(fresh_reply_turn_candidate(), |_| false).unwrap();
        let turn_id = turn.as_str().to_owned();
        fixture
            .journal
            .record_event(RoomEvent::RecipientItemReplied {
                address: TurnAddress::answering(&fixture.address, turn),
                answers: fixture.address.item().clone(),
                content: RETRO_REPLY_TEXT.to_owned(),
                kind: ReplyKind::Retro,
                status: None,
                retro: Some(declined.to_wire()),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                via_node: None,
            })
            .await
            .unwrap();
        let before = rows(&fixture).await;
        assert_eq!(
            refused(share(&mut fixture, &[], false).await),
            ProtocolError::Internal(
                "a retrospective reply was already sent for this item".to_owned(),
            ),
        );
        assert_eq!(rows(&fixture).await, before);
        assert_eq!(*fixture.status.borrow(), NodeState::Waiting);
        assert_eq!(
            fixture
                .server
                .pending_drafts
                .phase(fixture.node.as_str())
                .await,
            Some(DraftPhase::Ready),
        );
        match share(&mut fixture, &[], true).await {
            RecipientActionOutcome::Applied {
                reply: Some(receipt),
            } => {
                assert_eq!(receipt.kind, ReplyKind::Retro);
                assert_eq!(
                    receipt.turn_id, turn_id,
                    "the one existing turn is replayed"
                );
            }
            other => panic!("expected the replayed receipt, got {other:?}"),
        }
        assert_eq!(retro_turns(&fixture).await.len(), 1);
        assert_eq!(*fixture.status.borrow(), NodeState::Completed);
    }

    #[tokio::test]
    async fn retro_share_retry_after_append_before_card_save_completes_once() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = failing_storage(tmp.path());
        let mut fixture = retro_fixture(tmp.path(), Some(storage.clone())).await;
        storage.fail.store(true, Ordering::SeqCst);
        assert!(matches!(
            refused(share(&mut fixture, &[RetroField::Received], false).await),
            ProtocolError::Internal(error) if error.contains("injected card persistence failure"),
        ));
        assert!(
            !storage.fail.load(Ordering::SeqCst),
            "the card save reached the injected failure"
        );
        assert_eq!(
            retro_turns(&fixture).await.len(),
            1,
            "the turn was already durable"
        );
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
        let row_id = row_id(&fixture);
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
        let before_conflict = rows(&fixture).await;
        assert_eq!(
            refused(share(&mut fixture, &[], true).await),
            ProtocolError::Internal(
                "a retrospective reply was already sent for this item".to_owned(),
            ),
        );
        assert_eq!(rows(&fixture).await, before_conflict);
        assert!(applied_retro(
            &share(&mut fixture, &[RetroField::Received], false).await
        ));
        assert_eq!(
            retro_turns(&fixture).await.len(),
            1,
            "retry reuses the one Retro turn"
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
        assert_eq!(result.text, RETRO_REPLY_TEXT);
        assert_settled_card(&fixture).await;
        let before_duplicate = rows(&fixture).await;
        assert!(matches!(
            share(&mut fixture, &[RetroField::Received], false).await,
            RecipientActionOutcome::Idempotent
        ));
        assert_eq!(rows(&fixture).await, before_duplicate);
    }

    #[tokio::test]
    async fn retro_orphan_share_appends_once_and_repairs_its_card_on_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = failing_storage(tmp.path());
        let mut fixture = retro_fixture(tmp.path(), Some(storage.clone())).await;
        fixture
            .server
            .pending_drafts
            .abandon(fixture.node.as_str())
            .await;
        fixture.server.node_tree.deregister(&fixture.node).await;
        storage.fail.store(true, Ordering::SeqCst);
        assert!(applied_retro(&share(&mut fixture, &[], false).await));
        assert!(
            !storage.fail.load(Ordering::SeqCst),
            "the orphan card save reached the injected failure"
        );
        let turns = retro_turns(&fixture).await;
        assert_eq!(turns.len(), 1);
        assert!(matches!(
            &turns[0],
            RoomEvent::RecipientItemReplied { via_node: None, .. }
        ));
        let row_id = row_id(&fixture);
        assert_eq!(
            fixture.server.conversation.lock().await.peer_row_controls[&row_id].stage,
            PeerRowStage::AwaitingOperator
        );
        let before_retry = rows(&fixture).await;
        assert!(matches!(
            share(&mut fixture, &[], false).await,
            RecipientActionOutcome::Idempotent
        ));
        assert_eq!(rows(&fixture).await, before_retry);
        assert_settled_card(&fixture).await;
        assert!(fixture.server.take_result(&fixture.node).await.is_none());
        assert_eq!(
            refused(share(&mut fixture, &[], true).await),
            ProtocolError::Malformed(
                "a retrospective reply was already sent for this item".to_owned(),
            ),
        );
        assert_eq!(rows(&fixture).await, before_retry);
        assert_eq!(retro_turns(&fixture).await.len(), 1);
    }
}

#[cfg(all(test, not(feature = "a2a")))]
#[tokio::test]
async fn retro_share_without_a2a_keeps_the_reply_gate_and_never_writes_a_turn() {
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
                "retrospective sharing needs the `a2a` feature; this build was compiled without \
                 it."
                .to_owned(),
            ),
        ),
    ] {
        server
            .handle_client_frame_tiered(
                ClientFrame::ShareRecipientRetro {
                    item_id: "ri_unknown".to_owned(),
                    leave_out: Vec::new(),
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
                action: RecipientAction::ShareRetro,
                outcome: RecipientActionOutcome::Refused { error }, ..
            } if error == expected,
        ));
        assert!(journal.events.lock().await.is_empty());
    }
}
