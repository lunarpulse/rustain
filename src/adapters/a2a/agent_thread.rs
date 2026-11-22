//! Story `agents-thread` — the owner-side binding of an Agents row to its peer exchange.
//!
//! The Agents roster knows only node ids. The A2A runtime alone knows which delegation row
//! this host dispatched, so the offer rule lives here (D2, D3): [`offer`] is I/O-free (it reads
//! the roster binding and the seeded dispatch ledger, never the network and never the
//! journal), and [`open`] performs the one scoped list read plus the one thread read of D4,
//! through the shared reader in [`super::send`]. Rendering is the host's job: this module
//! returns the structured read.

use super::driver::{A2aDelegationRuntime, mint_node_id, parse_a2a_node_id};
use super::send::{TaskThreadRead, read_thread_of_task};
use crate::domain::models::{AgentRowView, OwnershipKind, SentThreadOffer};

/// The offer of one Agents row (D3), or `None`. A row is offered iff all hold: it is a `Peer`
/// row; its id decodes to `(peer, task)`; `peer` is a roster alias; the runtime proves the row's
/// node answers the `messageId` its delegation submitted (AT-02: a peer that answered with any
/// other id — another task this host once dispatched to it included — names an exchange that is
/// not this delegation's); and `task` is a dispatch this host journaled to that peer (`PQ2`)
/// over the seeded ledger. Nothing here touches the network, the journal's write side or a
/// capability key.
pub async fn offer(runtime: &A2aDelegationRuntime, row: &AgentRowView) -> Option<SentThreadOffer> {
    if !matches!(row.ownership, OwnershipKind::Peer) {
        return None;
    }
    let (peer, task) = parse_a2a_node_id(&row.agent_id)?;
    let (spec, _) = runtime.peer_binding(&peer)?;
    if !runtime.node_answers_own_submission(&row.agent_id) {
        return None;
    }
    runtime
        .owns_task(&spec, &task)
        .await
        .then_some(SentThreadOffer { peer, task })
}

/// What [`open`] found.
#[derive(Debug)]
pub enum AgentThreadRead {
    /// The offer no longer verifies: the alias left the roster, the node does not answer this
    /// delegation's own submission, or the task is not one this host dispatched to that peer.
    /// Nothing was requested.
    Unverifiable,
    /// The peer's scoped list was read (and, for exactly one match, its thread).
    Read(TaskThreadRead),
}

/// Re-verify `offer` against the runtime as it is now, then resolve its exact exchange: one
/// scoped list read and, for exactly one item naming the task, one thread read (D4). Sends no
/// message and no task request; the only write is the `PeerReplyObserved` rows the shared
/// thread read already settles.
pub async fn open(runtime: &A2aDelegationRuntime, offer: &SentThreadOffer) -> AgentThreadRead {
    let Some((spec, _)) = runtime.peer_binding(&offer.peer) else {
        return AgentThreadRead::Unverifiable;
    };
    if !runtime.node_answers_own_submission(&mint_node_id(&offer.peer, &offer.task))
        || !runtime.owns_task(&spec, &offer.task).await
    {
        return AgentThreadRead::Unverifiable;
    }
    AgentThreadRead::Read(read_thread_of_task(runtime, &offer.peer, &offer.task).await)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::adapters::a2a::client::A2aClientAdapter;
    use crate::adapters::a2a::error::A2aError;
    use crate::adapters::a2a::lifecycle::A2aTaskTransport;
    use crate::domain::models::{
        A2aPeerSource, A2aPeerSpec, AgentId, Authorship, NodeState, RedactedUrl, RoomEvent,
        SendAuthorization, TrustTier,
    };
    use crate::domain::ports::{RoomJournal, RoomJournalError};
    use crate::infrastructure::subagent::NodeTree;

    struct Accepting;

    #[async_trait::async_trait]
    impl RoomJournal for Accepting {
        async fn record_event(&self, _event: RoomEvent) -> Result<(), RoomJournalError> {
            Ok(())
        }
    }

    /// A peer that answers `message/send` with a completed task named `.0`, whatever
    /// `messageId` it was sent.
    struct Answers(&'static str);

    #[async_trait::async_trait]
    impl A2aTaskTransport for Answers {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Ok(serde_json::json!({
                "kind": "task",
                "id": self.0,
                "contextId": "ctx",
                "status": { "state": "completed" },
            }))
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            unreachable!("a completed first answer is never polled")
        }
        async fn tasks_cancel(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            unreachable!("a completed first answer is never cancelled")
        }
    }

    fn spec(id: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            id,
            RedactedUrl::from("http://127.0.0.2:9"),
            A2aPeerSource::Workspace,
        )
    }

    fn runtime(peers: &[&A2aPeerSpec]) -> A2aDelegationRuntime {
        let (tx, _rx) = mpsc::unbounded_channel();
        let bindings: Vec<_> = peers
            .iter()
            .map(|spec| {
                (
                    (*spec).clone(),
                    Arc::new(A2aClientAdapter::new(spec, None).unwrap()),
                )
            })
            .collect();
        A2aDelegationRuntime::new(NodeTree::new(), Arc::new(Accepting), tx)
            .with_peer_bindings(bindings.into())
    }

    fn row(id: AgentId, ownership: OwnershipKind) -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: id,
            parent_id: AgentId::root(),
            subagent_type: "a2a-peer".to_owned(),
            spawned_at: 0,
            depth: 1,
            current_status: NodeState::Running,
            ownership,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        }
    }

    /// Seed the ledger (a first use): later own dispatches are kept current in it.
    async fn seed(runtime: &A2aDelegationRuntime, peer: &A2aPeerSpec) {
        assert!(!runtime.owns_task(peer, "warm-up").await);
    }

    /// A typed `/team send` of `task` to `peer`: an own dispatch with no delegation node.
    async fn typed_send(runtime: &A2aDelegationRuntime, peer: &A2aPeerSpec, task: &str) {
        runtime
            .journal_send_dispatch(
                peer,
                task,
                "hello",
                None,
                Authorship::HumanWritten,
                SendAuthorization::HumanDecided,
            )
            .await
            .unwrap();
    }

    /// One model delegation submitting `submitted` to `peer`, which answers with task id
    /// `answered`. Returns whether it completed and the Peer row id it materializes.
    async fn delegate(
        runtime: &A2aDelegationRuntime,
        peer: &A2aPeerSpec,
        submitted: &str,
        answered: &'static str,
    ) -> (bool, AgentId) {
        let done = runtime
            .delegate(
                peer,
                TrustTier::Unverified,
                "call-1",
                Arc::new(Answers(answered)),
                serde_json::json!({
                    "message": {
                        "messageId": submitted,
                        "parts": [{ "kind": "text", "text": "hello" }]
                    }
                }),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .is_ok();
        (done, mint_node_id(&peer.id, answered))
    }

    #[tokio::test]
    async fn agents_thread_offer_table_follows_the_d3_rule() {
        let marcus = spec("marcus");
        let jun = spec("jun");
        let runtime = runtime(&[&marcus]);
        seed(&runtime, &marcus).await;
        let (done, own_id) = delegate(&runtime, &marcus, "task-own", "task-own").await;
        assert!(done);

        // Own delegation row to a roster peer: offered, keyed by what the id decodes to.
        let own = row(own_id.clone(), OwnershipKind::Peer);
        assert_eq!(
            offer(&runtime, &own).await,
            Some(SentThreadOffer {
                peer: "marcus".to_owned(),
                task: "task-own".to_owned(),
            })
        );
        // The same id on an Owned row is never offered (ownership decides, not the id).
        let owned = row(own_id, OwnershipKind::Owned);
        assert_eq!(offer(&runtime, &owned).await, None);
        // An id that does not decode.
        let undecodable = row(
            AgentId::from_validated("sub-1".to_owned()),
            OwnershipKind::Peer,
        );
        assert_eq!(offer(&runtime, &undecodable).await, None);
        // A decoded row whose alias is not in the roster.
        let unknown_alias = row(mint_node_id("jun", "task-own"), OwnershipKind::Peer);
        assert!(runtime.peer_binding(&jun.id).is_none());
        assert_eq!(offer(&runtime, &unknown_alias).await, None);
        // A third-party peer that answered with a task id of its own (≠ the `messageId`).
        let (done, served) = delegate(&runtime, &marcus, "task-sent", "task-served").await;
        assert!(done);
        assert_eq!(
            offer(&runtime, &row(served, OwnershipKind::Peer)).await,
            None
        );
        // A task sanitization would change is never an own dispatch.
        let hostile = row(
            mint_node_id("marcus", "task\u{1b}[31m"),
            OwnershipKind::Peer,
        );
        assert_eq!(offer(&runtime, &hostile).await, None);
    }

    /// The own-dispatch half still decides on its own: a node proven to answer its
    /// delegation's `messageId`, whose dispatch row the seeded ledger does not hold (it was
    /// written before the seed into a journal that kept nothing), is not offered.
    #[tokio::test]
    async fn agents_thread_a_proven_node_without_an_own_dispatch_row_is_not_offered() {
        let marcus = spec("marcus");
        let runtime = runtime(&[&marcus]);
        let (done, id) = delegate(&runtime, &marcus, "task-unjournaled", "task-unjournaled").await;
        assert!(done);
        assert!(runtime.node_answers_own_submission(&id));
        assert_eq!(offer(&runtime, &row(id, OwnershipKind::Peer)).await, None);
    }

    /// AT-02 — a peer replays a previous valid own task: an earlier typed `/team send` to
    /// marcus dispatched `task-typed` (no delegation node); a later delegation submits
    /// `task-delegated` and marcus answers with `task-typed`. The row's task IS an own
    /// dispatch to marcus, yet it is not this delegation's exchange: no offer, and an offer for
    /// it opens nothing. A peer re-using a node id an earlier delegation proved revokes it.
    #[tokio::test]
    async fn agents_thread_a_replayed_own_task_is_never_offered_or_opened() {
        let marcus = spec("marcus");
        let runtime = runtime(&[&marcus]);
        seed(&runtime, &marcus).await;
        typed_send(&runtime, &marcus, "task-typed").await;
        let (done, replayed) = delegate(&runtime, &marcus, "task-delegated", "task-typed").await;
        assert!(done);
        assert!(
            runtime.owns_task(&marcus, "task-typed").await,
            "the replayed task is a valid own dispatch"
        );
        assert_eq!(
            offer(&runtime, &row(replayed, OwnershipKind::Peer)).await,
            None
        );
        // The peer URL is unroutable: reaching the network would be a `Read(Failed(..))`.
        let replay = SentThreadOffer {
            peer: "marcus".to_owned(),
            task: "task-typed".to_owned(),
        };
        assert!(matches!(
            open(&runtime, &replay).await,
            AgentThreadRead::Unverifiable
        ));

        // An earlier delegation's proven node, then a later delegation answered with its id.
        let (done, first) = delegate(&runtime, &marcus, "task-first", "task-first").await;
        assert!(done);
        let first_row = row(first.clone(), OwnershipKind::Peer);
        assert!(offer(&runtime, &first_row).await.is_some());
        let (_, reused) = delegate(&runtime, &marcus, "task-later", "task-first").await;
        assert_eq!(reused, first);
        assert_eq!(offer(&runtime, &first_row).await, None, "fails closed");
    }

    #[tokio::test]
    async fn agents_thread_open_of_an_unverifiable_offer_requests_nothing() {
        let marcus = spec("marcus");
        let runtime = runtime(&[&marcus]);
        seed(&runtime, &marcus).await;
        let (done, _) = delegate(&runtime, &marcus, "task-own", "task-own").await;
        assert!(done);
        let stale = |peer: &str, task: &str| SentThreadOffer {
            peer: peer.to_owned(),
            task: task.to_owned(),
        };
        // Alias removed and not-own both fail before any transport is resolved: the peer URL
        // is unroutable, so reaching the network would surface as a `Read(Failed(..))`.
        assert!(matches!(
            open(&runtime, &stale("gone", "task-own")).await,
            AgentThreadRead::Unverifiable
        ));
        assert!(matches!(
            open(&runtime, &stale("marcus", "task-served")).await,
            AgentThreadRead::Unverifiable
        ));
    }
}
