use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::ContentBlockType;
use crate::domain::models::conversation::{ChatMessage, generate_message_id};
use crate::domain::models::plan::{
    Plan, PlanDeviationKind, PlanStatus, PlanTask, PlanTaskStatus, TaskResult,
};
use crate::domain::models::tab::ConversationId;
use crate::domain::models::MessageRole;
use crate::domain::models::NoticeLevel;
use crate::domain::ports::EventEmitter;

const TASK_RESULT_TEXT_MAX_BYTES: usize = 4096;

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
    ) {
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

        let assistant_count = conversation.messages.iter().filter(|m| m.role == MessageRole::Assistant).count();
        let state = PlanRuntimeState {
            conversation_id: conversation_id.clone(),
            plan_id: plan_id.clone(),
            task_cancels: BTreeMap::new(),
            task_start_assistant_count: assistant_count,
            pause_pending_tasks: HashSet::new(),
            whole_plan_cancel_pending: false,
            pending_deviation: None,
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
            let plan = conversation.plans.get_mut(&plan_id).unwrap();
            let task = &mut plan.tasks[(task_number - 1) as usize];
            let prompt = format_task_prompt(task);
            task.status = PlanTaskStatus::Running;
            task.started_at_ms = Some(chrono::Utc::now().timestamp_millis());

            {
                let mut plans = self.plans.write().unwrap();
                if let Some(state) = plans.get_mut(&plan_id) {
                    state.task_cancels.insert(task_number, CancellationToken::new());
                }
            }

            // G1-P2: emit Running status BEFORE AgentThenSubmit (Critical Invariant 7)
            event_emitter.emit(AppEvent::PlanTaskStatusChanged {
                conversation_id: conversation_id.clone(),
                plan_id: plan_id.clone(),
                task_number,
                status: PlanTaskStatus::Running,
            });
            event_emitter.emit(AppEvent::AgentThenSubmit {
                conversation_id: conversation_id.clone(),
                text: prompt,
                synthetic: true,
            });
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
            let running_num = plan.tasks.iter().find(|t| t.status == PlanTaskStatus::Running).map(|t| t.number);
            if running_num != Some(task_number) {
                tracing::warn!(
                    "PlanRuntime::on_turn_complete: task_number {} does not match running task {:?}; ignoring",
                    task_number, running_num
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

        match outcome {
            TaskTurnOutcome::Success { result_text, tool_call_count, token_count } => {
                let truncated_text = truncate_result_text(&result_text);
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
                        text: truncated_text,
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

                // Check for whole-plan-cancel race (task completed before token cancel)
                let is_whole_cancel = {
                    let plans = self.plans.read().unwrap();
                    plans.get(plan_id).map(|s| s.whole_plan_cancel_pending).unwrap_or(false)
                };
                if is_whole_cancel {
                    if let Some(plan) = conversation.plans.get_mut(plan_id) {
                        for task in &mut plan.tasks {
                            if task.number == task_number { continue; }
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

                self.advance_after_task(conversation_id, plan_id, conversation, event_emitter);
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

                // Check for whole-plan-cancel race (task failed before token cancel)
                let is_whole_cancel = {
                    let plans = self.plans.read().unwrap();
                    plans.get(plan_id).map(|s| s.whole_plan_cancel_pending).unwrap_or(false)
                };
                if is_whole_cancel {
                    if let Some(plan) = conversation.plans.get_mut(plan_id) {
                        for task in &mut plan.tasks {
                            if task.number == task_number { continue; }
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
                    let skipped: Vec<_> = plan.tasks.iter()
                        .filter(|t| t.status == PlanTaskStatus::Skipped
                            && t.error.as_ref().map_or(false, |e| e.contains("depends on failed task")))
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
                    self.mark_deviation_pending(plan_id, deviation_kind.clone()).await;
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
                    self.advance_after_task(conversation_id, plan_id, conversation, event_emitter);
                }
            }
            TaskTurnOutcome::Cancelled { reason } => {
                let (is_pause_pending, is_whole_cancel) = {
                    let plans = self.plans.read().unwrap();
                    plans.get(plan_id).map(|s| {
                        (s.pause_pending_tasks.contains(&task_number), s.whole_plan_cancel_pending)
                    }).unwrap_or((false, false))
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
                    self.plans.write().unwrap()
                        .get_mut(plan_id)
                        .map(|s| { s.pause_pending_tasks.remove(&task_number); });
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

    fn advance_after_task(
        &self,
        conversation_id: &ConversationId,
        plan_id: &str,
        conversation: &mut crate::domain::models::Conversation,
        event_emitter: &dyn EventEmitter,
    ) {
        let next_number = {
            let plan = conversation.plans.get(plan_id).unwrap();
            find_next_eligible(plan).map(|t| t.number)
        };

        if let Some(task_number) = next_number {
            let plan = conversation.plans.get_mut(plan_id).unwrap();
            let task = &mut plan.tasks[(task_number - 1) as usize];
            let prompt = format_task_prompt(task);
            task.status = PlanTaskStatus::Running;
            task.started_at_ms = Some(chrono::Utc::now().timestamp_millis());

            {
                let mut plans = self.plans.write().unwrap();
                if let Some(state) = plans.get_mut(plan_id) {
                    state.task_cancels.insert(task_number, CancellationToken::new());
                }
            }

            // G1-P2: emit Running status BEFORE AgentThenSubmit (Critical Invariant 7)
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
        } else {
            let all_terminal = {
                let plan = conversation.plans.get(plan_id).unwrap();
                plan.tasks.iter().all(|t| is_terminal(t.status))
            };
            if all_terminal {
                finish_plan(conversation_id, plan_id, conversation, event_emitter);
                // G6-P19: clean up runtime state after plan walk completes
                self.plans.write().unwrap().remove(plan_id);
            } else {
                // G1-P1: AC3 step 5 — no eligible task but non-terminal tasks remain
                // (all remaining tasks are blocked by upstream failures/cancellations)
                let blocker_list = {
                    let plan = conversation.plans.get(plan_id).unwrap();
                    plan.tasks.iter()
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
                // G6-P19: clean up runtime state
                self.plans.write().unwrap().remove(plan_id);
            }
        }
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
        })
    }

    pub async fn mark_pause_pending(
        &self,
        plan_id: &str,
        task_number: u32,
    ) {
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

    pub async fn mark_deviation_pending(
        &self,
        plan_id: &str,
        kind: PlanDeviationKind,
    ) {
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
        plans.get(plan_id).map_or(false, |s| s.pending_deviation.is_some())
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
    ) {
        if self.has_pending_deviation(plan_id).await {
            tracing::debug!("PlanRuntime::resume_advance: short-circuited — pending_deviation is set");
            return;
        }
        self.advance_after_task(conversation_id, plan_id, conversation, event_emitter);
    }

    /// Returns true if the running task for this plan has received a new assistant
    /// message since it started — i.e. the turn is ready to be classified.
    pub fn can_complete_turn(&self, plan_id: &str, current_assistant_count: usize) -> bool {
        let plans = self.plans.read().unwrap();
        plans.get(plan_id).map_or(false, |s| {
            current_assistant_count > s.task_start_assistant_count
        })
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
            "failed", "error", "cannot", "unable to", "could not", "not possible",
        ];
        let content_lower = last_assistant_msg_content.to_lowercase();
        let looks_failed = !any_tool_succeeded
            && failure_keywords
                .iter()
                .any(|kw| content_lower.contains(kw));

        if looks_failed {
            TaskTurnOutcome::Failure {
                error: extract_first_sentence(last_assistant_msg_content),
            }
        } else {
            // G6-P21: do NOT truncate here — truncation happens at storage time in on_turn_complete
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
    let (plan_title, tasks_snapshot, completed, failed, skipped, total, total_elapsed_ms, total_tokens) = {
        let plan = conversation.plans.get_mut(plan_id).unwrap();
        let total = plan.tasks.len() as u32;
        let completed = plan.tasks.iter().filter(|t| t.status == PlanTaskStatus::Completed).count() as u32;
        let failed = plan.tasks.iter().filter(|t| t.status == PlanTaskStatus::Failed).count() as u32;
        let skipped = plan.tasks.iter().filter(|t| t.status == PlanTaskStatus::Skipped).count() as u32;
        let total_elapsed_ms: i64 = plan.tasks.iter().filter_map(|t| t.elapsed_ms()).sum();
        let total_tokens: u32 = plan.tasks.iter().filter_map(|t| t.result.as_ref().and_then(|r| r.token_count)).sum();

        plan.status = PlanStatus::Completed;

        let snapshot = plan.tasks.iter().map(|t| {
            (t.number, t.title.clone(), t.status, t.elapsed_ms(), t.error.clone(), t.result.clone())
        }).collect::<Vec<_>>();

        (plan.title.clone(), snapshot, completed, failed, skipped, total, total_elapsed_ms, total_tokens)
    };

    let summary = build_summary_markdown(
        &plan_title, completed, total, failed, skipped, total_elapsed_ms, total_tokens, &tasks_snapshot,
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
    tasks: &[(u32, String, PlanTaskStatus, Option<i64>, Option<String>, Option<TaskResult>)],
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
    summary.push_str(&format!(" · ~{} elapsed", format_elapsed_ms(total_elapsed_ms)));
    if total_tokens > 0 {
        summary.push_str(&format!(" · {} tokens", total_tokens));
    }
    md.push_str(&format!("**Summary:** {}\n\n", summary));

    md.push_str("| # | Task | Status | Elapsed |\n");
    md.push_str("|---|------|--------|---------|\n");
    for (number, title, status, elapsed, error, _result) in tasks {
        let status_str = match status {
            PlanTaskStatus::Completed => "✓".to_string(),
            PlanTaskStatus::Failed => format!(
                "✗ {}",
                error.as_deref().unwrap_or("error").lines().next().unwrap_or("")
            ),
            PlanTaskStatus::Skipped => format!(
                "⏭ {}",
                error.as_deref().unwrap_or("skipped").lines().next().unwrap_or("")
            ),
            _ => format!("{:?}", status),
        };
        let elapsed_str = match *elapsed {
            Some(ms) => format_elapsed_ms(ms),
            None => "—".to_string(),
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            number, title, status_str, elapsed_str
        ));
    }

    for (number, _title, status, _elapsed, _error, result) in tasks {
        if *status == PlanTaskStatus::Completed {
            if let Some(r) = result {
                if !r.text.is_empty() {
                    // G2-P5: use char boundary to avoid panic on multibyte chars
                    let display = if r.text.len() > 500 {
                        let boundary = r.text.char_indices()
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

fn format_task_prompt(task: &PlanTask) -> String {
    if task.description.is_empty() {
        format!("Now executing task {}: {}.", task.number, task.title)
    } else {
        format!(
            "Now executing task {}: {}.\n\n{}",
            task.number, task.title, task.description
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
    let i = plan.tasks.iter()
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
            if plan.tasks.iter().position(|pt| pt.number == t.number).unwrap_or(usize::MAX) <= j {
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

fn truncate_result_text(text: &str) -> String {
    if text.len() <= TASK_RESULT_TEXT_MAX_BYTES {
        return text.to_string();
    }
    // G2-P5: find valid UTF-8 char boundary at or before the byte limit
    let boundary = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i < TASK_RESULT_TEXT_MAX_BYTES)
        .last()
        .unwrap_or(0);
    format!("{} (truncated)", &text[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_outcome_success_on_positive_message() {
        let result = PlanRuntime::classify_outcome(
            "Updated 4 files. Tests pass.",
            2,
            true,
            None,
        );
        match result {
            TaskTurnOutcome::Success { .. } => {}
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn classify_outcome_failure_on_cannot() {
        let result = PlanRuntime::classify_outcome(
            "I cannot find the auth module.",
            0,
            false,
            None,
        );
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
}
