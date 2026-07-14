use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::ContentBlockType;
use crate::domain::models::MessageRole;
use crate::domain::models::NoticeLevel;
use crate::domain::models::agent::AgentDef;
use crate::domain::models::conversation::{ChatMessage, generate_message_id};
use crate::domain::models::plan::{
    DelegationInfo, Plan, PlanDeviationKind, PlanStatus, PlanTask, PlanTaskStatus, TaskResult,
};
use crate::domain::models::tab::ConversationId;
use crate::domain::ports::EventEmitter;
use crate::domain::services::delegation_decider::DelegationDecider;
use crate::domain::services::launch_spec_builder::LaunchSpecBuilder;

type TaskSummary = (
    u32,
    String,
    PlanTaskStatus,
    Option<i64>,
    Option<String>,
    Option<TaskResult>,
);

pub struct PlanRuntime {
    plans: RwLock<HashMap<String, PlanRuntimeState>>,
}

pub struct PlanRuntimeState {
    pub conversation_id: String,
    pub plan_id: String,
    pub task_cancels: BTreeMap<u32, CancellationToken>,
    /// Number of assistant messages in the conversation when the current task started.
    /// Used to detect when a new assistant response has arrived for this task.
    pub task_start_assistant_count: usize,
    /// Story 6.4: tasks that are pending a pause transition in the Cancelled branch.
    pub pause_pending_tasks: HashSet<u32>,
    /// Story 6.4: set true when user invokes !cancel-plan on the whole plan.
    pub whole_plan_cancel_pending: bool,
    /// Story 6.4: deviation pending reapproval. Blocks advance_after_task until resolved.
    pub pending_deviation: Option<PlanDeviationKind>,
    /// Story 10.6: maps parent task_number → currently running sub-task number.
    /// Used by `on_turn_complete` to route outcomes to the correct sub-task.
    pub active_sub_tasks: BTreeMap<u32, u32>,
    /// Story 10.6: sub-task failure policy for this plan execution.
    pub subtask_failure_policy: crate::domain::models::SubTaskFailurePolicy,
}

#[derive(Clone, Debug)]
pub enum TaskTurnOutcome {
    Success {
        result_text: String,
        tool_call_count: u32,
        token_count: Option<u32>,
    },
    Failure {
        error: String,
    },
    Cancelled {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DelegateTaskError {
    #[error("subagent runner failed: {0}")]
    LaunchFailed(#[from] crate::domain::models::SubagentError),
    #[error("plan {plan_id} or task {task_number} not found at delegate_task call site")]
    NotFound { plan_id: String, task_number: u32 },
}

impl PlanRuntime {
    pub fn new() -> Arc<Self> {
        Arc::new(PlanRuntime {
            plans: RwLock::new(HashMap::new()),
        })
    }

    pub fn start(
        self: Arc<Self>,
        conversation_id: ConversationId,
        plan_id: String,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        agents: &[AgentDef],
        mode: crate::domain::models::PermissionMode,
        subtask_failure_policy: crate::domain::models::SubTaskFailurePolicy,
    ) {
        tracing::debug!("PlanRuntime::start: plan={plan_id} conv={conversation_id}");
        // Story 6.4 reload normalization: flip Paused → Pending.
        // After restart, in-memory task_cancels is empty; a Paused
        // task has nothing to resume from. Pending lets find_next_eligible
        // re-arm it on next user action.
        if let Some(plan) = conversation.plans.get_mut(&plan_id) {
            for task in &mut plan.tasks {
                if task.status == PlanTaskStatus::Paused {
                    task.status = PlanTaskStatus::Pending;
                    task.started_at_ms = None;
                }
            }
        }

        {
            let plan = match conversation.plans.get(&plan_id) {
                Some(p) => p,
                None => {
                    tracing::warn!("PlanRuntime::start: plan {} not found", plan_id);
                    return;
                }
            };
            if plan.status != PlanStatus::Executing {
                tracing::warn!(
                    "PlanRuntime::start: plan {} status is {:?}, expected Executing",
                    plan_id,
                    plan.status
                );
                return;
            }
        }

        {
            let plans = self.plans.read().unwrap();
            if plans.contains_key(&plan_id) {
                tracing::warn!(
                    "PlanRuntime::start: plan {} already has runtime state (idempotent no-op)",
                    plan_id
                );
                return;
            }
        }

        let assistant_count = conversation
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .count();
        let state = PlanRuntimeState {
            conversation_id: conversation_id.clone(),
            plan_id: plan_id.clone(),
            task_cancels: BTreeMap::new(),
            task_start_assistant_count: assistant_count,
            pause_pending_tasks: HashSet::new(),
            whole_plan_cancel_pending: false,
            pending_deviation: None,
            active_sub_tasks: BTreeMap::new(),
            subtask_failure_policy,
        };

        {
            let mut plans = self.plans.write().unwrap();
            plans.insert(plan_id.clone(), state);
        }

        let next_number = {
            let plan = conversation.plans.get(&plan_id).unwrap();
            find_next_eligible(plan).map(|t| t.number)
        };

        if let Some(task_number) = next_number {
            let plan = conversation.plans.get(&plan_id).unwrap();
            let task = &plan.tasks[(task_number - 1) as usize];

            if task.has_sub_tasks() {
                let now_ms = chrono::Utc::now().timestamp_millis();
                {
                    let plan = conversation.plans.get_mut(&plan_id).unwrap();
                    let parent = &mut plan.tasks[(task_number - 1) as usize];
                    parent.status = PlanTaskStatus::Running;
                    parent.started_at_ms = Some(now_ms);
                }
                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.clone(),
                    task_number,
                    status: PlanTaskStatus::Running,
                });

                // Find first pending sub-task
                let next_sub = {
                    let plan = conversation.plans.get(&plan_id).unwrap();
                    let parent = &plan.tasks[(task_number - 1) as usize];
                    parent
                        .sub_tasks
                        .iter()
                        .find(|st| st.status == PlanTaskStatus::Pending)
                        .map(|st| st.number)
                };

                if let Some(sub_number) = next_sub {
                    let synthetic_task = {
                        let plan = conversation.plans.get(&plan_id).unwrap();
                        let parent = &plan.tasks[(task_number - 1) as usize];
                        let sub = &parent.sub_tasks[(sub_number - 1) as usize];
                        PlanTask {
                            number: sub.number,
                            title: sub.title.clone(),
                            description: sub.description.clone(),
                            depends_on: vec![],
                            status: sub.status,
                            started_at_ms: sub.started_at_ms,
                            completed_at_ms: sub.completed_at_ms,
                            result: sub.result.clone(),
                            error: sub.error.clone(),
                            waiting_on: vec![],
                            delegated_to: sub.delegated_to.clone(),
                            sub_tasks: vec![],
                        }
                    };
                    let plan = conversation.plans.get(&plan_id).unwrap();
                    if let Some(suggestion) =
                        DelegationDecider::suggest(plan, &synthetic_task, agents, mode)
                    {
                        {
                            let mut plans = self.plans.write().unwrap();
                            if let Some(state) = plans.get_mut(&plan_id) {
                                state.active_sub_tasks.insert(task_number, sub_number);
                            }
                        }
                        event_emitter.emit(AppEvent::PlanTaskDelegationRequested {
                            conversation_id: conversation_id.clone(),
                            plan_id: plan_id.clone(),
                            task_number,
                            suggestion,
                        });
                    } else {
                        self.dispatch_sub_task(
                            &conversation_id,
                            &plan_id,
                            conversation,
                            event_emitter,
                            task_number,
                            sub_number,
                        );
                    }
                } else {
                    // All sub-tasks already terminal — finalize parent immediately
                    self.finalize_parent_task(
                        &conversation_id,
                        &plan_id,
                        conversation,
                        event_emitter,
                        task_number,
                        crate::domain::models::SubTaskFailurePolicy::default(),
                    );
                    self.advance_after_task(
                        &conversation_id,
                        &plan_id,
                        conversation,
                        event_emitter,
                        agents,
                        mode,
                        None,
                    );
                }
                return;
            }

            // Story 10.5: check delegation before flipping to Running
            if let Some(suggestion) = DelegationDecider::suggest(plan, task, agents, mode) {
                event_emitter.emit(AppEvent::PlanTaskDelegationRequested {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.clone(),
                    task_number,
                    suggestion,
                });
                return;
            }

            self.dispatch_single_task(
                &conversation_id,
                &plan_id,
                conversation,
                event_emitter,
                task_number,
            );
        }
    }

    pub async fn on_turn_complete(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        task_number: u32,
        outcome: TaskTurnOutcome,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        agents: &[AgentDef],
        mode: crate::domain::models::PermissionMode,
    ) {
        // G1-P3 + G6-P20: validate plan state and task_number before mutating
        {
            let plan = match conversation.plans.get(plan_id) {
                Some(p) => p,
                None => return,
            };
            if plan.status == PlanStatus::Cancelled {
                return; // already cancelled; ignore stale TurnComplete
            }
            let running_num = plan
                .tasks
                .iter()
                .find(|t| t.status == PlanTaskStatus::Running)
                .map(|t| t.number);
            if running_num != Some(task_number) {
                tracing::warn!(
                    "PlanRuntime::on_turn_complete: task_number {} does not match running task {:?}; ignoring",
                    task_number,
                    running_num
                );
                return;
            }
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let idx = (task_number - 1) as usize;

        {
            let mut plans = self.plans.write().unwrap();
            if let Some(state) = plans.get_mut(plan_id) {
                state.task_cancels.remove(&task_number);
            }
        }

        // Story 10.6: route outcome to active sub-task if present
        if let Some(sub_number) = {
            let plans = self.plans.read().unwrap();
            plans
                .get(plan_id)
                .and_then(|s| s.active_sub_tasks.get(&task_number).copied())
        } {
            self.handle_sub_task_outcome(
                conversation_id,
                plan_id,
                task_number,
                sub_number,
                outcome,
                conversation,
                event_emitter,
                agents,
                mode,
                now_ms,
            )
            .await;
            return;
        }

        match outcome {
            TaskTurnOutcome::Success {
                result_text,
                tool_call_count,
                token_count,
            } => {
                {
                    let plan = match conversation.plans.get_mut(plan_id) {
                        Some(p) => p,
                        None => return,
                    };
                    if idx >= plan.tasks.len() {
                        return;
                    }
                    plan.tasks[idx].status = PlanTaskStatus::Completed;
                    plan.tasks[idx].completed_at_ms = Some(now_ms);
                    plan.tasks[idx].result = Some(TaskResult {
                        text: result_text,
                        tool_call_count,
                        token_count,
                    });
                }
                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    task_number,
                    status: PlanTaskStatus::Completed,
                });

                // Drain pause-pending entry — task already terminal, pause intent moot.
                // Only the Cancelled branch drains today; without this, the entry leaks
                // for the lifetime of the plan and a future check on the same task
                // number would be fooled.
                if let Some(s) = self.plans.write().unwrap().get_mut(plan_id) {
                    s.pause_pending_tasks.remove(&task_number);
                }

                // Check for whole-plan-cancel race (task completed before token cancel)
                let is_whole_cancel = {
                    let plans = self.plans.read().unwrap();
                    plans
                        .get(plan_id)
                        .map(|s| s.whole_plan_cancel_pending)
                        .unwrap_or(false)
                };
                if is_whole_cancel {
                    if let Some(plan) = conversation.plans.get_mut(plan_id) {
                        for task in &mut plan.tasks {
                            if task.number == task_number {
                                continue;
                            }
                            if !is_terminal(task.status) {
                                task.status = PlanTaskStatus::Cancelled;
                                task.completed_at_ms = Some(now_ms);
                                task.error = Some("Plan cancelled by user".to_string());
                                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                                    conversation_id: conversation_id.clone(),
                                    plan_id: plan_id.to_string(),
                                    task_number: task.number,
                                    status: PlanTaskStatus::Cancelled,
                                });
                            }
                        }
                        plan.status = PlanStatus::Cancelled;
                    }
                    event_emitter.emit(AppEvent::PlanCancelled {
                        conversation_id: conversation_id.clone(),
                        plan_id: plan_id.to_string(),
                        cancelled_at_task: None,
                    });
                    finish_plan(conversation_id, plan_id, conversation, event_emitter);
                    self.plans.write().unwrap().remove(plan_id);
                    return;
                }

                self.advance_after_task(
                    conversation_id,
                    plan_id,
                    conversation,
                    event_emitter,
                    agents,
                    mode,
                    None,
                );
            }
            TaskTurnOutcome::Failure { error } => {
                {
                    let plan = match conversation.plans.get_mut(plan_id) {
                        Some(p) => p,
                        None => return,
                    };
                    if idx >= plan.tasks.len() {
                        return;
                    }
                    plan.tasks[idx].status = PlanTaskStatus::Failed;
                    plan.tasks[idx].completed_at_ms = Some(now_ms);
                    plan.tasks[idx].error = Some(error);
                }
                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    task_number,
                    status: PlanTaskStatus::Failed,
                });

                // Drain pause-pending entry — task already terminal, pause intent moot.
                if let Some(s) = self.plans.write().unwrap().get_mut(plan_id) {
                    s.pause_pending_tasks.remove(&task_number);
                }

                // Check for whole-plan-cancel race (task failed before token cancel)
                let is_whole_cancel = {
                    let plans = self.plans.read().unwrap();
                    plans
                        .get(plan_id)
                        .map(|s| s.whole_plan_cancel_pending)
                        .unwrap_or(false)
                };
                if is_whole_cancel {
                    if let Some(plan) = conversation.plans.get_mut(plan_id) {
                        for task in &mut plan.tasks {
                            if task.number == task_number {
                                continue;
                            }
                            if !is_terminal(task.status) {
                                task.status = PlanTaskStatus::Cancelled;
                                task.completed_at_ms = Some(now_ms);
                                task.error = Some("Plan cancelled by user".to_string());
                                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                                    conversation_id: conversation_id.clone(),
                                    plan_id: plan_id.to_string(),
                                    task_number: task.number,
                                    status: PlanTaskStatus::Cancelled,
                                });
                            }
                        }
                        plan.status = PlanStatus::Cancelled;
                    }
                    event_emitter.emit(AppEvent::PlanCancelled {
                        conversation_id: conversation_id.clone(),
                        plan_id: plan_id.to_string(),
                        cancelled_at_task: None,
                    });
                    finish_plan(conversation_id, plan_id, conversation, event_emitter);
                    self.plans.write().unwrap().remove(plan_id);
                    return;
                }

                // Story 6.4: wrap skip_blocked_tasks with PlanDeviation emission
                let (skipped_count, changed_steps) = {
                    let _original_len = {
                        let plan = conversation.plans.get(plan_id).unwrap();
                        plan.tasks.len() as u32
                    };
                    let plan = conversation.plans.get_mut(plan_id).unwrap();
                    let pre_len = plan.tasks.len();
                    skip_blocked_tasks(conversation_id, plan_id, task_number, plan, event_emitter);
                    let skipped: Vec<_> = plan
                        .tasks
                        .iter()
                        .filter(|t| {
                            t.status == PlanTaskStatus::Skipped
                                && t.error
                                    .as_ref()
                                    .is_some_and(|e| e.contains("depends on failed task"))
                        })
                        .map(|t| t.number)
                        .collect();
                    let count = skipped.len() as u32;
                    // revert pre_len if the body changed length (shouldn't — tasks don't get added)
                    let _ = pre_len;
                    (count, skipped)
                };

                if skipped_count > 0 {
                    let plan = conversation.plans.get(plan_id).unwrap();
                    let current_step_count = plan.tasks.len() as u32 - skipped_count;
                    let summary = format!(
                        "Auto-skipped {} task(s) blocked by upstream failure of Task {}.",
                        skipped_count, task_number
                    );
                    let deviation_kind = PlanDeviationKind::AutoSkipBlockedTasks {
                        source_task: task_number,
                    };
                    self.mark_deviation_pending(plan_id, deviation_kind.clone())
                        .await;
                    event_emitter.emit(AppEvent::PlanDeviation {
                        conversation_id: conversation_id.clone(),
                        plan_id: plan_id.to_string(),
                        deviation_kind,
                        original_step_count: current_step_count + skipped_count,
                        current_step_count,
                        changed_steps,
                        summary,
                    });
                }

                if !self.has_pending_deviation(plan_id).await {
                    self.advance_after_task(
                        conversation_id,
                        plan_id,
                        conversation,
                        event_emitter,
                        agents,
                        mode,
                        None,
                    );
                }
            }
            TaskTurnOutcome::Cancelled { reason } => {
                let (is_pause_pending, is_whole_cancel) = {
                    let plans = self.plans.read().unwrap();
                    plans
                        .get(plan_id)
                        .map(|s| {
                            (
                                s.pause_pending_tasks.contains(&task_number),
                                s.whole_plan_cancel_pending,
                            )
                        })
                        .unwrap_or((false, false))
                };

                if is_pause_pending {
                    // AC1: pause-pending path — flip to Paused, plan stays Executing
                    {
                        let plan = match conversation.plans.get_mut(plan_id) {
                            Some(p) => p,
                            None => return,
                        };
                        if idx >= plan.tasks.len() {
                            return;
                        }
                        plan.tasks[idx].status = PlanTaskStatus::Paused;
                        plan.tasks[idx].error = None;
                        // completed_at_ms is NOT set — Paused is non-terminal
                    }
                    event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                        conversation_id: conversation_id.clone(),
                        plan_id: plan_id.to_string(),
                        task_number,
                        status: PlanTaskStatus::Paused,
                    });
                    // Remove from pause_pending set — it's been consumed
                    if let Some(s) = self.plans.write().unwrap().get_mut(plan_id) {
                        s.pause_pending_tasks.remove(&task_number);
                    }
                    // Do NOT call skip_blocked_tasks or advance_after_task — runtime idles
                    return;
                }

                if is_whole_cancel {
                    // AC6: whole-plan-cancel-pending path — cancel all non-terminal
                    {
                        let plan = match conversation.plans.get_mut(plan_id) {
                            Some(p) => p,
                            None => return,
                        };
                        if idx < plan.tasks.len() {
                            plan.tasks[idx].status = PlanTaskStatus::Cancelled;
                            plan.tasks[idx].completed_at_ms = Some(now_ms);
                            plan.tasks[idx].error = Some(reason);
                        }
                        for task in &mut plan.tasks {
                            if is_terminal(task.status) {
                                continue;
                            }
                            task.status = PlanTaskStatus::Cancelled;
                            task.completed_at_ms = Some(now_ms);
                            task.error = Some("Plan cancelled by user".to_string());
                            event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                                conversation_id: conversation_id.clone(),
                                plan_id: plan_id.to_string(),
                                task_number: task.number,
                                status: PlanTaskStatus::Cancelled,
                            });
                        }
                        plan.status = PlanStatus::Cancelled;
                    }
                    event_emitter.emit(AppEvent::PlanCancelled {
                        conversation_id: conversation_id.clone(),
                        plan_id: plan_id.to_string(),
                        cancelled_at_task: None,
                    });
                    finish_plan(conversation_id, plan_id, conversation, event_emitter);
                    self.plans.write().unwrap().remove(plan_id);
                    return;
                }

                // Existing hard-stop path (per-task cancel → PlanCancelled with Some(n))
                {
                    let plan = match conversation.plans.get_mut(plan_id) {
                        Some(p) => p,
                        None => return,
                    };
                    if idx >= plan.tasks.len() {
                        return;
                    }
                    plan.tasks[idx].status = PlanTaskStatus::Cancelled;
                    plan.tasks[idx].completed_at_ms = Some(now_ms);
                    plan.tasks[idx].error = Some(reason);
                    plan.status = PlanStatus::Cancelled;
                }
                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    task_number,
                    status: PlanTaskStatus::Cancelled,
                });
                event_emitter.emit(AppEvent::PlanCancelled {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    cancelled_at_task: Some(task_number),
                });
                // G6-P19: clean up runtime state on cancellation hard-stop
                self.plans.write().unwrap().remove(plan_id);
            }
        }
    }

    /// Story 10.6: apply a turn outcome to the active sub-task of `parent_number`,
    /// then finalize the parent when all sub-tasks are terminal.
    async fn handle_sub_task_outcome(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        parent_number: u32,
        sub_number: u32,
        outcome: TaskTurnOutcome,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        agents: &[AgentDef],
        mode: crate::domain::models::PermissionMode,
        now_ms: i64,
    ) {
        let parent_idx = (parent_number - 1) as usize;
        let sub_idx = (sub_number - 1) as usize;

        // Apply outcome to sub-task
        match &outcome {
            TaskTurnOutcome::Success {
                result_text,
                tool_call_count,
                token_count,
            } => {
                let plan = conversation.plans.get_mut(plan_id).unwrap();
                let sub_task = &mut plan.tasks[parent_idx].sub_tasks[sub_idx];
                sub_task.status = PlanTaskStatus::Completed;
                sub_task.completed_at_ms = Some(now_ms);
                sub_task.result = Some(TaskResult {
                    text: result_text.clone(),
                    tool_call_count: *tool_call_count,
                    token_count: *token_count,
                });
            }
            TaskTurnOutcome::Failure { error } => {
                let plan = conversation.plans.get_mut(plan_id).unwrap();
                let sub_task = &mut plan.tasks[parent_idx].sub_tasks[sub_idx];
                sub_task.status = PlanTaskStatus::Failed;
                sub_task.completed_at_ms = Some(now_ms);
                sub_task.error = Some(error.clone());
            }
            TaskTurnOutcome::Cancelled { reason } => {
                let plan = conversation.plans.get_mut(plan_id).unwrap();
                let sub_task = &mut plan.tasks[parent_idx].sub_tasks[sub_idx];
                sub_task.status = PlanTaskStatus::Cancelled;
                sub_task.completed_at_ms = Some(now_ms);
                sub_task.error = Some(reason.clone());
            }
        }

        // Story 10.6: read failure policy
        let policy = {
            let plans = self.plans.read().unwrap();
            plans
                .get(plan_id)
                .map(|s| s.subtask_failure_policy)
                .unwrap_or_default()
        };

        // If fail_fast and this sub-task failed, skip remaining pending siblings
        if matches!(
            policy,
            crate::domain::models::SubTaskFailurePolicy::FailFast
        ) {
            if let TaskTurnOutcome::Failure { .. } = outcome {
                let plan = conversation.plans.get_mut(plan_id).unwrap();
                let parent = &mut plan.tasks[parent_idx];
                for st in parent.sub_tasks.iter_mut() {
                    if st.status == PlanTaskStatus::Pending {
                        st.status = PlanTaskStatus::Skipped;
                        st.completed_at_ms = Some(now_ms);
                        st.error = Some(format!(
                            "skipped: sibling sub-task {}.{} failed",
                            parent_number, sub_number
                        ));
                    }
                }
            }
        }

        // Clear active sub-task
        {
            let mut plans = self.plans.write().unwrap();
            if let Some(state) = plans.get_mut(plan_id) {
                state.active_sub_tasks.remove(&parent_number);
            }
        }

        // Check if all sub-tasks are terminal
        let all_terminal = {
            let plan = conversation.plans.get(plan_id).unwrap();
            let parent = &plan.tasks[parent_idx];
            parent.sub_tasks.iter().all(|st| is_terminal(st.status))
        };

        if all_terminal {
            self.finalize_parent_task(
                conversation_id,
                plan_id,
                conversation,
                event_emitter,
                parent_number,
                policy,
            );

            // Failure cascade if parent failed (only under fail_fast)
            let parent_failed = {
                let plan = conversation.plans.get(plan_id).unwrap();
                plan.tasks[parent_idx].status == PlanTaskStatus::Failed
            };

            if parent_failed {
                let plan = conversation.plans.get_mut(plan_id).unwrap();
                skip_blocked_tasks(conversation_id, plan_id, parent_number, plan, event_emitter);
            }

            self.advance_after_task(
                conversation_id,
                plan_id,
                conversation,
                event_emitter,
                agents,
                mode,
                None,
            );
        } else {
            // More sub-tasks pending — dispatch next sub-task directly.
            // Cannot route through advance_after_task because the parent is
            // already Running and find_all_eligible filters out Running tasks.
            let next_sub = {
                let plan = conversation.plans.get(plan_id).unwrap();
                let parent = &plan.tasks[parent_idx];
                parent
                    .sub_tasks
                    .iter()
                    .find(|st| st.status == PlanTaskStatus::Pending)
                    .map(|st| st.number)
            };

            if let Some(sub_number) = next_sub {
                let synthetic_task = {
                    let plan = conversation.plans.get(plan_id).unwrap();
                    let parent = &plan.tasks[parent_idx];
                    let sub = &parent.sub_tasks[(sub_number - 1) as usize];
                    PlanTask {
                        number: sub.number,
                        title: sub.title.clone(),
                        description: sub.description.clone(),
                        depends_on: vec![],
                        status: sub.status,
                        started_at_ms: sub.started_at_ms,
                        completed_at_ms: sub.completed_at_ms,
                        result: sub.result.clone(),
                        error: sub.error.clone(),
                        waiting_on: vec![],
                        delegated_to: sub.delegated_to.clone(),
                        sub_tasks: vec![],
                    }
                };
                let plan = conversation.plans.get(plan_id).unwrap();
                if let Some(suggestion) =
                    DelegationDecider::suggest(plan, &synthetic_task, agents, mode)
                {
                    {
                        let mut plans = self.plans.write().unwrap();
                        if let Some(state) = plans.get_mut(plan_id) {
                            state.active_sub_tasks.insert(parent_number, sub_number);
                        }
                    }
                    event_emitter.emit(AppEvent::PlanTaskDelegationRequested {
                        conversation_id: conversation_id.clone(),
                        plan_id: plan_id.to_string(),
                        task_number: parent_number,
                        suggestion,
                    });
                } else {
                    self.dispatch_sub_task(
                        conversation_id,
                        plan_id,
                        conversation,
                        event_emitter,
                        parent_number,
                        sub_number,
                    );
                }
            }
        }
    }

    pub fn advance_after_task(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        agents: &[AgentDef],
        mode: crate::domain::models::PermissionMode,
        force_task_number: Option<u32>,
    ) {
        if let Some(task_number) = force_task_number {
            self.dispatch_single_task(
                conversation_id,
                plan_id,
                conversation,
                event_emitter,
                task_number,
            );
            return;
        }

        let eligible: Vec<u32> = {
            let plan = conversation.plans.get(plan_id).unwrap();
            find_all_eligible(plan).iter().map(|t| t.number).collect()
        };

        if eligible.is_empty() {
            let all_terminal = {
                let plan = conversation.plans.get(plan_id).unwrap();
                plan.tasks.iter().all(|t| is_terminal(t.status))
            };
            if all_terminal {
                finish_plan(conversation_id, plan_id, conversation, event_emitter);
                self.plans.write().unwrap().remove(plan_id);
            } else {
                let blocker_list = {
                    let plan = conversation.plans.get(plan_id).unwrap();
                    plan.tasks
                        .iter()
                        .filter(|t| is_terminal(t.status) && t.status != PlanTaskStatus::Completed)
                        .map(|t| t.number.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let now_ms = chrono::Utc::now().timestamp_millis();
                {
                    let plan = conversation.plans.get_mut(plan_id).unwrap();
                    for task in &mut plan.tasks {
                        if !is_terminal(task.status) {
                            task.status = PlanTaskStatus::Skipped;
                            task.error = Some(format!(
                                "Skipped — blocked by upstream task(s) {}",
                                blocker_list
                            ));
                            task.completed_at_ms = Some(now_ms);
                            event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                                conversation_id: conversation_id.clone(),
                                plan_id: plan_id.to_string(),
                                task_number: task.number,
                                status: PlanTaskStatus::Skipped,
                            });
                        }
                    }
                }
                finish_plan(conversation_id, plan_id, conversation, event_emitter);
                self.plans.write().unwrap().remove(plan_id);
            }
            return;
        }

        let bound = DelegationDecider::fan_out_bound(eligible.len(), 4);
        let mut local_dispatched = false;

        for task_number in eligible.iter().take(bound) {
            let plan = conversation.plans.get(plan_id).unwrap();
            let task = &plan.tasks[(*task_number - 1) as usize];

            // Story 10.6: task with sub-tasks drives its own sub-task execution.
            if task.has_sub_tasks() {
                let now_ms = chrono::Utc::now().timestamp_millis();
                {
                    let plan = conversation.plans.get_mut(plan_id).unwrap();
                    let parent = &mut plan.tasks[(*task_number - 1) as usize];
                    if parent.status != PlanTaskStatus::Running {
                        parent.status = PlanTaskStatus::Running;
                        parent.started_at_ms = Some(now_ms);
                        event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                            conversation_id: conversation_id.clone(),
                            plan_id: plan_id.to_string(),
                            task_number: *task_number,
                            status: PlanTaskStatus::Running,
                        });
                    }
                }

                // Find first pending sub-task
                let next_sub = {
                    let plan = conversation.plans.get(plan_id).unwrap();
                    let parent = &plan.tasks[(*task_number - 1) as usize];
                    parent
                        .sub_tasks
                        .iter()
                        .find(|st| st.status == PlanTaskStatus::Pending)
                        .map(|st| st.number)
                };

                if let Some(sub_number) = next_sub {
                    // Check delegation for sub-task (synthetic PlanTask for decider)
                    let synthetic_task = {
                        let plan = conversation.plans.get(plan_id).unwrap();
                        let parent = &plan.tasks[(*task_number - 1) as usize];
                        let sub = &parent.sub_tasks[(sub_number - 1) as usize];
                        PlanTask {
                            number: sub.number,
                            title: sub.title.clone(),
                            description: sub.description.clone(),
                            depends_on: vec![],
                            status: sub.status,
                            started_at_ms: sub.started_at_ms,
                            completed_at_ms: sub.completed_at_ms,
                            result: sub.result.clone(),
                            error: sub.error.clone(),
                            waiting_on: vec![],
                            delegated_to: sub.delegated_to.clone(),
                            sub_tasks: vec![],
                        }
                    };
                    let plan = conversation.plans.get(plan_id).unwrap();
                    if let Some(suggestion) =
                        DelegationDecider::suggest(plan, &synthetic_task, agents, mode)
                    {
                        {
                            let mut plans = self.plans.write().unwrap();
                            if let Some(state) = plans.get_mut(plan_id) {
                                state.active_sub_tasks.insert(*task_number, sub_number);
                            }
                        }
                        event_emitter.emit(AppEvent::PlanTaskDelegationRequested {
                            conversation_id: conversation_id.clone(),
                            plan_id: plan_id.to_string(),
                            task_number: *task_number,
                            suggestion,
                        });
                    } else if !local_dispatched {
                        self.dispatch_sub_task(
                            conversation_id,
                            plan_id,
                            conversation,
                            event_emitter,
                            *task_number,
                            sub_number,
                        );
                        local_dispatched = true;
                    }
                } else {
                    // All sub-tasks already terminal — finalize parent immediately
                    self.finalize_parent_task(
                        conversation_id,
                        plan_id,
                        conversation,
                        event_emitter,
                        *task_number,
                        crate::domain::models::SubTaskFailurePolicy::default(),
                    );
                }
                continue;
            }

            if let Some(suggestion) = DelegationDecider::suggest(plan, task, agents, mode) {
                event_emitter.emit(AppEvent::PlanTaskDelegationRequested {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    task_number: *task_number,
                    suggestion,
                });
            } else if !local_dispatched {
                self.dispatch_single_task(
                    conversation_id,
                    plan_id,
                    conversation,
                    event_emitter,
                    *task_number,
                );
                local_dispatched = true;
            }
        }
    }

    fn dispatch_single_task(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        task_number: u32,
    ) {
        let plan = conversation.plans.get_mut(plan_id).unwrap();
        let task = &mut plan.tasks[(task_number - 1) as usize];
        let prompt = format_task_prompt(task);
        task.status = PlanTaskStatus::Running;
        task.started_at_ms = Some(chrono::Utc::now().timestamp_millis());

        {
            let mut plans = self.plans.write().unwrap();
            if let Some(state) = plans.get_mut(plan_id) {
                state
                    .task_cancels
                    .insert(task_number, CancellationToken::new());
            }
        }

        event_emitter.emit(AppEvent::PlanTaskStatusChanged {
            conversation_id: conversation_id.clone(),
            plan_id: plan_id.to_string(),
            task_number,
            status: PlanTaskStatus::Running,
        });
        event_emitter.emit(AppEvent::AgentThenSubmit {
            conversation_id: conversation_id.clone(),
            text: prompt,
            synthetic: true,
        });
    }

    /// Story 10.6 — dispatch a single sub-task inside a parent task.
    fn dispatch_sub_task(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        parent_number: u32,
        sub_number: u32,
    ) {
        let prompt = {
            let plan = conversation.plans.get(plan_id).unwrap();
            let parent = &plan.tasks[(parent_number - 1) as usize];
            let sub_task = &parent.sub_tasks[(sub_number - 1) as usize];
            format_sub_task_prompt(parent, sub_task)
        };

        {
            let plan = conversation.plans.get_mut(plan_id).unwrap();
            let parent = &mut plan.tasks[(parent_number - 1) as usize];
            let sub_task = &mut parent.sub_tasks[(sub_number - 1) as usize];
            sub_task.status = PlanTaskStatus::Running;
            sub_task.started_at_ms = Some(chrono::Utc::now().timestamp_millis());
        }

        {
            let mut plans = self.plans.write().unwrap();
            if let Some(state) = plans.get_mut(plan_id) {
                state.active_sub_tasks.insert(parent_number, sub_number);
                state
                    .task_cancels
                    .insert(parent_number * 1000 + sub_number, CancellationToken::new());
            }
        }

        event_emitter.emit(AppEvent::PlanTaskStatusChanged {
            conversation_id: conversation_id.clone(),
            plan_id: plan_id.to_string(),
            task_number: parent_number,
            status: PlanTaskStatus::Running,
        });
        event_emitter.emit(AppEvent::AgentThenSubmit {
            conversation_id: conversation_id.clone(),
            text: prompt,
            synthetic: true,
        });
    }

    /// Story 10.6 — aggregate sub-task results into a parent `TaskResult`.
    pub fn aggregate_sub_task_results(
        parent: &crate::domain::models::plan::PlanTask,
    ) -> TaskResult {
        let mut lines = Vec::with_capacity(parent.sub_tasks.len());
        let mut total_tool_calls = 0u32;
        let mut total_tokens = 0u32;

        for st in &parent.sub_tasks {
            let icon = match st.status {
                PlanTaskStatus::Completed => "✅",
                PlanTaskStatus::Failed => "❌",
                PlanTaskStatus::Skipped => "⏭️",
                PlanTaskStatus::Cancelled => "🚫",
                _ => "⏳",
            };
            let first_line = st
                .result
                .as_ref()
                .map(|r| {
                    total_tool_calls += r.tool_call_count;
                    if let Some(tc) = r.token_count {
                        total_tokens += tc;
                    }
                    r.text.lines().next().unwrap_or("(no output)").to_string()
                })
                .or_else(|| {
                    st.error
                        .as_ref()
                        .map(|e| e.lines().next().unwrap_or("(error)").to_string())
                })
                .unwrap_or_else(|| "(no output)".to_string());
            let mut line = format!(
                "- [{}] Sub-task {}.{}: {} — {}",
                icon, parent.number, st.number, st.title, first_line
            );
            if let Some(ref info) = st.delegated_to {
                if let Some(ref task_id) = info.spool_task_id {
                    line.push_str(&format!(
                        " (full output: read_task_output(\"{}\"))",
                        task_id
                    ));
                }
            }
            lines.push(line);
        }

        TaskResult {
            text: lines.join("\n"),
            tool_call_count: total_tool_calls,
            token_count: if total_tokens > 0 {
                Some(total_tokens)
            } else {
                None
            },
        }
    }

    /// Story 10.6 — finalize a parent task once all sub-tasks are terminal.
    fn finalize_parent_task(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        parent_number: u32,
        policy: crate::domain::models::SubTaskFailurePolicy,
    ) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let new_status = {
            let plan = conversation.plans.get_mut(plan_id).unwrap();
            let parent = &mut plan.tasks[(parent_number - 1) as usize];
            let all_terminal = parent.sub_tasks.iter().all(|st| is_terminal(st.status));
            if !all_terminal {
                tracing::warn!("finalize_parent_task called before all sub-tasks terminal");
                return;
            }
            let any_failed = parent
                .sub_tasks
                .iter()
                .any(|st| st.status == PlanTaskStatus::Failed);
            let status = if any_failed {
                match policy {
                    crate::domain::models::SubTaskFailurePolicy::FailFast => PlanTaskStatus::Failed,
                    crate::domain::models::SubTaskFailurePolicy::BestEffort => {
                        PlanTaskStatus::Completed
                    }
                }
            } else {
                PlanTaskStatus::Completed
            };
            let failed_count = parent
                .sub_tasks
                .iter()
                .filter(|st| st.status == PlanTaskStatus::Failed)
                .count();
            let error = if failed_count > 0 {
                Some(format!(
                    "{}/{} sub-tasks failed",
                    failed_count,
                    parent.sub_tasks.len()
                ))
            } else {
                None
            };
            parent.status = status;
            parent.completed_at_ms = Some(now_ms);
            parent.error = error;
            parent.result = Some(Self::aggregate_sub_task_results(parent));
            status
        };

        event_emitter.emit(AppEvent::PlanTaskStatusChanged {
            conversation_id: conversation_id.clone(),
            plan_id: plan_id.to_string(),
            task_number: parent_number,
            status: new_status,
        });
    }

    /// Story 10.5 — spawn a subagent for the named task, await terminal status,
    /// then route the outcome through `on_turn_complete` so the 6-2a FSM
    /// (Running → Completed/Failed/Cancelled) and the unified `PlanSummary`
    /// contract remain intact.
    ///
    /// On `Err(SubagentError)` returns immediately with `Err(...)` so the
    /// caller can fall back to local sequential execution.
    pub async fn delegate_task(
        self: Arc<Self>,
        conversation_id: &ConversationId,
        plan_id: &str,
        task_number: u32,
        agent_def: &AgentDef,
        runner: Arc<dyn crate::domain::ports::SubagentRunner>,
        spool: Arc<crate::infrastructure::subagent::SubagentSpool>,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: Arc<dyn EventEmitter>,
        default_model: &str,
    ) -> Result<(), DelegateTaskError> {
        // Story 10.6: check if this task_number has an active sub-task
        let active_sub = {
            let plans = self.plans.read().unwrap();
            plans
                .get(plan_id)
                .and_then(|s| s.active_sub_tasks.get(&task_number).copied())
        };

        let plan =
            conversation
                .plans
                .get_mut(plan_id)
                .ok_or_else(|| DelegateTaskError::NotFound {
                    plan_id: plan_id.to_string(),
                    task_number,
                })?;
        let idx = (task_number - 1) as usize;
        if idx >= plan.tasks.len() {
            return Err(DelegateTaskError::NotFound {
                plan_id: plan_id.to_string(),
                task_number,
            });
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let agent_name = agent_def.name.clone();

        let (spec, is_sub_task) = if let Some(sub_number) = active_sub {
            // Delegating a sub-task
            let sub_idx = (sub_number - 1) as usize;
            if sub_idx >= plan.tasks[idx].sub_tasks.len()
                || plan.tasks[idx].sub_tasks[sub_idx].status != PlanTaskStatus::Pending
            {
                return Err(DelegateTaskError::NotFound {
                    plan_id: plan_id.to_string(),
                    task_number,
                });
            }
            let spec = LaunchSpecBuilder::from_sub_task(
                &plan.tasks[idx],
                &plan.tasks[idx].sub_tasks[sub_idx],
                agent_def,
                default_model,
                0,
                None,
            );
            plan.tasks[idx].sub_tasks[sub_idx].delegated_to = Some(DelegationInfo {
                agent_name: agent_name.clone(),
                agent_id: None,
                delegated_at_ms: now_ms,
                spool_task_id: None,
            });
            plan.tasks[idx].sub_tasks[sub_idx].status = PlanTaskStatus::Running;
            plan.tasks[idx].sub_tasks[sub_idx].started_at_ms = Some(now_ms);
            (spec, true)
        } else {
            // Delegating a top-level task
            if plan.tasks[idx].status != PlanTaskStatus::Pending {
                return Err(DelegateTaskError::NotFound {
                    plan_id: plan_id.to_string(),
                    task_number,
                });
            }
            let spec = LaunchSpecBuilder::from_plan_task(
                &plan.tasks[idx],
                agent_def,
                default_model,
                0,
                None,
            );
            plan.tasks[idx].delegated_to = Some(DelegationInfo {
                agent_name: agent_name.clone(),
                agent_id: None,
                delegated_at_ms: now_ms,
                spool_task_id: None,
            });
            plan.tasks[idx].status = PlanTaskStatus::Running;
            plan.tasks[idx].started_at_ms = Some(now_ms);
            (spec, false)
        };

        {
            let mut plans = self.plans.write().unwrap();
            if let Some(state) = plans.get_mut(plan_id) {
                state
                    .task_cancels
                    .insert(task_number, CancellationToken::new());
            }
        }

        event_emitter.emit(AppEvent::PlanTaskStatusChanged {
            conversation_id: conversation_id.clone(),
            plan_id: plan_id.to_string(),
            task_number,
            status: PlanTaskStatus::Running,
        });

        // 4. Spawn the runner
        let child_cancel = {
            let plans = self.plans.read().unwrap();
            plans
                .get(plan_id)
                .and_then(|s| s.task_cancels.get(&task_number))
                .cloned()
                .unwrap_or_default()
                .child_token()
        };

        let task_handle = match runner.launch(spec, child_cancel, None).await {
            Ok(handle) => handle,
            Err(e) => {
                // Revert on launch failure
                let plan = conversation.plans.get_mut(plan_id).unwrap();
                if is_sub_task {
                    if let Some(sub_number) = active_sub {
                        let sub_idx = (sub_number - 1) as usize;
                        plan.tasks[idx].sub_tasks[sub_idx].delegated_to = None;
                        plan.tasks[idx].sub_tasks[sub_idx].status = PlanTaskStatus::Pending;
                        plan.tasks[idx].sub_tasks[sub_idx].started_at_ms = None;
                    }
                    {
                        let mut plans = self.plans.write().unwrap();
                        if let Some(state) = plans.get_mut(plan_id) {
                            state.active_sub_tasks.remove(&task_number);
                        }
                    }
                } else {
                    plan.tasks[idx].delegated_to = None;
                    plan.tasks[idx].status = PlanTaskStatus::Pending;
                    plan.tasks[idx].started_at_ms = None;
                }
                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    task_number,
                    status: PlanTaskStatus::Pending,
                });
                return Err(DelegateTaskError::LaunchFailed(e));
            }
        };

        // 5. Update delegated_to with agent_id and spool_task_id
        {
            let plan = conversation.plans.get_mut(plan_id).unwrap();
            if is_sub_task {
                if let Some(sub_number) = active_sub {
                    let sub_idx = (sub_number - 1) as usize;
                    if let Some(ref mut info) = plan.tasks[idx].sub_tasks[sub_idx].delegated_to {
                        info.agent_id = Some(task_handle.agent_id.as_str().to_string());
                        info.spool_task_id = Some(task_handle.task_id.clone());
                    }
                }
            } else if let Some(ref mut info) = plan.tasks[idx].delegated_to {
                info.agent_id = Some(task_handle.agent_id.as_str().to_string());
                info.spool_task_id = Some(task_handle.task_id.clone());
            }
        }

        // 6. Emit live-progress notice
        let delegated_title = if let Some(sub_number) = active_sub {
            let sub_idx = (sub_number - 1) as usize;
            format!(
                "{}.{} {}",
                task_number,
                sub_number,
                conversation.plans[plan_id].tasks[idx].sub_tasks[sub_idx].title
            )
        } else {
            conversation.plans[plan_id].tasks[idx].title.clone()
        };
        event_emitter.emit(AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.clone()),
            level: NoticeLevel::Info,
            message: format!("⏳ Delegated to {}: \"{}\"", agent_name, delegated_title),
        });

        // 7. Spawn background task to await terminal status and emit bridge event
        let conversation_id_clone = conversation_id.clone();
        let plan_id_string = plan_id.to_string();
        let task_number_clone = task_number;
        let spool_clone = spool.clone();
        let event_emitter_clone = event_emitter.clone();

        let mut terminal_received = false;
        let cid_for_fallback = conversation_id_clone.clone();
        let pid_for_fallback = plan_id_string.clone();
        tokio::spawn(async move {
            let mut status_rx = task_handle.status_rx;
            while let Some(status) = status_rx.recv().await {
                if matches!(
                    status,
                    crate::domain::models::NodeState::Completed
                        | crate::domain::models::NodeState::Failed
                        | crate::domain::models::NodeState::Cancelled
                ) {
                    terminal_received = true;
                    let tail = spool_clone
                        .read_tail(&task_handle.task_id, 8192)
                        .await
                        .unwrap_or_default();
                    let outcome = match status {
                        crate::domain::models::NodeState::Completed => TaskTurnOutcome::Success {
                            result_text: tail,
                            tool_call_count: 0,
                            token_count: None,
                        },
                        crate::domain::models::NodeState::Failed => {
                            let error =
                                tail.lines().last().unwrap_or("(no error tail)").to_string();
                            TaskTurnOutcome::Failure {
                                error: format!("Subagent failed: {}", error),
                            }
                        }
                        crate::domain::models::NodeState::Cancelled => TaskTurnOutcome::Cancelled {
                            reason: "Delegated subagent killed".into(),
                        },
                        _ => unreachable!(),
                    };

                    let _ = event_emitter_clone.emit(AppEvent::PlanTaskDelegationCompleted {
                        conversation_id: conversation_id_clone,
                        plan_id: plan_id_string,
                        task_number: task_number_clone,
                        outcome,
                    });
                    break;
                }
            }
            if !terminal_received {
                let _ = event_emitter_clone.emit(AppEvent::PlanTaskDelegationCompleted {
                    conversation_id: cid_for_fallback,
                    plan_id: pid_for_fallback,
                    task_number: task_number_clone,
                    outcome: TaskTurnOutcome::Failure {
                        error: "Subagent status channel closed unexpectedly".to_string(),
                    },
                });
            }
        });

        Ok(())
    }

    pub async fn snapshot(&self, plan_id: &str) -> Option<PlanRuntimeState> {
        let plans = self.plans.read().unwrap();
        // G3-P12: clone actual tokens so callers (6.4) can cancel live tasks
        plans.get(plan_id).map(|s| PlanRuntimeState {
            conversation_id: s.conversation_id.clone(),
            plan_id: s.plan_id.clone(),
            task_cancels: s.task_cancels.clone(),
            task_start_assistant_count: s.task_start_assistant_count,
            pause_pending_tasks: s.pause_pending_tasks.clone(),
            whole_plan_cancel_pending: s.whole_plan_cancel_pending,
            pending_deviation: s.pending_deviation.clone(),
            active_sub_tasks: s.active_sub_tasks.clone(),
            subtask_failure_policy: s.subtask_failure_policy,
        })
    }

    pub async fn mark_pause_pending(&self, plan_id: &str, task_number: u32) {
        let mut plans = self.plans.write().unwrap();
        if let Some(state) = plans.get_mut(plan_id) {
            state.pause_pending_tasks.insert(task_number);
        }
    }

    #[allow(dead_code)]
    pub async fn mark_whole_plan_cancel_pending(&self, plan_id: &str) {
        let mut plans = self.plans.write().unwrap();
        if let Some(state) = plans.get_mut(plan_id) {
            state.whole_plan_cancel_pending = true;
        }
    }

    pub async fn mark_deviation_pending(&self, plan_id: &str, kind: PlanDeviationKind) {
        let mut plans = self.plans.write().unwrap();
        if let Some(state) = plans.get_mut(plan_id) {
            state.pending_deviation = Some(kind);
        }
    }

    pub async fn clear_deviation_pending(&self, plan_id: &str) {
        let mut plans = self.plans.write().unwrap();
        if let Some(state) = plans.get_mut(plan_id) {
            state.pending_deviation = None;
        }
    }

    /// Story 6.4: remove runtime state for a plan that has been cancelled
    /// without a running task (no-turn-complete path).
    pub async fn remove_plan(&self, plan_id: &str) {
        let mut plans = self.plans.write().unwrap();
        plans.remove(plan_id);
    }

    async fn has_pending_deviation(&self, plan_id: &str) -> bool {
        let plans = self.plans.read().unwrap();
        plans
            .get(plan_id)
            .is_some_and(|s| s.pending_deviation.is_some())
    }

    /// Public wrapper for the private `advance_after_task`. Called by 6-4
    /// handlers after a status mutation (pause-resume, skip, retry, edit).
    /// Short-circuits if pending_deviation is set.
    pub async fn resume_advance(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
        agents: &[AgentDef],
        mode: crate::domain::models::PermissionMode,
    ) {
        if self.has_pending_deviation(plan_id).await {
            tracing::debug!(
                "PlanRuntime::resume_advance: short-circuited — pending_deviation is set"
            );
            return;
        }
        self.advance_after_task(
            conversation_id,
            plan_id,
            conversation,
            event_emitter,
            agents,
            mode,
            None,
        );
    }

    /// Returns true if the running task for this plan has received a new assistant
    /// message since it started — i.e. the turn is ready to be classified.
    pub fn can_complete_turn(&self, plan_id: &str, current_assistant_count: usize) -> bool {
        let plans = self.plans.read().unwrap();
        plans
            .get(plan_id)
            .is_some_and(|s| current_assistant_count > s.task_start_assistant_count)
    }

    pub fn classify_outcome(
        last_assistant_msg_content: &str,
        tool_call_count: u32,
        any_tool_succeeded: bool,
        stop_reason: Option<crate::domain::models::StopReason>,
    ) -> TaskTurnOutcome {
        if stop_reason == Some(crate::domain::models::StopReason::Cancelled) {
            return TaskTurnOutcome::Cancelled {
                reason: "turn-cancelled".to_string(),
            };
        }

        let failure_keywords = [
            "failed",
            "error",
            "cannot",
            "unable to",
            "could not",
            "not possible",
        ];
        let content_lower = last_assistant_msg_content.to_lowercase();
        let looks_failed =
            !any_tool_succeeded && failure_keywords.iter().any(|kw| content_lower.contains(kw));

        if looks_failed {
            TaskTurnOutcome::Failure {
                error: extract_first_sentence(last_assistant_msg_content),
            }
        } else {
            // G6-P21bis (Story 6.3-FU3): store full result; downstream consumers
            // (drill-down, clipboard, summary) cap at render time. Storage no
            // longer truncates — see AC1/AC2.
            TaskTurnOutcome::Success {
                result_text: last_assistant_msg_content.to_string(),
                tool_call_count,
                token_count: None,
            }
        }
    }
}

fn find_next_eligible(plan: &Plan) -> Option<&PlanTask> {
    let statuses: Vec<PlanTaskStatus> = plan.tasks.iter().map(|t| t.status).collect();
    for (i, task) in plan.tasks.iter().enumerate() {
        if is_terminal(task.status) {
            continue;
        }
        // G2-P6: guard against zero (1-indexed; 0 is never valid) and out-of-range deps
        let deps_satisfied = task.depends_on.iter().all(|dep| {
            if *dep == 0 {
                return false; // invalid dep number
            }
            let dep_idx = (*dep - 1) as usize;
            dep_idx < statuses.len() && statuses[dep_idx] == PlanTaskStatus::Completed
        });
        if deps_satisfied {
            return Some(&plan.tasks[i]);
        }
        // Sequential walk guarantees backward-only deps are always satisfied before
        // we reach a task; unsatisfied deps here mean upstream failure/cancellation.
        // G1-P1: the caller (advance_after_task) handles the deadlock case.
    }
    None
}

/// Story 10.5: return ALL tasks eligible for parallel delegation.
/// A task is eligible when:
/// - Its status is not terminal and not Running
/// - All its `depends_on` entries are Completed
pub fn find_all_eligible(plan: &Plan) -> Vec<&PlanTask> {
    let statuses: Vec<PlanTaskStatus> = plan.tasks.iter().map(|t| t.status).collect();
    plan.tasks
        .iter()
        .filter(|task| {
            if is_terminal(task.status) || task.status == PlanTaskStatus::Running {
                return false;
            }
            task.depends_on.iter().all(|dep| {
                if *dep == 0 {
                    return false;
                }
                let idx = (*dep - 1) as usize;
                idx < statuses.len() && statuses[idx] == PlanTaskStatus::Completed
            })
        })
        .collect()
}

fn skip_blocked_tasks(
    conversation_id: &ConversationId,
    plan_id: &str,
    failed_task_number: u32,
    plan: &mut Plan,
    event_emitter: &dyn EventEmitter,
) {
    for task in &mut plan.tasks {
        if is_terminal(task.status) {
            continue;
        }
        if task.depends_on.contains(&failed_task_number) {
            task.status = PlanTaskStatus::Skipped;
            task.error = Some(format!(
                "Skipped — depends on failed task {}",
                failed_task_number
            ));
            task.completed_at_ms = Some(chrono::Utc::now().timestamp_millis());
            event_emitter.emit(AppEvent::SystemNotice {
                conversation_id: Some(conversation_id.clone()),
                level: NoticeLevel::Warning,
                message: format!(
                    "Auto-skipped task {} ({}) — depends on failed task {}",
                    task.number, task.title, failed_task_number
                ),
            });
            event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                conversation_id: conversation_id.clone(),
                plan_id: plan_id.to_string(),
                task_number: task.number,
                status: PlanTaskStatus::Skipped,
            });
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        let snapshot: Vec<(u32, PlanTaskStatus)> =
            plan.tasks.iter().map(|t| (t.number, t.status)).collect();
        for task in &mut plan.tasks {
            if is_terminal(task.status) {
                continue;
            }
            let blocked = task.depends_on.iter().any(|dep| {
                snapshot
                    .iter()
                    .any(|(n, s)| *n == *dep && *s != PlanTaskStatus::Completed)
            });
            if blocked {
                task.status = PlanTaskStatus::Skipped;
                let blocked_by: Vec<String> = task
                    .depends_on
                    .iter()
                    .filter(|dep| {
                        snapshot
                            .iter()
                            .any(|(n, s)| *n == **dep && *s != PlanTaskStatus::Completed)
                    })
                    .map(|n| n.to_string())
                    .collect();
                task.error = Some(format!(
                    "Skipped — blocked by upstream task(s) {}",
                    blocked_by.join(", ")
                ));
                task.completed_at_ms = Some(chrono::Utc::now().timestamp_millis());
                event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                    conversation_id: conversation_id.clone(),
                    plan_id: plan_id.to_string(),
                    task_number: task.number,
                    status: PlanTaskStatus::Skipped,
                });
                changed = true;
            }
        }
    }
}

pub(crate) fn finish_plan(
    conversation_id: &ConversationId,
    plan_id: &str,
    conversation: &mut crate::domain::models::Conversation,
    event_emitter: &dyn EventEmitter,
) {
    let (
        plan_title,
        tasks_snapshot,
        completed,
        failed,
        skipped,
        total,
        total_elapsed_ms,
        total_tokens,
    ) = {
        let plan = conversation.plans.get_mut(plan_id).unwrap();
        let total = plan.tasks.len() as u32;
        let completed = plan
            .tasks
            .iter()
            .filter(|t| t.status == PlanTaskStatus::Completed)
            .count() as u32;
        let failed = plan
            .tasks
            .iter()
            .filter(|t| t.status == PlanTaskStatus::Failed)
            .count() as u32;
        let skipped = plan
            .tasks
            .iter()
            .filter(|t| t.status == PlanTaskStatus::Skipped)
            .count() as u32;
        let total_elapsed_ms: i64 = plan.tasks.iter().filter_map(|t| t.elapsed_ms()).sum();
        let total_tokens: u32 = plan
            .tasks
            .iter()
            .filter_map(|t| t.result.as_ref().and_then(|r| r.token_count))
            .sum();

        // Preserve Cancelled — whole-plan-cancel race set it before reaching here.
        if plan.status != PlanStatus::Cancelled {
            plan.status = PlanStatus::Completed;
        }

        let mut snapshot = Vec::new();
        for t in &plan.tasks {
            snapshot.push((
                t.number,
                t.title.clone(),
                t.status,
                t.elapsed_ms(),
                t.error.clone(),
                t.result.clone(),
            ));
            for st in &t.sub_tasks {
                let sub_title = st.title.clone();
                snapshot.push((
                    t.number * 1000 + st.number,
                    sub_title,
                    st.status,
                    st.elapsed_ms(),
                    st.error.clone(),
                    st.result.clone(),
                ));
            }
        }

        (
            plan.title.clone(),
            snapshot,
            completed,
            failed,
            skipped,
            total,
            total_elapsed_ms,
            total_tokens,
        )
    };

    let summary = build_summary_markdown(
        &plan_title,
        completed,
        total,
        failed,
        skipped,
        total_elapsed_ms,
        total_tokens,
        &tasks_snapshot,
    );

    let msg = ChatMessage {
        id: generate_message_id(),
        role: MessageRole::Assistant,
        content: summary,
        content_blocks: vec![ContentBlockType::PlanSummary],
        tool_calls: vec![],
        created_at: crate::domain::models::session_meta::now_unix(),
        token_count: None,
        stop_reason: None,
        synthetic: true,
        images: vec![],
        origin: crate::domain::models::ChannelKind::Terminal,
    };
    conversation.messages.push(msg);

    event_emitter.emit(AppEvent::PlanCompleted {
        conversation_id: conversation_id.clone(),
        plan_id: plan_id.to_string(),
        completed,
        failed,
        skipped,
        total_elapsed_ms,
    });
}

fn build_summary_markdown(
    plan_title: &str,
    completed: u32,
    total: u32,
    failed: u32,
    skipped: u32,
    total_elapsed_ms: i64,
    total_tokens: u32,
    tasks: &[TaskSummary],
) -> String {
    let mut md = format!("## Plan complete: {}\n\n", plan_title);

    // G4-P13: spec mandates · (middle dot U+00B7) between elapsed/token fields
    let mut count_parts = vec![format!("{}/{} tasks completed", completed, total)];
    if failed > 0 {
        count_parts.push(format!("{} failed", failed));
    }
    if skipped > 0 {
        count_parts.push(format!("{} skipped", skipped));
    }
    let mut summary = count_parts.join(", ");
    summary.push_str(&format!(
        " · ~{} elapsed",
        format_elapsed_ms(total_elapsed_ms)
    ));
    if total_tokens > 0 {
        summary.push_str(&format!(" · {} tokens", total_tokens));
    }
    md.push_str(&format!("**Summary:** {}\n\n", summary));

    md.push_str("| # | Task | Status | Elapsed |\n");
    md.push_str("|---|------|--------|---------|\n");
    for (i, (number, title, status, elapsed, error, _result)) in tasks.iter().enumerate() {
        if *number >= 1000 {
            // Sub-task row: number format is parent * 1000 + sub
            let parent_num = number / 1000;
            let sub_num = number % 1000;
            let status_str = match status {
                PlanTaskStatus::Completed => "✓".to_string(),
                PlanTaskStatus::Failed => format!(
                    "✗ {}",
                    error
                        .as_deref()
                        .unwrap_or("error")
                        .lines()
                        .next()
                        .unwrap_or("")
                ),
                PlanTaskStatus::Skipped => format!(
                    "⏭ {}",
                    error
                        .as_deref()
                        .unwrap_or("skipped")
                        .lines()
                        .next()
                        .unwrap_or("")
                ),
                _ => format!("{:?}", status),
            };
            let elapsed_str = match *elapsed {
                Some(ms) => format_elapsed_ms(ms),
                None => "—".to_string(),
            };
            md.push_str(&format!(
                "| {}.{} | {} | {} | {} |\n",
                parent_num, sub_num, title, status_str, elapsed_str
            ));
            continue;
        }

        // Count sub-tasks for this parent
        let sub_count = tasks[i + 1..]
            .iter()
            .take_while(|(n, _, _, _, _, _)| *n >= 1000 && *n / 1000 == *number)
            .count();
        let completed_subs = tasks[i + 1..]
            .iter()
            .take_while(|(n, _, _, _, _, _)| *n >= 1000 && *n / 1000 == *number)
            .filter(|(_, _, s, _, _, _)| {
                *s == PlanTaskStatus::Completed || *s == PlanTaskStatus::Skipped
            })
            .count();

        let title_with_fraction = if sub_count > 0 {
            format!("{} ({}/{} sub-tasks)", title, completed_subs, sub_count)
        } else {
            title.clone()
        };

        let status_str = match status {
            PlanTaskStatus::Completed => "✓".to_string(),
            PlanTaskStatus::Failed => format!(
                "✗ {}",
                error
                    .as_deref()
                    .unwrap_or("error")
                    .lines()
                    .next()
                    .unwrap_or("")
            ),
            PlanTaskStatus::Skipped => format!(
                "⏭ {}",
                error
                    .as_deref()
                    .unwrap_or("skipped")
                    .lines()
                    .next()
                    .unwrap_or("")
            ),
            _ => format!("{:?}", status),
        };
        let elapsed_str = match *elapsed {
            Some(ms) => format_elapsed_ms(ms),
            None => "—".to_string(),
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            number, title_with_fraction, status_str, elapsed_str
        ));
    }

    for (number, _title, status, _elapsed, _error, result) in tasks {
        // Skip sub-task rows — their results are aggregated into the parent
        if *number >= 1000 {
            continue;
        }
        if *status == PlanTaskStatus::Completed {
            if let Some(r) = result {
                if !r.text.is_empty() {
                    // G2-P5: use char boundary to avoid panic on multibyte chars
                    let display = if r.text.len() > 500 {
                        let boundary = r
                            .text
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i < 500)
                            .last()
                            .unwrap_or(0);
                        format!("{} (truncated)", &r.text[..boundary])
                    } else {
                        r.text.clone()
                    };
                    md.push_str(&format!("\n**Task {} result:**\n{}\n", number, display));
                }
            }
        }
    }

    md
}

pub fn format_elapsed_ms(ms: i64) -> String {
    let total_secs = (ms.max(0) as u64) / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    match (hours, mins, secs) {
        (0, 0, s) => format!("{}s", s),
        (0, m, s) => format!("{}m {}s", m, s),
        (h, m, s) => format!("{}h {}m {}s", h, m, s),
    }
}

pub fn format_task_prompt(task: &PlanTask) -> String {
    if task.description.is_empty() {
        format!("Now executing task {}: {}.", task.number, task.title)
    } else {
        format!(
            "Now executing task {}: {}.\n\n{}",
            task.number, task.title, task.description
        )
    }
}

/// Story 10.6: format a sub-task prompt carrying the parent task's title as context.
pub fn format_sub_task_prompt(
    parent: &PlanTask,
    sub_task: &crate::domain::models::plan::PlanSubTask,
) -> String {
    if sub_task.description.is_empty() {
        format!(
            "(Sub-task {}.{} of task {}: {}) {}",
            parent.number, sub_task.number, parent.number, parent.title, sub_task.title
        )
    } else {
        format!(
            "(Sub-task {}.{} of task {}: {}) {}\n\n{}",
            parent.number,
            sub_task.number,
            parent.number,
            parent.title,
            sub_task.title,
            sub_task.description
        )
    }
}

fn is_terminal(status: PlanTaskStatus) -> bool {
    matches!(
        status,
        PlanTaskStatus::Completed
            | PlanTaskStatus::Failed
            | PlanTaskStatus::Skipped
            | PlanTaskStatus::Cancelled
    )
    // Paused is non-terminal — eligibility skips it but resume can flip it back.
}

/// Story 6.4: public version of `is_terminal` for use by adapters/event loop.
pub fn is_terminal_pub(status: PlanTaskStatus) -> bool {
    is_terminal(status)
}

/// Story 6.4: validate a reorder move.
/// `i` = current index of `task_number` in plan.tasks, `j` = target index.
/// Pure function; no I/O.
pub fn validate_reorder(plan: &Plan, task_number: u32, j: usize) -> Result<(), String> {
    let i = plan
        .tasks
        .iter()
        .position(|t| t.number == task_number)
        .ok_or_else(|| format!("Task {} not found in plan", task_number))?;

    if i == j {
        return Ok(());
    }

    if plan.tasks[i].status != PlanTaskStatus::Pending {
        return Err("Only pending tasks can be reordered".to_string());
    }

    // Can't push before a task it depends on
    for dep_n in &plan.tasks[i].depends_on {
        if let Some(dep_pos) = plan.tasks.iter().position(|t| &t.number == dep_n) {
            if dep_pos >= j {
                return Err("Would push before a task it depends on".to_string());
            }
        }
    }

    // Can't push past a task that depends on it
    for t in &plan.tasks {
        if t.depends_on.contains(&task_number) {
            if plan
                .tasks
                .iter()
                .position(|pt| pt.number == t.number)
                .unwrap_or(usize::MAX)
                <= j
            {
                return Err("Would push past a task that depends on it".to_string());
            }
        }
    }

    Ok(())
}

fn extract_first_sentence(text: &str) -> String {
    let text = text.trim();
    if let Some(pos) = text.find('.') {
        text[..=pos].to_string()
    } else if let Some(pos) = text.find('\n') {
        text[..pos].to_string()
    } else {
        let max = text.len().min(200);
        text[..max].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_outcome_success_on_positive_message() {
        let result = PlanRuntime::classify_outcome("Updated 4 files. Tests pass.", 2, true, None);
        match result {
            TaskTurnOutcome::Success { .. } => {}
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn classify_outcome_failure_on_cannot() {
        let result =
            PlanRuntime::classify_outcome("I cannot find the auth module.", 0, false, None);
        match result {
            TaskTurnOutcome::Failure { error } => {
                assert!(error.contains("cannot"));
            }
            other => panic!("Expected Failure, got {:?}", other),
        }
    }

    #[test]
    fn classify_outcome_success_with_tool_success_overrules_error_keyword() {
        let result = PlanRuntime::classify_outcome(
            "Tried to read the file but encountered an error. Continuing with cached version which worked.",
            1,
            true,
            None,
        );
        match result {
            TaskTurnOutcome::Success { .. } => {}
            other => panic!("Expected Success (tool succeeded), got {:?}", other),
        }
    }

    #[test]
    fn classify_outcome_cancelled() {
        let result = PlanRuntime::classify_outcome(
            "partial",
            0,
            false,
            Some(crate::domain::models::StopReason::Cancelled),
        );
        match result {
            TaskTurnOutcome::Cancelled { reason } => {
                assert_eq!(reason, "turn-cancelled");
            }
            other => panic!("Expected Cancelled, got {:?}", other),
        }
    }

    #[test]
    fn format_task_prompt_with_description() {
        let task = PlanTask {
            number: 2,
            title: "Extract middleware".to_string(),
            description: "Survey the existing auth middleware.".to_string(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        };
        let prompt = format_task_prompt(&task);
        assert!(prompt.starts_with("Now executing task 2: Extract middleware."));
        assert!(prompt.contains("Survey the existing auth middleware."));
    }

    #[test]
    fn format_task_prompt_without_description() {
        let task = PlanTask {
            number: 1,
            title: "Setup".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        };
        let prompt = format_task_prompt(&task);
        assert_eq!(prompt, "Now executing task 1: Setup.");
        assert!(!prompt.contains('\n'));
    }

    #[test]
    fn format_elapsed_ms_various() {
        assert_eq!(format_elapsed_ms(500), "0s");
        assert_eq!(format_elapsed_ms(12_000), "12s");
        assert_eq!(format_elapsed_ms(133_000), "2m 13s");
        assert_eq!(format_elapsed_ms(3_864_000), "1h 4m 24s");
    }

    #[test]
    fn build_summary_markdown_basic() {
        let tasks = vec![
            (
                1u32,
                "Setup".to_string(),
                PlanTaskStatus::Completed,
                Some(12_000i64),
                None,
                Some(TaskResult {
                    text: "Done".to_string(),
                    tool_call_count: 2,
                    token_count: Some(100),
                }),
            ),
            (
                2u32,
                "Build".to_string(),
                PlanTaskStatus::Failed,
                Some(4_000i64),
                Some("File not found".to_string()),
                None,
            ),
        ];
        let md = build_summary_markdown("Test Plan", 1, 2, 1, 0, 16_000, 100, &tasks);
        assert!(md.contains("Plan complete: Test Plan"));
        assert!(md.contains("1/2 tasks completed"));
        assert!(md.contains("1 failed"));
        assert!(md.contains("~16s elapsed"));
        assert!(md.contains("100 tokens"));
        assert!(md.contains("| # | Task |"));
        assert!(md.contains("✓"));
        assert!(md.contains("✗"));
        assert!(md.contains("**Task 1 result:**"));
    }

    // Story 6.3-FU3 AC3: even after storage no longer truncates, the
    // chat-summary projection MUST still cap each completed task's result
    // entry at 500 bytes with the existing `(truncated)` suffix. This is the
    // independent invariant that 6.3-FU3 deliberately preserves.
    #[test]
    fn plan_summary_md_caps_at_500_bytes_independent_of_storage() {
        let big = "z".repeat(100 * 1024); // 100 KiB
        let tasks = vec![(
            1u32,
            "Big".to_string(),
            PlanTaskStatus::Completed,
            Some(1_000i64),
            None,
            Some(TaskResult {
                text: big,
                tool_call_count: 0,
                token_count: None,
            }),
        )];
        let md = build_summary_markdown("P", 1, 1, 0, 0, 1_000, 0, &tasks);

        let header = "**Task 1 result:**\n";
        let start = md.find(header).expect("task result block present") + header.len();
        let end = md[start..]
            .find("\n")
            .map(|n| start + n)
            .unwrap_or(md.len());
        let entry = &md[start..end];

        assert!(
            entry.ends_with("(truncated)"),
            "entry must end with (truncated): {:?}",
            &entry[entry.len().saturating_sub(40)..]
        );
        // Body cap is 500 bytes pre-suffix; allow the literal " (truncated)" tail.
        assert!(
            entry.len() <= 500 + " (truncated)".len() + 1,
            "entry must remain bounded; got {} bytes",
            entry.len()
        );
    }

    #[test]
    fn find_all_eligible_linear_deps_returns_one() {
        let plan = Plan {
            id: "p".to_string(),
            title: "Linear".to_string(),
            tasks: vec![
                make_task_with_status(1, "T1", PlanTaskStatus::Completed, vec![]),
                make_task_with_status(2, "T2", PlanTaskStatus::Pending, vec![1]),
                make_task_with_status(3, "T3", PlanTaskStatus::Pending, vec![2]),
            ],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let eligible = find_all_eligible(&plan);
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].number, 2);
    }

    #[test]
    fn find_all_eligible_diamond_returns_two() {
        let plan = Plan {
            id: "p".to_string(),
            title: "Diamond".to_string(),
            tasks: vec![
                make_task_with_status(1, "T1", PlanTaskStatus::Completed, vec![]),
                make_task_with_status(2, "T2", PlanTaskStatus::Pending, vec![1]),
                make_task_with_status(3, "T3", PlanTaskStatus::Pending, vec![1]),
                make_task_with_status(4, "T4", PlanTaskStatus::Pending, vec![2, 3]),
            ],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let eligible = find_all_eligible(&plan);
        assert_eq!(eligible.len(), 2);
        let numbers: Vec<u32> = eligible.iter().map(|t| t.number).collect();
        assert!(numbers.contains(&2));
        assert!(numbers.contains(&3));
    }

    #[test]
    fn find_all_eligible_respects_running() {
        let plan = Plan {
            id: "p".to_string(),
            title: "Mixed".to_string(),
            tasks: vec![
                make_task_with_status(1, "T1", PlanTaskStatus::Completed, vec![]),
                make_task_with_status(2, "T2", PlanTaskStatus::Running, vec![1]),
                make_task_with_status(3, "T3", PlanTaskStatus::Pending, vec![1]),
            ],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let eligible = find_all_eligible(&plan);
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].number, 3);
    }

    fn make_task_with_status(
        number: u32,
        title: &str,
        status: PlanTaskStatus,
        depends_on: Vec<u32>,
    ) -> PlanTask {
        PlanTask {
            number,
            title: title.to_string(),
            description: String::new(),
            depends_on,
            status,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        }
    }
}
