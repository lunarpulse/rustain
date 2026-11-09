//! Host-side artifact loading and journal projection for elicitation tickets.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::adapters::artifact::FileSystemArtifactStore;
use crate::adapters::tui::decision_queue::{self, Responder, TicketPayload};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{AgentId, ArtifactId, RoomEvent, TicketResolution};
use crate::domain::ports::ArtifactStore;
use crate::infrastructure::transparency_awareness::LogAwarenessObserver;

pub(crate) struct TicketBridge {
    store: FileSystemArtifactStore,
    seen: HashSet<(AgentId, ArtifactId)>,
    projected: HashMap<(AgentId, ArtifactId), u64>,
    resolved: HashSet<(AgentId, ArtifactId)>,
    event_assignments: HashSet<(AgentId, ArtifactId)>,
    folds: u64,
}

impl TicketBridge {
    pub(crate) fn new(workspace: &Path) -> Self {
        Self {
            store: FileSystemArtifactStore::new(workspace),
            seen: HashSet::new(),
            projected: HashMap::new(),
            resolved: HashSet::new(),
            event_assignments: HashSet::new(),
            folds: 0,
        }
    }

    async fn assign(
        &mut self,
        state: &mut TuiState,
        node: &AgentId,
        artifact: &ArtifactId,
        producer: &AgentId,
        seq: u64,
        time: i64,
        responder: Responder,
    ) -> bool {
        let body = match self.store.get(artifact).await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%artifact, %error, "cannot read ticket artifact");
                return false;
            }
        };
        let envelope = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%artifact, %error, "invalid ticket artifact");
                return false;
            }
        };
        match TicketPayload::new(
            node.clone(),
            artifact.clone(),
            producer.clone(),
            seq,
            time,
            envelope,
        ) {
            Ok(payload) => {
                decision_queue::raise_ticket(state, payload, responder);
                self.seen.insert((node.clone(), artifact.clone()));
                true
            }
            Err(error) => {
                tracing::warn!(%artifact, %error, "invalid ticket envelope");
                false
            }
        }
    }

    pub(crate) async fn event(
        &mut self,
        state: &mut TuiState,
        event: &RoomEvent,
        #[cfg(feature = "mcp")] runtimes: &[std::sync::Arc<
            crate::adapters::mcp::task_driver::McpTaskRuntime,
        >],
    ) {
        match event {
            RoomEvent::TicketAssigned { node, artifact, .. } => {
                match self.store.head(artifact).await {
                    Ok(meta) => {
                        if self
                            .assign(
                                state,
                                node,
                                artifact,
                                &meta.producer,
                                0,
                                0,
                                Responder::Local,
                            )
                            .await
                        {
                            self.resolved.remove(&(node.clone(), artifact.clone()));
                            self.event_assignments
                                .insert((node.clone(), artifact.clone()));
                            #[cfg(feature = "mcp")]
                            let live = {
                                let mut found = false;
                                for runtime in runtimes {
                                    if runtime.live_tasks().await.contains(node) {
                                        found = true;
                                        break;
                                    }
                                }
                                found
                            };
                            #[cfg(not(feature = "mcp"))]
                            let live = false;
                            if !live {
                                decision_queue::resolve_ticket(state, node, artifact, true);
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%artifact, %error, "cannot read ticket metadata"),
                }
            }
            RoomEvent::TicketResolved {
                node,
                artifact,
                outcome,
            } => {
                self.resolved.insert((node.clone(), artifact.clone()));
                self.event_assignments
                    .remove(&(node.clone(), artifact.clone()));
                decision_queue::resolve_ticket(
                    state,
                    node,
                    artifact,
                    matches!(outcome, TicketResolution::ExpiredUnanswered { .. }),
                );
            }
            _ => {}
        }
    }

    pub(crate) async fn observe(
        &mut self,
        state: &mut TuiState,
        observer: &LogAwarenessObserver,
        responder: Responder,
        read_only: bool,
        #[cfg(feature = "mcp")] runtimes: &[std::sync::Arc<
            crate::adapters::mcp::task_driver::McpTaskRuntime,
        >],
    ) {
        if read_only || self.folds == observer.full_reads() {
            return;
        }
        self.folds = observer.full_reads();
        for ticket in observer.tickets() {
            let identity = (ticket.node.clone(), ticket.artifact.clone());
            if let Some(outcome) = &ticket.outcome {
                if self.seen.contains(&identity) && !self.event_assignments.contains(&identity) {
                    decision_queue::resolve_ticket(
                        state,
                        &ticket.node,
                        &ticket.artifact,
                        matches!(outcome, TicketResolution::ExpiredUnanswered { .. }),
                    );
                }
                continue;
            }
            if self
                .projected
                .get(&identity)
                .is_some_and(|seq| *seq >= ticket.seq)
                || (self.resolved.contains(&identity) && !self.projected.contains_key(&identity))
            {
                continue;
            }
            // A live answer can settle before the observer's older fold lands.
            // Do not resurrect a ticket that this host has already retired.
            if self.seen.contains(&identity)
                && !self.projected.contains_key(&identity)
                && !state.decisions.items().any(|item| {
                    matches!(&item.subject, decision_queue::DecisionSubject::Ticket(payload)
                    if payload.node == ticket.node && payload.artifact == ticket.artifact)
                })
            {
                self.projected.insert(identity, ticket.seq);
                continue;
            }
            if !self
                .assign(
                    state,
                    &ticket.node,
                    &ticket.artifact,
                    &ticket.producer,
                    ticket.seq,
                    ticket.recorded_at_ms,
                    responder,
                )
                .await
            {
                continue;
            }
            self.resolved.remove(&identity);
            self.event_assignments.remove(&identity);
            self.projected.insert(identity, ticket.seq);
            if responder == Responder::Local {
                #[cfg(feature = "mcp")]
                let live = {
                    let mut found = false;
                    for runtime in runtimes {
                        if runtime.live_tasks().await.contains(&ticket.node) {
                            found = true;
                            break;
                        }
                    }
                    found
                };
                #[cfg(not(feature = "mcp"))]
                let live = false;
                if !live {
                    decision_queue::resolve_ticket(state, &ticket.node, &ticket.artifact, true);
                }
            }
        }
    }
}

/// Encode the same inputResponses map for both runtime and daemon routes.
pub(crate) fn response(
    payload: &TicketPayload,
    effect: decision_queue::DecisionEffect,
) -> Result<serde_json::Value, String> {
    use decision_queue::DecisionEffect;
    let answer = match effect {
        DecisionEffect::TicketAccept => serde_json::json!({
            "action": "accept", "content": payload.answer()?
        }),
        DecisionEffect::TicketDecline => serde_json::json!({ "action": "decline" }),
        DecisionEffect::TicketCancel => serde_json::json!({ "action": "cancel" }),
        _ => return Err("not a ticket answer".to_owned()),
    };
    let mut responses = serde_json::Map::new();
    responses.insert(payload.key.clone(), answer);
    Ok(serde_json::Value::Object(responses))
}

pub(crate) async fn send_local(
    state: &mut TuiState,
    facts: &crate::adapters::tui::notification_log::LogFacts,
    #[cfg(feature = "mcp")] runtimes: &[std::sync::Arc<
        crate::adapters::mcp::task_driver::McpTaskRuntime,
    >],
) {
    let Some((key, effect, decision_queue::DecisionSubject::Ticket(payload))) =
        state.decisions.take_sending()
    else {
        return;
    };
    let responses = match response(&payload, effect) {
        Ok(responses) => responses,
        Err(error) => {
            decision_queue::set_ticket_error(state, &key, error);
            return;
        }
    };
    #[cfg(feature = "mcp")]
    {
        let responses = match serde_json::from_value(responses) {
            Ok(responses) => responses,
            Err(error) => {
                decision_queue::set_ticket_error(state, &key, format!("{error}"));
                return;
            }
        };
        let answer = crate::adapters::mcp::task_driver::InputAnswer { responses };
        for runtime in runtimes {
            match runtime.submit_answer(&payload.node, answer.clone()).await {
                Ok(()) => {
                    decision_queue::settle_sent(state, facts, &key);
                    return;
                }
                Err(crate::adapters::mcp::task_driver::AnswerRoutingError::UnknownNode) => {}
                Err(error) => {
                    decision_queue::set_ticket_error(state, &key, error.to_string());
                    return;
                }
            }
        }
    }
    #[cfg(not(feature = "mcp"))]
    let _ = (responses, facts);
    decision_queue::resolve_ticket(state, &payload.node, &payload.artifact, true);
}

#[cfg(test)]
mod k13_ticket {
    use super::*;
    use crate::adapters::tui::decision_queue::DecisionEffect;
    use crate::domain::models::ContentHash;

    #[test]
    fn decline_and_cancel_send_distinct_actions() {
        let node = AgentId::parse("task-node").unwrap();
        let payload = TicketPayload::new(
            node.clone(),
            ArtifactId::from(ContentHash::from_bytes([1; 32])),
            node,
            1,
            0,
            serde_json::json!({"key": "request-1", "request": {}}),
        )
        .unwrap();
        assert_eq!(
            response(&payload, DecisionEffect::TicketDecline).unwrap(),
            serde_json::json!({"request-1": {"action": "decline"}})
        );
        assert_eq!(
            response(&payload, DecisionEffect::TicketCancel).unwrap(),
            serde_json::json!({"request-1": {"action": "cancel"}})
        );
    }

    #[tokio::test]
    async fn story21_6_bridge_resolution_and_dead_replay_retain_distinct_stale_copy() {
        use crate::adapters::tui::decision_queue::{MISSED_TICKET, NO_LONGER_PENDING, Validity};
        use crate::adapters::tui::state::LogAwarenessView;
        use crate::domain::models::{
            ArtifactKind, CapabilityTokenId, EvidenceArtifactDraft, HostBinding,
        };
        use crate::infrastructure::subagent::NodeJournal;

        let workspace = tempfile::tempdir().unwrap();
        let node = AgentId::parse("task-node").unwrap();
        let store = FileSystemArtifactStore::new(workspace.path());
        let artifact = store.put(EvidenceArtifactDraft {
            kind: ArtifactKind::InputRequest,
            producer: node.clone(),
            authority: CapabilityTokenId::default(),
            provenance: vec![],
            depends_on: vec![],
            review: None,
            host: HostBinding::new("host", "workspace"),
        }, br#"{"key":"r","request":{"params":{"message":"Question","requestedSchema":{"type":"boolean"}}}}"#).await.unwrap();
        let assigned = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.id.clone(),
            to: None,
        };
        let journal = NodeJournal::open_workspace(workspace.path()).await.unwrap();
        journal
            .append_room(RoomEvent::ArtifactCreated {
                artifact: artifact.clone(),
            })
            .await
            .unwrap();
        journal.append_room(assigned.clone()).await.unwrap();

        let mut bridge = TicketBridge::new(workspace.path());
        let mut state = TuiState::new(80, 24);
        bridge
            .event(
                &mut state,
                &assigned,
                #[cfg(feature = "mcp")]
                &[],
            )
            .await;
        assert_eq!(
            state.decisions.items().next().unwrap().validity,
            Validity::Stale(MISSED_TICKET)
        );
        bridge
            .event(
                &mut state,
                &RoomEvent::TicketResolved {
                    node: node.clone(),
                    artifact: artifact.id.clone(),
                    outcome: TicketResolution::Answered,
                },
                #[cfg(feature = "mcp")]
                &[],
            )
            .await;
        assert_eq!(
            state.decisions.items().next().unwrap().validity,
            Validity::Stale(NO_LONGER_PENDING)
        );
        assert_eq!(decision_queue::waiting(&state), 1);
        let facts = crate::adapters::tui::notification_log::LogFacts::default();
        decision_queue::open_waiting(&mut state, &facts);
        decision_queue::handle_list_key(
            &mut state,
            crate::adapters::tui::keymap::Host::Local,
            crate::adapters::tui::keymap::Key::Enter,
            &facts,
        );
        assert_eq!(
            decision_queue::handle_key(&mut state, decision_queue::PaneKey::Enter, &facts),
            decision_queue::PaneOutcome::Consumed
        );
        assert_eq!(decision_queue::waiting(&state), 1);

        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        let mut view = LogAwarenessView::default();
        observer.tick(&mut view);
        observer.settle(&mut view).await;
        let mut cold = TicketBridge::new(workspace.path());
        let mut replay = TuiState::new(80, 24);
        cold.observe(
            &mut replay,
            &observer,
            Responder::Local,
            false,
            #[cfg(feature = "mcp")]
            &[],
        )
        .await;
        assert_eq!(
            replay.decisions.items().next().unwrap().validity,
            Validity::Stale(MISSED_TICKET)
        );
        assert_eq!(decision_queue::waiting(&replay), 1);
        let mut attached = TicketBridge::new(workspace.path());
        let mut attached_state = TuiState::new(80, 24);
        attached
            .observe(
                &mut attached_state,
                &observer,
                Responder::Attached,
                false,
                #[cfg(feature = "mcp")]
                &[],
            )
            .await;
        assert_eq!(
            attached_state.decisions.items().next().unwrap().validity,
            Validity::Live
        );

        let mut uncomposed = TuiState::new(80, 24);
        let mut once = TicketBridge::new(workspace.path());
        super::super::transparency_bridge::replay_uncomposed_tickets(
            workspace.path(),
            &mut uncomposed,
            &mut once,
            #[cfg(feature = "mcp")]
            &[],
        )
        .await;
        assert_eq!(decision_queue::waiting(&uncomposed), 1);
        let mut disabled = LogAwarenessObserver::disabled();
        journal
            .append_room(RoomEvent::TicketResolved {
                node: node.clone(),
                artifact: artifact.id.clone(),
                outcome: TicketResolution::Answered,
            })
            .await
            .unwrap();
        for seconds in [1, 5, 60] {
            disabled.tick_at(
                std::time::Instant::now() + std::time::Duration::from_secs(seconds),
                &mut view,
            );
            disabled.settle(&mut view).await;
        }
        assert_eq!((disabled.jobs_started(), disabled.full_reads()), (0, 0));
        assert_eq!(decision_queue::waiting(&uncomposed), 1);
        let now = std::time::Instant::now();
        observer.tick_at(now + std::time::Duration::from_secs(2), &mut view);
        observer.settle(&mut view).await;
        attached
            .observe(
                &mut attached_state,
                &observer,
                Responder::Attached,
                false,
                #[cfg(feature = "mcp")]
                &[],
            )
            .await;
        assert_eq!(
            attached_state.decisions.items().next().unwrap().validity,
            Validity::Stale(NO_LONGER_PENDING)
        );
        journal.append_room(assigned).await.unwrap();
        observer.tick_at(now + std::time::Duration::from_secs(4), &mut view);
        observer.settle(&mut view).await;
        attached
            .observe(
                &mut attached_state,
                &observer,
                Responder::Attached,
                false,
                #[cfg(feature = "mcp")]
                &[],
            )
            .await;
        assert_eq!(
            attached_state.decisions.items().next().unwrap().validity,
            Validity::Live
        );
        let mut restarted = TicketBridge::new(workspace.path());
        let mut restarted_state = TuiState::new(80, 24);
        restarted
            .observe(
                &mut restarted_state,
                &observer,
                Responder::Attached,
                false,
                #[cfg(feature = "mcp")]
                &[],
            )
            .await;
        assert_eq!(
            restarted_state.decisions.items().next().unwrap().validity,
            Validity::Live
        );
    }
}
