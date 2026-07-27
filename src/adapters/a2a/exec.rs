//! Inbound-task bookkeeping: the map of tasks this server accepted, their
//! submitter scoping, their cancellation tokens, and the result-direction
//! projection from the executed node's lifecycle back onto the A2A task FSM.
//!
//! Story 18.1b, AC1b / AC5b / AC6b.
//!
//! Everything here is `tokio::sync` — the async-lock policy is a ratchet
//! (`MAX_KNOWN_STD_SYNC_LOCKS`), and this module holds state touched from inside
//! an axum handler and from a spawned lifecycle watcher at the same time.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::domain::models::{AgentId, NodeState, PeerId, RapTaskState};

/// Whether a task is waiting on a human before it may run.
///
/// `NodeState` has no `AuthRequired`, and correctly so: at the moment approval
/// is outstanding **no node exists yet** — registering one would be a core
/// mutation performed before authority was granted (NFR70). Pending-approval is
/// therefore task-level bookkeeping, and it is carried into the projection as
/// this flag so that the one function which decides the wire state can express
/// every state the wire has.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAuth {
    /// No approval outstanding.
    None,
    /// A human decision is outstanding.
    Approval,
}

/// Result-direction projection: an executed node's coarse lifecycle onto the A2A
/// task FSM.
///
/// The inverse of `driver::project_rap_to_node_state`, and **lossy by
/// construction** in both directions:
///
/// * `Waiting` and `Suspended` both collapse to `working`. A remote submitter
///   has no vocabulary for "parked awaiting a local operator's answer", and A2A
///   `input-required` would be a lie — it means *the submitter* must respond.
/// * `Created` collapses to `working` rather than `submitted`: by the time a
///   node exists we have already accepted, and going backwards on the wire is
///   not a legal `RapTaskState` transition.
/// * `AuthRequired` has no `NodeState` at all; it arrives via [`PendingAuth`].
///
/// Exhaustive over `NodeState` on purpose (the enum is `#[non_exhaustive]` only
/// for downstream crates): a new lifecycle state must break this build, not
/// silently render as `failed` to every peer on the network.
#[must_use]
pub const fn project_node_to_rap(node: NodeState, pending: PendingAuth) -> RapTaskState {
    if let PendingAuth::Approval = pending {
        // An outstanding human decision outranks whatever the node is doing —
        // in practice there is no node yet.
        return RapTaskState::AuthRequired;
    }
    match node {
        NodeState::Created | NodeState::Running | NodeState::Waiting | NodeState::Suspended => {
            RapTaskState::Working
        }
        NodeState::Completed => RapTaskState::Completed,
        NodeState::Failed => RapTaskState::Failed,
        NodeState::Cancelled => RapTaskState::Canceled,
    }
}

/// Opaque, non-reversible handle for "who submitted this task".
///
/// Derived from the presented credential rather than stored as it: a task map
/// that holds submitter API keys is a credential store nobody designed. The
/// digest is only ever compared to another digest, so a plain `==` is correct
/// here — there is no secret to leak a prefix of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubmitterKey(String);

impl SubmitterKey {
    /// The single submitter identity of the plaintext loopback socket. Loopback
    /// carries no credential, so every loopback caller is the same principal —
    /// which is accurate: they all already run code on this machine.
    #[must_use]
    pub fn loopback() -> Self {
        Self("loopback".to_owned())
    }

    /// Digest of a presented API key.
    #[must_use]
    pub fn from_api_key(key: &str) -> Self {
        Self(format!(
            "apikey:{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))
        ))
    }

    /// A stable, pseudonymous peer id for this submitting principal.
    ///
    /// The `PeerId` constructor requires 32 bytes, and SHA-256 always produces
    /// exactly that many. Deriving from the already non-secret submitter handle
    /// means authority, room records, and task scoping agree without exposing
    /// the configured credential or this host's signing identity.
    #[must_use]
    pub fn pseudonymous_peer_id(&self) -> PeerId {
        PeerId::from_public_key(&Sha256::digest(self.0.as_bytes())).unwrap()
    }

    /// The opaque handle, for encoding into a node id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rebuild a key from an encoded handle recovered after a restart.
    ///
    /// Only ever fed a value this type produced (via a node id), which is why it
    /// is not a parser: there is nothing to validate that the digest itself does
    /// not already guarantee.
    #[must_use]
    pub fn from_encoded(handle: String) -> Self {
        Self(handle)
    }
}

/// A task this server accepted.
pub struct InboundTask {
    /// The A2A task id, echoed to the submitter.
    pub id: String,
    /// The local node this task executes as. Never disclosed.
    pub node_id: AgentId,
    /// Who may `tasks/get` and `tasks/cancel` this task.
    pub submitter: SubmitterKey,
    /// The token `drive_preloaded_turn` selects on. `tasks/cancel` cancels it.
    pub cancel: CancellationToken,
    /// State of the first `tasks/get` journal record. Lives on the existing
    /// submitter-scoped record — which is already evicted on terminal — rather
    /// than in a new `(peer, task)` map that would need an eviction path.
    query_state: AtomicU8,
    /// State of the first result disclosure. Unlike status queries, disclosure
    /// is fail-closed: concurrent response paths wait for the in-flight append
    /// instead of returning content before its canonical record exists.
    disclosure_state: AtomicU8,
    disclosure_settled: Notify,
    /// Wakes a durable-cancel request once a lifecycle watcher terminalizes the
    /// task. This is task-local async coordination, never a host-thread lock.
    terminal: Notify,
    state: RwLock<TaskState>,
}

#[derive(Debug, Clone)]
struct TaskState {
    rap: RapTaskState,
    pending: PendingAuth,
    /// The agent's answer, captured once the node reaches a terminal state.
    result: Option<String>,
    /// Human-readable explanation for a non-obvious terminal state.
    detail: Option<String>,
}

/// A consistent read of a task's disclosable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub state: RapTaskState,
    pub result: Option<String>,
    pub detail: Option<String>,
}

impl InboundTask {
    const QUERY_UNSEEN: u8 = 0;
    const QUERY_RECORDING: u8 = 1;
    const QUERY_RECORDED: u8 = 2;
    const DISCLOSURE_UNSEEN: u8 = 0;
    const DISCLOSURE_RECORDING: u8 = 1;
    const DISCLOSURE_RECORDED: u8 = 2;

    fn new(id: String, node_id: AgentId, submitter: SubmitterKey, initial: RapTaskState) -> Self {
        Self {
            id,
            node_id,
            submitter,
            cancel: CancellationToken::new(),
            query_state: AtomicU8::new(Self::QUERY_UNSEEN),
            disclosure_state: AtomicU8::new(Self::DISCLOSURE_UNSEEN),
            disclosure_settled: Notify::new(),
            terminal: Notify::new(),
            state: RwLock::new(TaskState {
                rap: initial,
                pending: PendingAuth::None,
                result: None,
                detail: None,
            }),
        }
    }

    /// Claim the one in-flight attempt to record the first observed
    /// `tasks/get`. The claim remains retryable if the append fails; otherwise
    /// an outage at the first poll would erase the only observation forever.
    pub fn claim_status_query(&self) -> bool {
        self.query_state
            .compare_exchange(
                Self::QUERY_UNSEEN,
                Self::QUERY_RECORDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Commit a successfully durable first-query record.
    pub fn commit_status_query(&self) {
        self.query_state
            .store(Self::QUERY_RECORDED, Ordering::Release);
    }

    /// Release a failed record attempt so a later status query can retry it.
    pub fn release_status_query(&self) {
        let _ = self.query_state.compare_exchange(
            Self::QUERY_RECORDING,
            Self::QUERY_UNSEEN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Restore the one-way state from the canonical journal during startup.
    pub fn restore_status_query(&self) {
        self.query_state
            .store(Self::QUERY_RECORDED, Ordering::Release);
    }

    /// Claim the first durable disclosure of this task's immutable result.
    ///
    /// A concurrent result-bearing response waits for the active append. It may
    /// return content only after that append commits, or retry the append after
    /// the active writer releases a failed claim.
    pub async fn claim_result_disclosure(&self) -> bool {
        loop {
            let settled = self.disclosure_settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            match self.disclosure_state.load(Ordering::Acquire) {
                Self::DISCLOSURE_RECORDED => return false,
                Self::DISCLOSURE_UNSEEN => {
                    if self
                        .disclosure_state
                        .compare_exchange(
                            Self::DISCLOSURE_UNSEEN,
                            Self::DISCLOSURE_RECORDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                Self::DISCLOSURE_RECORDING => settled.await,
                state => unreachable!("invalid disclosure state {state}"),
            }
        }
    }

    pub fn commit_result_disclosure(&self) {
        self.disclosure_state
            .store(Self::DISCLOSURE_RECORDED, Ordering::Release);
        self.disclosure_settled.notify_waiters();
    }

    pub fn release_result_disclosure(&self) {
        let _ = self.disclosure_state.compare_exchange(
            Self::DISCLOSURE_RECORDING,
            Self::DISCLOSURE_UNSEEN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.disclosure_settled.notify_waiters();
    }

    pub async fn snapshot(&self) -> TaskSnapshot {
        let state = self.state.read().await;
        TaskSnapshot {
            state: state.rap,
            result: state.result.clone(),
            detail: state.detail.clone(),
        }
    }

    pub async fn advance(&self, target: RapTaskState) -> bool {
        let mut state = self.state.write().await;
        if state.rap == target {
            return true;
        }
        let transitioned = match state.rap.transition_or_err(target) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, task = %self.id, "A2A inbound task transition refused");
                false
            }
        };
        drop(state);
        if transitioned && target.is_terminal() {
            self.terminal.notify_waiters();
        }
        transitioned
    }

    /// Terminalize a task whose accepted outcome could not be persisted,
    /// without exposing the FSM's required intermediate `Working` hop.
    pub async fn fail_without_execution(&self, detail: impl Into<String>) -> bool {
        let mut state = self.state.write().await;
        if state.rap.is_terminal() {
            return false;
        }
        if matches!(
            state.rap,
            RapTaskState::Submitted | RapTaskState::AuthRequired
        ) && state.rap.transition_or_err(RapTaskState::Working).is_err()
        {
            return false;
        }
        if state.rap != RapTaskState::Working
            || state.rap.transition_or_err(RapTaskState::Failed).is_err()
        {
            return false;
        }
        state.pending = PendingAuth::None;
        state.detail = Some(detail.into());
        drop(state);
        self.terminal.notify_waiters();
        true
    }

    pub async fn set_pending_auth(&self, pending: PendingAuth) {
        self.state.write().await.pending = pending;
    }

    pub async fn pending_auth(&self) -> PendingAuth {
        self.state.read().await.pending
    }

    pub async fn set_detail(&self, detail: impl Into<String>) {
        self.state.write().await.detail = Some(detail.into());
    }

    pub async fn set_result(&self, result: Option<String>) {
        self.state.write().await.result = result;
    }

    pub async fn is_terminal(&self) -> bool {
        self.state.read().await.rap.is_terminal()
    }

    /// Wait until a lifecycle owner records a terminal wire state.
    ///
    /// `notified()` is created before the state read so an immediately
    /// concurrent terminal transition cannot be missed between the check and
    /// the wait.
    pub async fn wait_for_terminal(&self) {
        loop {
            let notified = self.terminal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_terminal().await {
                return;
            }
            notified.await;
        }
    }
}

/// Every task this server accepted, keyed by `(submitter, A2A task id)`.
pub struct InboundTaskStore {
    tasks: RwLock<HashMap<(SubmitterKey, String), Arc<InboundTask>>>,
    capacity: usize,
}

/// How many tasks one server retains. Bounded: an authenticated peer that
/// submits in a loop must not grow this map without limit. Eviction only ever
/// removes terminal tasks — a live task is never dropped out from under its
/// submitter.
pub const MAX_RETAINED_TASKS: usize = 512;

impl Default for InboundTaskStore {
    fn default() -> Self {
        Self::new(MAX_RETAINED_TASKS)
    }
}

impl InboundTaskStore {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Insert a newly-admitted task.
    ///
    /// Returns `None` only when `id` is already live for this submitter: an A2A
    /// `messageId` is submitter-chosen, so a replayed id must not silently
    /// rebind that principal's existing task. Another principal may use the
    /// same id without learning whether this one has.
    pub async fn insert(
        &self,
        id: String,
        node_id: AgentId,
        submitter: SubmitterKey,
        initial: RapTaskState,
    ) -> Option<Arc<InboundTask>> {
        let mut tasks = self.tasks.write().await;
        let key = (submitter.clone(), id.clone());
        if tasks.contains_key(&key) {
            return None;
        }
        if tasks.len() >= self.capacity {
            let mut evictable = Vec::new();
            for (key, task) in tasks.iter() {
                if let Ok(state) = task.state.try_read()
                    && state.rap.is_terminal()
                {
                    evictable.push(key.clone());
                }
            }
            if evictable.is_empty() {
                tracing::warn!(
                    "A2A inbound task store is full of live tasks; refusing a new submission"
                );
                return None;
            }
            evictable.sort_by(|left, right| {
                left.1
                    .cmp(&right.1)
                    .then(left.0.as_str().cmp(right.0.as_str()))
            });
            for key in evictable.iter().take(evictable.len().div_ceil(2)) {
                tasks.remove(key);
            }
        }
        let task = Arc::new(InboundTask::new(id, node_id, submitter, initial));
        tasks.insert(key, task.clone());
        Some(task)
    }

    /// Look up a task **as** `submitter`.
    ///
    /// A task that exists but belongs to another credential is reported exactly
    /// as a task that does not exist (AC6b/R4). This is technically imprecise
    /// about existence and is chosen deliberately: distinguishing "not yours"
    /// from "not found" turns `tasks/get` into a cross-peer task-id enumeration
    /// oracle, and mapping a host's federation for free is worse than an
    /// ambiguous error code. The rationale is recorded in the ADR so the next
    /// developer does not helpfully restore the oracle.
    pub async fn get_scoped(&self, id: &str, submitter: &SubmitterKey) -> Option<Arc<InboundTask>> {
        self.tasks
            .read()
            .await
            .get(&(submitter.clone(), id.to_owned()))
            .cloned()
    }

    /// Lookup by the complete internal key.
    ///
    /// Lifecycle code must carry the submitter alongside a task id; an
    /// unscoped lookup would silently reintroduce a cross-principal collision.
    pub async fn get(&self, id: &str, submitter: &SubmitterKey) -> Option<Arc<InboundTask>> {
        self.get_scoped(id, submitter).await
    }

    pub async fn len(&self) -> usize {
        self.tasks.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// The node-id namespace for inbound A2A tasks.
///
/// Both components cross a network trust boundary, so neither is interpolated
/// into an `AgentId` before encoding — same discipline as the outbound
/// `driver::mint_node_id`, a separate prefix so inbound and outbound peer nodes
/// are never confused in the tree or in recovery.
pub const INBOUND_NODE_PREFIX: &str = "a2a-in";

/// The `subagent_type` every inbound A2A peer node carries.
///
/// Distinct from the outbound driver's `a2a-peer` on purpose: restart
/// reconciliation must fail *our* in-flight inbound tasks without touching a
/// delegation this instance issued to somebody else, and one shared marker
/// would make those two indistinguishable in `NodeTree::list()`.
pub const INBOUND_SUBAGENT_TYPE: &str = "a2a-inbound";

/// Mint the local node id for an inbound task.
///
/// Encodes `(submitter, task id)` reversibly. Both values cross a network trust
/// boundary, so neither is interpolated into an `AgentId` before encoding — the
/// same discipline as the outbound `driver::mint_node_id`.
///
/// Carrying the *submitter* rather than our own peer id is what lets restart
/// reconciliation rebuild submitter scoping from the node tree alone, with no
/// second durable index to keep consistent.
#[must_use]
pub fn mint_inbound_node_id(submitter: &SubmitterKey, task_id: &str) -> AgentId {
    let peer = URL_SAFE_NO_PAD.encode(submitter.as_str().as_bytes());
    let task = URL_SAFE_NO_PAD.encode(task_id.as_bytes());
    AgentId::from_validated(format!("{INBOUND_NODE_PREFIX}/p-{peer}/t-{task}"))
}

/// Recover `(submitter, task_id)` from a node id minted by
/// [`mint_inbound_node_id`].
#[must_use]
pub fn parse_inbound_node_id(node_id: &AgentId) -> Option<(SubmitterKey, String)> {
    let rest = node_id
        .as_str()
        .strip_prefix(INBOUND_NODE_PREFIX)?
        .strip_prefix('/')?;
    let (peer, task) = rest.split_once('/')?;
    if task.contains('/') {
        return None;
    }
    let peer = String::from_utf8(URL_SAFE_NO_PAD.decode(peer.strip_prefix("p-")?).ok()?).ok()?;
    let task = String::from_utf8(URL_SAFE_NO_PAD.decode(task.strip_prefix("t-")?).ok()?).ok()?;

    if peer.is_empty() || task.is_empty() {
        return None;
    }
    Some((SubmitterKey::from_encoded(peer), task))
}

/// The opaque reason a task resolves `failed` before its local turn can start.
///
/// Startup diagnostics can name node-tree limits, agent ids, journals, or
/// provider configuration. Those remain local tracing data rather than a
/// remote task result.
pub const START_FAILURE_DETAIL: &str =
    "the task could not be started on this host; see the host operator's logs";

/// The reason a task resolves `failed` because the host restarted.
///
/// Kept textually distinct from [`CANCEL_DETAIL`]: a remote peer distinguishing
/// "the host died" from "my cancel was honored" is the difference between
/// retrying and not (AC5b restart scope).
pub const RESTART_DETAIL: &str = "the host restarted while this task was in flight; durable host-side resumption is not \
     implemented (DF-18-1-HOSTRECONCILE) — resubmit if the work is still wanted";

/// The reason a task resolves `canceled`.
pub const CANCEL_DETAIL: &str = "canceled at the submitter's request via tasks/cancel";

/// The reason a task resolves `failed` because its turn errored.
///
/// Deliberately opaque. The real failure names the host's model configuration,
/// endpoint and credential state; a helpful error message here would disclose
/// all three to whoever submitted the task.
pub const FAILURE_DETAIL: &str =
    "the local agent turn failed; see the host operator's logs for the cause";

/// The reason a task resolves `failed` because its acceptance could not be
/// made durable (Story 18.2, AC1).
///
/// Textually distinct from [`START_FAILURE_DETAIL`] on purpose: "your task
/// could not start" and "this host cannot record what it does" are different
/// facts, and only the second tells the submitter that retrying will keep
/// failing until the operator fixes the disk. Naming the transparency log is
/// the disclosure NFR67 wants; it reveals nothing about host state.
pub const UNRECORDED_ACCEPT_DETAIL: &str = "refused: this host could not durably record the task in its transparency log, and it does \
     not execute work it cannot account for";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_projection_can_express_every_wire_state_a_local_node_can_reach() {
        use PendingAuth::None as NoAuth;
        assert_eq!(
            project_node_to_rap(NodeState::Created, NoAuth),
            RapTaskState::Working
        );
        assert_eq!(
            project_node_to_rap(NodeState::Running, NoAuth),
            RapTaskState::Working
        );
        assert_eq!(
            project_node_to_rap(NodeState::Waiting, NoAuth),
            RapTaskState::Working
        );
        assert_eq!(
            project_node_to_rap(NodeState::Suspended, NoAuth),
            RapTaskState::Working
        );
        assert_eq!(
            project_node_to_rap(NodeState::Completed, NoAuth),
            RapTaskState::Completed
        );
        assert_eq!(
            project_node_to_rap(NodeState::Failed, NoAuth),
            RapTaskState::Failed
        );
        assert_eq!(
            project_node_to_rap(NodeState::Cancelled, NoAuth),
            RapTaskState::Canceled
        );
    }

    #[test]
    fn pending_approval_projects_to_auth_required_from_any_node_state() {
        // The multiword state is the one that matters: it is the only thing that
        // makes the camelCase-serde mutant falsifiable on the served path.
        for node in NodeState::ALL {
            assert_eq!(
                project_node_to_rap(*node, PendingAuth::Approval),
                RapTaskState::AuthRequired,
                "{node:?}"
            );
        }
    }

    #[test]
    fn node_ids_round_trip_through_untrusted_submitter_and_task_values() {
        for (submitter, task) in [
            (SubmitterKey::loopback(), "task-1"),
            (SubmitterKey::from_api_key("k"), "a/b/c"),
            (
                SubmitterKey::from_encoded("../../etc/passwd".to_owned()),
                "τask",
            ),
        ] {
            let id = mint_inbound_node_id(&submitter, task);
            assert!(id.as_str().starts_with("a2a-in/"));
            assert_eq!(
                parse_inbound_node_id(&id),
                Some((submitter, task.to_owned())),
                "a node id must survive a restart round trip, scoping included"
            );
        }
    }

    #[test]
    fn pseudonymous_peer_ids_are_stable_per_submitter_and_never_host_identity() {
        let first = SubmitterKey::from_api_key("first-key");
        let second = SubmitterKey::from_api_key("second-key");
        assert_eq!(
            first.pseudonymous_peer_id(),
            SubmitterKey::from_api_key("first-key").pseudonymous_peer_id()
        );
        assert_ne!(first.pseudonymous_peer_id(), second.pseudonymous_peer_id());
        assert_ne!(
            SubmitterKey::loopback().pseudonymous_peer_id(),
            first.pseudonymous_peer_id()
        );
    }

    #[test]
    fn an_outbound_peer_node_id_is_not_mistaken_for_an_inbound_one() {
        let outbound = AgentId::from_validated("a2a/p-x/t-y".to_owned());
        assert_eq!(parse_inbound_node_id(&outbound), None);
        assert_eq!(
            parse_inbound_node_id(&AgentId::from_validated("a2a-in/garbage".to_owned())),
            None
        );
    }

    #[tokio::test]
    async fn a_task_is_invisible_to_every_other_credential() {
        let store = InboundTaskStore::default();
        let owner = SubmitterKey::from_api_key("owner-key");
        let other = SubmitterKey::from_api_key("other-key");
        store
            .insert(
                "t1".to_owned(),
                mint_inbound_node_id(&owner, "t1"),
                owner.clone(),
                RapTaskState::Submitted,
            )
            .await
            .expect("insert");

        assert!(store.get_scoped("t1", &owner).await.is_some());
        assert!(store.get_scoped("t1", &other).await.is_none());
        assert!(
            store
                .get_scoped("t1", &SubmitterKey::loopback())
                .await
                .is_none()
        );
        // Identical to a task that never existed.
        assert!(store.get_scoped("fabricated", &other).await.is_none());
    }

    #[tokio::test]
    async fn a_replayed_task_id_does_not_rebind_a_live_task() {
        let store = InboundTaskStore::default();
        let owner = SubmitterKey::loopback();
        let first = store
            .insert(
                "t1".to_owned(),
                mint_inbound_node_id(&owner, "t1"),
                owner.clone(),
                RapTaskState::Submitted,
            )
            .await
            .expect("first insert");
        assert!(
            store
                .insert(
                    "t1".to_owned(),
                    mint_inbound_node_id(&owner, "other"),
                    owner.clone(),
                    RapTaskState::Submitted,
                )
                .await
                .is_none()
        );
        assert_eq!(
            store
                .get_scoped("t1", &owner)
                .await
                .expect("still there")
                .node_id,
            first.node_id
        );
    }

    #[tokio::test]
    async fn another_submitter_can_reuse_an_unrelated_task_id() {
        let store = InboundTaskStore::default();
        let first = SubmitterKey::from_api_key("first-key");
        let second = SubmitterKey::from_api_key("second-key");
        let first_task = store
            .insert(
                "same-id".to_owned(),
                mint_inbound_node_id(&first, "same-id"),
                first.clone(),
                RapTaskState::Submitted,
            )
            .await
            .expect("first principal insert");
        let second_task = store
            .insert(
                "same-id".to_owned(),
                mint_inbound_node_id(&second, "same-id"),
                second.clone(),
                RapTaskState::Submitted,
            )
            .await
            .expect("second principal must not collide");

        assert_ne!(first_task.node_id, second_task.node_id);
        assert_eq!(store.len().await, 2);
        assert_eq!(
            store
                .get("same-id", &first)
                .await
                .expect("first task")
                .node_id,
            first_task.node_id
        );
        assert_eq!(
            store
                .get("same-id", &second)
                .await
                .expect("second task")
                .node_id,
            second_task.node_id
        );
    }

    #[tokio::test]
    async fn the_store_evicts_terminal_tasks_and_never_live_ones() {
        let store = InboundTaskStore::new(2);
        let owner = SubmitterKey::loopback();
        let live = store
            .insert(
                "live".to_owned(),
                mint_inbound_node_id(&owner, "live"),
                owner.clone(),
                RapTaskState::Submitted,
            )
            .await
            .expect("live");
        let done = store
            .insert(
                "done".to_owned(),
                mint_inbound_node_id(&owner, "done"),
                owner.clone(),
                RapTaskState::Submitted,
            )
            .await
            .expect("done");
        assert!(done.advance(RapTaskState::Rejected).await);

        assert!(
            store
                .insert(
                    "new".to_owned(),
                    mint_inbound_node_id(&owner, "new"),
                    owner.clone(),
                    RapTaskState::Submitted,
                )
                .await
                .is_some()
        );
        assert!(
            store.get_scoped("live", &owner).await.is_some(),
            "live task evicted"
        );
        assert!(
            store.get_scoped("done", &owner).await.is_none(),
            "terminal task retained"
        );
        let _ = live;
    }

    #[tokio::test]
    async fn transitions_go_through_the_real_fsm() {
        let task = InboundTask::new(
            "t".to_owned(),
            mint_inbound_node_id(&SubmitterKey::loopback(), "t"),
            SubmitterKey::loopback(),
            RapTaskState::Submitted,
        );
        // Submitted -> Working -> AuthRequired -> Working -> Completed is the
        // R10 arc, and every hop is legal in the shipped FSM.
        assert!(task.advance(RapTaskState::Working).await);
        assert!(task.advance(RapTaskState::AuthRequired).await);
        assert!(task.advance(RapTaskState::Working).await);
        assert!(task.advance(RapTaskState::Completed).await);
        assert!(task.is_terminal().await);
        // Terminal is terminal: a late watcher update cannot resurrect it.
        assert!(!task.advance(RapTaskState::Working).await);
        assert_eq!(task.snapshot().await.state, RapTaskState::Completed);
    }

    #[tokio::test]
    async fn terminal_transition_wakes_durable_cancel_waiters() {
        let task = std::sync::Arc::new(InboundTask::new(
            "t".to_owned(),
            mint_inbound_node_id(&SubmitterKey::loopback(), "t"),
            SubmitterKey::loopback(),
            RapTaskState::Working,
        ));
        let waiting = task.clone();
        let waiter = tokio::spawn(async move {
            waiting.wait_for_terminal().await;
        });
        tokio::task::yield_now().await;
        assert!(task.advance(RapTaskState::Canceled).await);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("terminal transition wakes waiter")
            .expect("waiter does not panic");
    }

    #[test]
    fn the_restart_and_cancel_reasons_are_distinguishable() {
        // AC5b: a peer must be able to tell "the host died" (retry) from "my
        // cancel was honored" (do not retry).
        assert_ne!(RESTART_DETAIL, CANCEL_DETAIL);
        assert!(RESTART_DETAIL.contains("restart"));
        assert!(CANCEL_DETAIL.contains("cancel"));
    }

    #[test]
    fn startup_failure_reason_is_capability_scoped() {
        assert!(START_FAILURE_DETAIL.contains("host operator"));
        assert!(!START_FAILURE_DETAIL.contains("AgentId"));
    }
}
