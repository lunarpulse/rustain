//! Story Q-6 AC4 (K4) — a confirmed whole-plan cancellation reaches every running
//! plan-owned task and finalizes exactly once, after the last one settled.
//!
//! Front door: `PlanRuntime::confirm_whole_plan_cancel` is the core the TUI's
//! `InputAction::Decision(CancelPlanConfirm)` arm calls (the event loop's arm
//! holds nothing but that call plus its notice); children are launched through
//! `PlanRuntime::delegate_task` with a real runner integration; each child's
//! terminal status flows back as the `PlanTaskDelegationCompleted` event the
//! event loop turns into `PlanRuntime::on_turn_complete`, which this test replays
//! exactly as the loop does. The runner HOLDS every acknowledgment until the test
//! releases it, so "stop received" and "stop acknowledged" are separate events.
//! Waits are structural (channel receives); no sleep.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AgentDef, AgentId, AgentLaunchSpec, Conversation, NodeState, PermissionMode, Plan, PlanStatus,
    PlanTask, PlanTaskStatus, SubTaskFailurePolicy, SubagentError, TaskHandle,
    generate_conversation_id,
};
use rustain::domain::ports::{EventEmitter, SubagentRunner};
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};
use rustain::infrastructure::subagent::SubagentSpool;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(20);

fn task(number: u32, depends_on: Vec<u32>) -> PlanTask {
    PlanTask {
        number,
        title: format!("Task {number}"),
        description: format!("synthetic task {number}"),
        depends_on,
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

fn plan(id: &str, tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: id.to_string(),
        title: "Cancel plan".to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1_700_000_000,
        resolved_at: Some(1_700_000_060),
        host_message_id: Some("msg-1".to_string()),
        origin_restrictions: None,
    }
}

fn conversation(plans: Vec<Plan>) -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages: vec![],
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: plans
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect::<HashMap<_, _>>(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    }
}

/// One launched child: its stop token (`cancel`), the held acknowledgment, and
/// the channel a natural terminal status would use.
struct HeldChild {
    cancel: CancellationToken,
    ack: Arc<Notify>,
    status_tx: mpsc::Sender<NodeState>,
}

#[derive(Default)]
struct HeldRunner {
    children: parking_lot::Mutex<Vec<HeldChild>>,
}

impl HeldRunner {
    fn child(&self, index: usize) -> (CancellationToken, Arc<Notify>, mpsc::Sender<NodeState>) {
        let children = self.children.lock();
        let child = &children[index];
        (
            child.cancel.clone(),
            child.ack.clone(),
            child.status_tx.clone(),
        )
    }
    fn launched(&self) -> usize {
        self.children.lock().len()
    }
}

#[async_trait::async_trait]
impl SubagentRunner for HeldRunner {
    async fn launch(
        &self,
        _spec: AgentLaunchSpec,
        cancel: CancellationToken,
        _parent: Option<&TaskHandle>,
        agent_id: AgentId,
    ) -> Result<TaskHandle, SubagentError> {
        let (status_tx, status_rx) = mpsc::channel(4);
        let ack = Arc::new(Notify::new());
        self.children.lock().push(HeldChild {
            cancel: cancel.clone(),
            ack: ack.clone(),
            status_tx: status_tx.clone(),
        });
        // The child reports `Cancelled` only after it saw the stop AND the test
        // released its acknowledgment.
        let (watch_cancel, watch_tx) = (cancel.clone(), status_tx);
        tokio::spawn(async move {
            watch_cancel.cancelled().await;
            ack.notified().await;
            let _ = watch_tx.send(NodeState::Cancelled).await;
        });
        let (parent_disconnect, _rx) = mpsc::unbounded_channel();
        Ok(TaskHandle {
            agent_id,
            task_id: "held-child".to_string(),
            status_rx,
            command_tx: mpsc::channel(1).0,
            cancel,
            subagent_type: "held".to_string(),
            spawned_at: 0,
            parent_disconnect,
            yield_rx: None,
            isolation_diff_rx: None,
            effective_workspace: std::path::PathBuf::from("."),
            isolated: false,
            authority: rustain::domain::models::CapabilityTokenId::root(),
            authority_token: None,
            patch_provenance: rustain::domain::models::ProvenanceTag::UserOriginated,
        })
    }
}

struct ChannelEmitter(mpsc::UnboundedSender<AppEvent>);

impl EventEmitter for ChannelEmitter {
    fn emit(&self, event: AppEvent) {
        let _ = self.0.send(event);
    }
}

struct World {
    runtime: Arc<PlanRuntime>,
    conversation: Conversation,
    plan_id: String,
    runner: Arc<HeldRunner>,
    spool: Arc<SubagentSpool>,
    emitter: Arc<ChannelEmitter>,
    events: mpsc::UnboundedReceiver<AppEvent>,
    /// Every event drained so far, in order.
    seen: Vec<AppEvent>,
}

impl World {
    async fn new(plans: Vec<Plan>, primary: &str) -> Self {
        let (tx, events) = mpsc::unbounded_channel();
        let emitter = Arc::new(ChannelEmitter(tx));
        let mut conversation = conversation(plans);
        let runtime = PlanRuntime::new();
        for id in conversation.plans.keys().cloned().collect::<Vec<_>>() {
            runtime.clone().start(
                conversation.id.clone(),
                id.clone(),
                &mut conversation,
                emitter.as_ref(),
                &[],
                PermissionMode::Normal,
                SubTaskFailurePolicy::default(),
            );
            // `start` dispatches into the turn loop; this witness drives the
            // delegated path, so every task begins Pending again.
            for task in &mut conversation.plans.get_mut(&id).unwrap().tasks {
                task.status = PlanTaskStatus::Pending;
                task.started_at_ms = None;
            }
        }
        let spool = Arc::new(
            SubagentSpool::new(tempfile::tempdir().unwrap().keep())
                .await
                .unwrap(),
        );
        let mut world = Self {
            runtime,
            conversation,
            plan_id: primary.to_string(),
            runner: Arc::new(HeldRunner::default()),
            spool,
            emitter,
            events,
            seen: Vec::new(),
        };
        // `start` queued its own turn-loop dispatch events; the witness
        // observes only what follows the confirmation.
        world.drain();
        world.seen.clear();
        world
    }

    async fn delegate(&mut self, plan_id: &str, task_number: u32) {
        let agent = AgentDef {
            name: "held".to_string(),
            description: "held child".to_string(),
            file: std::path::PathBuf::from("/dev/null"),
            model: None,
            allowed_tools: None,
            exclude_tools: None,
            isolated: false,
        };
        self.runtime
            .clone()
            .delegate_task(
                &self.conversation.id.clone(),
                plan_id,
                task_number,
                &agent,
                self.runner.clone(),
                self.spool.clone(),
                &mut self.conversation,
                self.emitter.clone(),
                "default-model",
            )
            .await
            .expect("delegation launches");
    }

    /// Wait for the next child terminal event and replay it into the runtime
    /// exactly as the event loop's `PlanTaskDelegationCompleted` arm does.
    async fn replay_next_completion(&mut self) -> (String, u32) {
        loop {
            let event = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("a child reports")
                .expect("emitter open");
            if let AppEvent::PlanTaskDelegationCompleted {
                conversation_id,
                plan_id,
                task_number,
                outcome,
            } = event
            {
                self.runtime
                    .on_turn_complete(
                        &conversation_id,
                        &plan_id,
                        task_number,
                        outcome,
                        &mut self.conversation,
                        self.emitter.as_ref(),
                        &[],
                        PermissionMode::Normal,
                    )
                    .await;
                return (plan_id, task_number);
            }
            self.seen.push(event);
        }
    }

    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.seen.push(event);
        }
    }

    fn cancelled_events(&mut self) -> usize {
        self.drain();
        self.seen
            .iter()
            .filter(|event| matches!(event, AppEvent::PlanCancelled { .. }))
            .count()
    }

    fn status(&self, plan_id: &str, number: u32) -> PlanTaskStatus {
        self.conversation.plans[plan_id].tasks[(number - 1) as usize].status
    }
}

fn three_task_plan(id: &str) -> Plan {
    // 1 and 2 are independent and run together; 3 waits on 1 (pending work).
    plan(id, vec![task(1, vec![]), task(2, vec![]), task(3, vec![1])])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k4_whole_plan_cancel_signals_every_child_and_finalizes_once_after_the_last_ack() {
    whole_plan_cancel(0, 1).await;
}

/// Acknowledgments are matched to the tracked execution that sent them, not to
/// "the first row marked Running": the SECOND-launched child may report first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k4_acknowledgments_match_their_own_execution_in_either_order() {
    whole_plan_cancel(1, 0).await;
}

async fn whole_plan_cancel(first_ack: usize, last_ack: usize) {
    let (first_task, last_task) = (first_ack as u32 + 1, last_ack as u32 + 1);
    let mut world = World::new(
        vec![three_task_plan("plan-a"), three_task_plan("plan-b")],
        "plan-a",
    )
    .await;
    world.delegate("plan-a", 1).await;
    world.delegate("plan-a", 2).await;
    world.delegate("plan-b", 1).await;
    assert_eq!(world.runner.launched(), 3);
    let plan_id = world.plan_id.clone();

    // Declining a confirmation is "no call": nothing has been signalled.
    for index in 0..3 {
        assert!(
            !world.runner.child(index).0.is_cancelled(),
            "child {index} before"
        );
    }

    let signalled = world.runtime.confirm_whole_plan_cancel(&plan_id).await;

    // Both of THIS plan's children receive the stop — not just the first — and
    // the unrelated plan's child is untouched.
    assert_eq!(signalled, 2);
    assert!(world.runner.child(0).0.is_cancelled());
    assert!(world.runner.child(1).0.is_cancelled());
    assert!(
        !world.runner.child(2).0.is_cancelled(),
        "an unrelated plan is untouched"
    );
    let snapshot = world
        .runtime
        .snapshot(&plan_id)
        .await
        .expect("still tracked");
    assert!(snapshot.whole_plan_cancel_pending, "Cancelling is latched");
    assert_eq!(
        world.conversation.plans[&plan_id].status,
        PlanStatus::Executing
    );
    assert_eq!(
        world.cancelled_events(),
        0,
        "no terminal presentation before any ack"
    );

    // The FIRST legitimate acknowledgment: one child settles, the plan does not.
    world.runner.child(first_ack).1.notify_one();
    let (_, first) = world.replay_next_completion().await;
    assert_eq!(first, first_task);
    assert_eq!(
        world.status(&plan_id, first_task),
        PlanTaskStatus::Cancelled
    );
    assert_eq!(
        world.status(&plan_id, last_task),
        PlanTaskStatus::Running,
        "the other child is still live"
    );
    assert_eq!(
        world.status(&plan_id, 3),
        PlanTaskStatus::Pending,
        "pending work is not yet cancelled"
    );
    assert_eq!(
        world.conversation.plans[&plan_id].status,
        PlanStatus::Executing
    );
    assert_eq!(
        world.cancelled_events(),
        0,
        "an early aggregate completion fails"
    );
    assert_eq!(world.runner.launched(), 3, "no follow-on work started");
    assert!(
        world.runtime.snapshot(&plan_id).await.is_some(),
        "runtime state not removed yet"
    );

    // The LAST acknowledgment: exactly one terminal result.
    world.runner.child(last_ack).1.notify_one();
    let (_, second) = world.replay_next_completion().await;
    assert_eq!(second, last_task);
    assert_eq!(
        world.conversation.plans[&plan_id].status,
        PlanStatus::Cancelled
    );
    assert_eq!(
        world.status(&plan_id, 3),
        PlanTaskStatus::Cancelled,
        "pending work never started"
    );
    assert_eq!(
        world.cancelled_events(),
        1,
        "exactly one terminal presentation"
    );
    assert!(
        world.runtime.snapshot(&plan_id).await.is_none(),
        "removed once, at the end"
    );
    assert_eq!(world.runner.launched(), 3, "no follow-on work started");

    // A stale or duplicate completion neither finishes the plan again nor
    // launches anything.
    for outcome in [
        TaskTurnOutcome::Cancelled {
            reason: "duplicate".into(),
        },
        TaskTurnOutcome::Success {
            result_text: "stale".into(),
            tool_call_count: 0,
            token_count: None,
        },
    ] {
        world
            .runtime
            .on_turn_complete(
                &world.conversation.id.clone(),
                &plan_id,
                last_task,
                outcome,
                &mut world.conversation,
                world.emitter.as_ref(),
                &[],
                PermissionMode::Normal,
            )
            .await;
    }
    assert_eq!(world.cancelled_events(), 1, "still exactly one");
    assert_eq!(world.runner.launched(), 3);
    // The unrelated plan keeps running.
    assert_eq!(world.status("plan-b", 1), PlanTaskStatus::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k4_a_child_that_finishes_naturally_during_cancelling_keeps_its_outcome_and_starts_nothing()
{
    let mut world = World::new(
        vec![plan(
            "plan-a",
            vec![task(1, vec![]), task(2, vec![]), task(3, vec![2])],
        )],
        "plan-a",
    )
    .await;
    world.delegate("plan-a", 1).await;
    world.delegate("plan-a", 2).await;
    let plan_id = world.plan_id.clone();

    assert_eq!(world.runtime.confirm_whole_plan_cancel(&plan_id).await, 2);

    // Child 2 completes on its own (a racing natural success) before any stop ack.
    world
        .runner
        .child(1)
        .2
        .send(NodeState::Completed)
        .await
        .unwrap();
    let (_, number) = world.replay_next_completion().await;
    assert_eq!(number, 2);
    assert_eq!(
        world.status(&plan_id, 2),
        PlanTaskStatus::Completed,
        "a genuine outcome is preserved"
    );
    assert_eq!(
        world.conversation.plans[&plan_id].status,
        PlanStatus::Executing,
        "child 1 is still live"
    );
    assert_eq!(world.cancelled_events(), 0);
    let launches_before = world.runner.launched();

    world.runner.child(0).1.notify_one();
    let (_, number) = world.replay_next_completion().await;
    assert_eq!(number, 1);

    assert_eq!(
        world.conversation.plans[&plan_id].status,
        PlanStatus::Cancelled
    );
    assert_eq!(
        world.status(&plan_id, 2),
        PlanTaskStatus::Completed,
        "not rewritten to Cancelled"
    );
    assert_eq!(
        world.status(&plan_id, 3),
        PlanTaskStatus::Cancelled,
        "its dependent never started"
    );
    assert_eq!(world.cancelled_events(), 1);
    assert_eq!(
        world.runner.launched(),
        launches_before,
        "no task launched after the latch"
    );
    world.drain();
    assert!(
        !world.seen.iter().any(|event| matches!(
            event,
            AppEvent::AgentThenSubmit { .. } | AppEvent::PlanTaskDelegationRequested { .. }
        )),
        "no follow-on dispatch was requested"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k4_confirming_an_unknown_plan_signals_nothing_and_changes_nothing() {
    let mut world = World::new(vec![three_task_plan("plan-a")], "plan-a").await;
    world.delegate("plan-a", 1).await;
    assert_eq!(
        world
            .runtime
            .confirm_whole_plan_cancel("no-such-plan")
            .await,
        0
    );
    assert!(!world.runner.child(0).0.is_cancelled());
    assert!(
        !world
            .runtime
            .snapshot("plan-a")
            .await
            .unwrap()
            .whole_plan_cancel_pending
    );
    assert_eq!(world.cancelled_events(), 0);
}
