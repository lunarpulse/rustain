//! Sender-local, memory-only proposals. A surface is a currently authenticated
//! trusted writer, not a generic permission/approval request or a bus subscriber.

use std::collections::BTreeMap;
use std::sync::Arc;
#[cfg(any(test, feature = "test-instrumentation"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::{Mutex, MutexGuard, mpsc, watch};

use super::protocol::{DaemonFrame, ProtocolError, TeamProposalCloseOutcome};
use crate::domain::models::{ProposalRaiseOutcome, TeamProposal};
use crate::domain::ports::TeamProposalSink;

#[cfg(any(test, feature = "a2a"))]
#[derive(Debug)]
pub(crate) enum ProposalTakeError {
    Unknown,
    Busy,
}

struct Writer {
    connection: u64,
    tx: mpsc::Sender<DaemonFrame>,
    terminal: watch::Sender<Option<ProtocolError>>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    hooks: Option<super::server::TeamSendTestHooks>,
}

#[derive(Default)]
struct State {
    writer: Option<Writer>,
    pending: BTreeMap<String, Arc<TeamProposal>>,
    closed: bool,
}

/// One registry is constructed by the daemon composition root and shared with
/// its lazy egress/tool factory and attach server. There is deliberately no
/// persistence, background task, timer, permission runtime, or model callback.
#[derive(Default)]
pub struct TeamProposalRegistry {
    state: Mutex<State>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    acquisitions: AtomicUsize,
}

impl TeamProposalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    async fn lock_state(&self) -> MutexGuard<'_, State> {
        let guard = self.state.lock().await;
        #[cfg(any(test, feature = "test-instrumentation"))]
        self.acquisitions.fetch_add(1, Ordering::AcqRel);
        guard
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn lock_acquisitions(&self) -> usize {
        self.acquisitions.load(Ordering::Acquire)
    }

    /// Observational snapshot; no private proposal is published onto EventBus.
    pub async fn pending(&self) -> Vec<Arc<TeamProposal>> {
        self.lock_state().await.pending.values().cloned().collect()
    }

    /// Exactly-once snapshot consumption, under exactly one state acquisition.
    /// Callers acquire send capacity before this destructive operation.
    pub async fn take(&self, id: &str) -> Option<Arc<TeamProposal>> {
        let mut state = self.lock_state().await;
        state.pending.remove(id)
    }

    #[cfg(any(test, feature = "a2a"))]
    /// Check identity, admit capacity, and consume under one acquisition. An
    /// unknown identity is refused even while all eight send slots are busy.
    pub(crate) async fn take_for_send(
        &self,
        id: &str,
        slots: Arc<tokio::sync::Semaphore>,
    ) -> Result<(Arc<TeamProposal>, tokio::sync::OwnedSemaphorePermit), ProposalTakeError> {
        let mut state = self.lock_state().await;
        if !state.pending.contains_key(id) {
            return Err(ProposalTakeError::Unknown);
        }
        let permit = slots
            .try_acquire_owned()
            .map_err(|_| ProposalTakeError::Busy)?;
        let proposal = state
            .pending
            .remove(id)
            .expect("proposal checked under the same lock");
        Ok((proposal, permit))
    }
    #[cfg(feature = "a2a")]
    /// Re-present a retained proposal after a correlated non-destructive refusal.
    pub(crate) async fn reemit(&self, id: &str, connection: u64) {
        let state = self.lock_state().await;
        if let Some(writer) = state
            .writer
            .as_ref()
            .filter(|writer| writer.connection == connection)
            && let Some(proposal) = state.pending.values().find(|proposal| proposal.id == id)
        {
            publish(writer, proposal_frame(proposal));
        }
    }

    /// Only the signed TrustedLocal + ReadWrite grant calls this. Re-emission
    /// and surface replacement are atomic with respect to a concurrent raise.
    pub(crate) async fn grant_writer(
        &self,
        connection: u64,
        tx: mpsc::Sender<DaemonFrame>,
        terminal: watch::Sender<Option<ProtocolError>>,
        #[cfg(any(test, feature = "test-instrumentation"))] hooks: Option<
            super::server::TeamSendTestHooks,
        >,
    ) {
        let mut state = self.lock_state().await;
        if state.closed {
            return;
        }
        let writer = Writer {
            connection,
            tx,
            terminal,
            #[cfg(any(test, feature = "test-instrumentation"))]
            hooks,
        };
        for proposal in state.pending.values() {
            if !publish(&writer, proposal_frame(proposal)) {
                break;
            }
        }
        state.writer = Some(writer);
    }

    /// A late detach from the old writer cannot clear a replacement's surface.
    pub(crate) async fn detach_writer(&self, connection: u64) {
        let mut state = self.lock_state().await;
        if state
            .writer
            .as_ref()
            .is_some_and(|writer| writer.connection == connection)
        {
            state.writer = None;
        }
    }

    /// Conversation reset/close withdraws, never retargets, the immutable body.
    pub async fn withdraw_conversation(&self, conversation: &str) -> Vec<Arc<TeamProposal>> {
        let mut state = self.lock_state().await;
        let mut withdrawn = None;
        state.pending.retain(|_, proposal| {
            if proposal.conversation_id == conversation {
                withdrawn = Some(proposal.clone());
                false
            } else {
                true
            }
        });
        if let (Some(proposal), Some(writer)) = (withdrawn.as_ref(), state.writer.as_ref()) {
            publish(
                writer,
                DaemonFrame::TeamProposalClosed {
                    proposal: proposal.id.clone(),
                    outcome: TeamProposalCloseOutcome::Withdrawn,
                },
            );
        }
        withdrawn.into_iter().collect()
    }

    pub(crate) async fn shutdown(&self) {
        let mut state = self.lock_state().await;
        state.pending.clear();
        state.writer = None;
        state.closed = true;
    }
}

fn proposal_frame(proposal: &TeamProposal) -> DaemonFrame {
    DaemonFrame::TeamProposal {
        proposal: proposal.id.clone(),
        recipients: proposal.recipients.clone(),
        message_type: proposal.message_type,
        body: proposal.body.clone(),
    }
}

fn publish(writer: &Writer, frame: DaemonFrame) -> bool {
    super::server::publish_team_frame_to_connection(
        &writer.tx,
        &writer.terminal,
        frame,
        writer.connection,
        #[cfg(any(test, feature = "test-instrumentation"))]
        writer.hooks.as_ref(),
    )
}

#[async_trait]
impl TeamProposalSink for TeamProposalRegistry {
    async fn raise(&self, proposal: Arc<TeamProposal>) -> ProposalRaiseOutcome {
        let mut state = self.lock_state().await;
        if state.closed {
            return ProposalRaiseOutcome::Unavailable;
        }
        let Some(writer) = state
            .writer
            .as_ref()
            .filter(|writer| !writer.tx.is_closed() && writer.terminal.borrow().is_none())
        else {
            return ProposalRaiseOutcome::Unavailable;
        };
        if state
            .pending
            .values()
            .any(|pending| pending.conversation_id == proposal.conversation_id)
        {
            return ProposalRaiseOutcome::Pending;
        }
        // On queue overflow the terminal lane explicitly closes this writer;
        // retain the proposal for the next actual trusted writer grant.
        publish(writer, proposal_frame(&proposal));
        state.pending.insert(proposal.id.clone(), proposal);
        ProposalRaiseOutcome::Shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SemanticMessageType;

    fn proposal(id: &str, conversation: &str) -> Arc<TeamProposal> {
        Arc::new(TeamProposal {
            id: id.into(),
            conversation_id: conversation.into(),
            recipients: vec!["lena-po".into(), "jun-dev".into()],
            message_type: SemanticMessageType::ArchitectureUpdate,
            body: "exact bytes\n\t한글 👩🏽‍💻".into(),
        })
    }

    #[tokio::test]
    async fn every_registry_mutator_acquires_exactly_one_state_lock() {
        let registry = TeamProposalRegistry::new();
        let (tx, _rx) = mpsc::channel(16);
        let (terminal, _terminal_rx) = watch::channel(None);
        macro_rules! once {
            ($operation:expr) => {{
                let before = registry.lock_acquisitions();
                let result = $operation.await;
                assert_eq!(
                    registry.lock_acquisitions() - before,
                    1,
                    "K04 each proposal mutator must acquire the state lock exactly once"
                );
                result
            }};
        }
        once!(registry.grant_writer(1, tx, terminal, None));
        assert_eq!(
            once!(registry.raise(proposal("tp_a", "a"))),
            ProposalRaiseOutcome::Shown
        );
        assert_eq!(
            once!(registry.raise(proposal("tp_duplicate", "a"))),
            ProposalRaiseOutcome::Pending
        );
        assert_eq!(
            once!(registry.raise(proposal("tp_b", "b"))),
            ProposalRaiseOutcome::Shown
        );
        assert_eq!(once!(registry.take("tp_a")).unwrap().id, "tp_a");
        assert!(once!(registry.take("tp_a")).is_none());
        assert_eq!(once!(registry.withdraw_conversation("b"))[0].id, "tp_b");
        assert_eq!(
            once!(registry.raise(proposal("tp_c", "c"))),
            ProposalRaiseOutcome::Shown
        );
        let slots = Arc::new(tokio::sync::Semaphore::new(0));
        assert!(matches!(
            once!(registry.take_for_send("tp_unknown", slots.clone())),
            Err(ProposalTakeError::Unknown)
        ));
        assert!(matches!(
            once!(registry.take_for_send("tp_c", slots.clone())),
            Err(ProposalTakeError::Busy)
        ));
        slots.add_permits(1);
        let (taken, permit) = once!(registry.take_for_send("tp_c", slots)).unwrap();
        assert_eq!(taken.id, "tp_c");
        drop(permit);
        once!(registry.detach_writer(1));
        once!(registry.shutdown());
        let (tx, _late_rx) = mpsc::channel(4);
        let (terminal, _late_terminal) = watch::channel(None);
        once!(registry.grant_writer(2, tx, terminal, None));
        assert_eq!(
            once!(registry.raise(proposal("tp_after_shutdown", "d"))),
            ProposalRaiseOutcome::Unavailable
        );
        assert!(
            registry.pending().await.is_empty(),
            "shutdown must prevent a late connection from reviving proposals"
        );
    }

    #[tokio::test]
    async fn full_writer_queue_is_terminal_retains_and_reemits_exact_snapshot() {
        let registry = TeamProposalRegistry::new();
        let (tx, mut rx) = mpsc::channel(1);
        let (terminal, terminal_rx) = watch::channel(None);
        tx.try_send(DaemonFrame::Detached).unwrap();
        registry.grant_writer(1, tx, terminal, None).await;
        let snapshot = proposal("tp_retained", "a");
        assert_eq!(
            registry.raise(snapshot.clone()).await,
            ProposalRaiseOutcome::Shown
        );
        assert!(
            terminal_rx.has_changed().is_ok_and(|changed| changed),
            "K04 full proposal queue must publish an explicit terminal error"
        );
        assert!(
            matches!(&*terminal_rx.borrow(), Some(ProtocolError::Internal(reason))
            if reason == "team result queue overflow; connection closed"),
            "K04 full proposal queue must publish an explicit terminal error"
        );
        assert!(matches!(rx.try_recv(), Ok(DaemonFrame::Detached)));
        assert_eq!(
            registry.pending().await,
            vec![snapshot.clone()],
            "K04 terminal proposal overflow must retain its exact snapshot"
        );
        registry.detach_writer(1).await;
        let (tx, mut next_rx) = mpsc::channel(4);
        let (terminal, _terminal_rx) = watch::channel(None);
        registry.grant_writer(2, tx, terminal, None).await;
        assert!(
            matches!(next_rx.try_recv(),
            Ok(DaemonFrame::TeamProposal { proposal, recipients, message_type, body })
            if proposal == snapshot.id && recipients == snapshot.recipients
                && message_type == snapshot.message_type && body == snapshot.body),
            "K04 replacement writer must receive the unchanged retained proposal"
        );
        // Late cleanup of the old connection cannot revoke writer two.
        registry.detach_writer(1).await;
        assert_eq!(
            registry.raise(proposal("tp_other_conversation", "b")).await,
            ProposalRaiseOutcome::Shown
        );
        assert!(matches!(
            next_rx.try_recv(),
            Ok(DaemonFrame::TeamProposal { .. })
        ));
    }

    #[tokio::test]
    async fn no_surface_never_retains_and_reset_withdraws_without_retargeting() {
        let registry = TeamProposalRegistry::new();
        let snapshot = proposal("tp_a", "a");
        assert_eq!(
            registry.raise(snapshot.clone()).await,
            ProposalRaiseOutcome::Unavailable
        );
        assert!(
            registry.pending().await.is_empty(),
            "K01 no writer must return R2 and retain zero proposals"
        );
        let (tx, mut rx) = mpsc::channel(4);
        let (terminal, _terminal_rx) = watch::channel(None);
        registry.grant_writer(1, tx, terminal, None).await;
        assert_eq!(
            registry.raise(snapshot.clone()).await,
            ProposalRaiseOutcome::Shown
        );
        let _ = rx.recv().await;
        assert!(registry.withdraw_conversation("different").await.is_empty());
        assert_eq!(registry.pending().await, vec![snapshot.clone()]);
        assert_eq!(registry.withdraw_conversation("a").await, vec![snapshot]);
        assert!(
            matches!(rx.recv().await,
            Some(DaemonFrame::TeamProposalClosed { proposal, outcome: TeamProposalCloseOutcome::Withdrawn })
            if proposal == "tp_a"),
            "K04 reset must withdraw the original proposal identity"
        );
        assert!(registry.take("tp_a").await.is_none());
        assert!(registry.pending().await.is_empty());
    }
}

#[cfg(test)]
mod nonblocking_tests {
    use super::*;
    use futures::FutureExt;

    /// A fresh uncontended registry has no asynchronous work to wait for after
    /// publication. A decision-waiting sink is rejected on its first poll,
    /// without a deadline, fake sleep, or held-lock timing window. Socket K05
    /// separately proves actual peer and next-operator turn completion.
    #[tokio::test]
    async fn proposal_raise_completes_on_first_poll_without_waiting_for_decision() {
        let registry = TeamProposalRegistry::new();
        let (tx, mut rx) = mpsc::channel(4);
        let (terminal, _terminal_rx) = watch::channel(None);
        registry.grant_writer(1, tx, terminal, None).await;
        let snapshot = Arc::new(TeamProposal {
            id: "tp_nonblocking".into(),
            conversation_id: "a".into(),
            recipients: vec!["lena-po".into()],
            message_type: crate::domain::models::SemanticMessageType::ArchitectureUpdate,
            body: "operator must decide later".into(),
        });
        assert_eq!(
            registry.raise(snapshot.clone()).now_or_never(),
            Some(ProposalRaiseOutcome::Shown),
            "K05 proposal raise must complete on its first uncontended poll without awaiting a decision"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonFrame::TeamProposal { .. })
        ));
        assert_eq!(registry.pending().await, vec![snapshot]);
    }
}
