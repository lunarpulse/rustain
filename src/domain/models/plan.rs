//! Domain model for inline plan proposals (Story 6-1a) and plan execution (Story 6-2a).
//!
//! Plans live on `Conversation.plans: HashMap<id, Plan>`. Content blocks
//! (`ContentBlockType::PlanCard`) reference plans by ID via `host_message_id`.
//! This indirection keeps the content block enum unit-variant (backward-compatible
//! with pre-6-1a sessions) while allowing rich plan data to be stored separately.
//!
//! **Key invariants:**
//! - At most one pending plan card per conversation (enforced by `TuiState::pending_plan_card`)
//! - `propose_plan` is risk-Safe — never routes through `ApprovalRuntime`
//! - Plans are serialized with `#[serde(default)]` for backward compatibility
//!
//! **Task lifecycle (Story 6-2a):**
//! - `started_at_ms` set by `PlanRuntime` on dispatch (Pending → Running)
//! - `result` set on Success terminal
//! - `error` set on Failure / Skipped / Cancelled
//! - `completed_at_ms` set on any terminal transition
//! - `waiting_on` populated defensively (sequential walk should not produce `Waiting`)

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Stable identifier (nanoid::nanoid!()) for cross-message reference.
    pub id: String,
    pub title: String,
    pub tasks: Vec<PlanTask>,
    /// Optional model-supplied estimate (rendered verbatim in card footer).
    /// Falls back to `derive_effort_estimate(&tasks)` when absent (see AC3 helper).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_effort: Option<EffortEstimate>,
    pub status: PlanStatus,
    /// Unix seconds — when `propose_plan` was invoked.
    pub created_at: i64,
    /// Unix seconds — when the user resolved the card (None while pending).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    /// Conversation message ID that hosts the inline PlanCard content block.
    /// Used by chat-pane re-render and export to associate the block with its plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTask {
    /// 1-indexed display position. Reorders during edit MUST renumber so display is stable.
    pub number: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 1-indexed numbers of tasks this task depends on (consumed by 6.2a — sequential
    /// execution still respects this even though we deliver "execute in order"). Must be
    /// strictly less than `self.number` (validated at AC2 / edit-save time).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<u32>,
    /// Status defaults to Pending until 6.2a flips it.
    #[serde(default)]
    pub status: PlanTaskStatus,
    /// Wall-clock ms when this task transitioned to Running. None until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    /// Wall-clock ms when this task transitioned to a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    /// Captured result on Success. None for non-Success terminal states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    /// Captured error on Failure or rationale on Skipped/Cancelled. None on Success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For Waiting (defensive — should not arise in sequential walk per AC3 note).
    /// Stored so 6.3's panel can render dep tags consistently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<u32>,
    /// Story 10.5: When `Some`, the task is currently delegated (or was delegated
    /// then completed/failed). `None` means local sequential execution per 6-2a.
    /// Field is additive; pre-10.5 sessions deserialize with `None` per
    /// `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated_to: Option<DelegationInfo>,
    /// Story 10.6: Sub-tasks decomposed from this parent task.
    /// Field is additive; pre-10.6 sessions deserialize with `[]` per
    /// `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_tasks: Vec<PlanSubTask>,
}

impl PlanTask {
    /// Wall-clock elapsed ms (Running → terminal). `None` if not started.
    /// While in Running, returns the live elapsed-since-start.
    pub fn elapsed_ms(&self) -> Option<i64> {
        self.started_at_ms.map(|start| {
            let end = self.completed_at_ms.unwrap_or_else(now_unix_ms);
            (end - start).max(0)
        })
    }

    /// Story 10.6: `(completed_sub_tasks, total_sub_tasks)`.
    pub fn sub_task_progress(&self) -> (usize, usize) {
        let total = self.sub_tasks.len();
        let completed = self
            .sub_tasks
            .iter()
            .filter(|st| {
                matches!(
                    st.status,
                    PlanTaskStatus::Completed | PlanTaskStatus::Skipped
                )
            })
            .count();
        (completed, total)
    }

    /// Story 10.6: true when this task carries at least one sub-task.
    pub fn has_sub_tasks(&self) -> bool {
        !self.sub_tasks.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationInfo {
    /// Agent definition name (e.g., "code-reviewer"). Matches `AgentDef.name`.
    pub agent_name: String,
    /// AgentId of the spawned subagent (matches `SubagentRegistry`).
    /// `None` while the spawn is pending; set as soon as `launch()` returns Ok.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Wall-clock ms when the delegation was approved (or auto-accepted via YOLO).
    pub delegated_at_ms: i64,
    /// Spool task_id for `read_task_output` retrieval (matches Story 10-0 spool).
    /// Populated when `TaskHandle.task_id` is captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spool_task_id: Option<String>,
}

/// Story 10.6: A single-level sub-task decomposed from a parent `PlanTask`.
/// Deliberately does NOT contain `sub_tasks` — decomposition is capped at one
/// level in v0 (structurally bounded, well inside NFR15 depth-3 ceiling).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSubTask {
    /// Intra-parent 1-indexed display position.
    pub number: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub status: PlanTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    /// Captured result on Success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    /// Captured error on Failure or rationale on Skipped/Cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When `Some`, the sub-task is currently delegated (or was delegated then completed/failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated_to: Option<DelegationInfo>,
}

impl PlanSubTask {
    /// Wall-clock elapsed ms (Running → terminal). `None` if not started.
    pub fn elapsed_ms(&self) -> Option<i64> {
        self.started_at_ms.map(|start| {
            let end = self.completed_at_ms.unwrap_or_else(now_unix_ms);
            (end - start).max(0)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    /// Plain-text concatenation of the assistant message that closed the task's turn.
    /// Stored verbatim — no length cap (Story 6.3-FU3 reversed the legacy 4 KiB
    /// storage truncation; downstream consumers cap at render time).
    pub text: String,
    pub tool_call_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u32>,
}

fn now_unix_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffortEstimate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PlanStatus {
    #[default]
    Pending,
    Executing,
    Completed,
    Rejected,
    Editing,
    /// Plan execution was interrupted by a Cancelled task (AC2 hard-stop branch).
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PlanTaskStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
    /// Dep gate — defensive; sequential walk should not produce this.
    Waiting,
    /// Story 6.4: user-initiated pause. Distinct from `Cancelled` (which is a hard stop).
    /// A `Paused` task can transition back to `Pending` via the resume flow (AC1).
    /// Downstream-dependent tasks are transitively paused.
    Paused,
    /// 6.4 will flip via per-task cancel; this story produces it on whole-turn cancel.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanDecision {
    Approve,
    AutoApproveYolo,
    Edit,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PlanDeviationKind {
    /// Runtime auto-skipped downstream tasks blocked by upstream failure/cancellation.
    AutoSkipBlockedTasks { source_task: u32 },
    /// Agent proposed a plan revision via the propose_plan_revision tool.
    /// Variant reserved; tool implementation deferred to 6.4-FU1.
    AgentRevision { proposed_tasks: Vec<PlanTask> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_serde_round_trip_all_fields() {
        let plan = Plan {
            id: "test-id".to_string(),
            title: "Test Plan".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "Step 1".to_string(),
                description: "Do thing".to_string(),
                depends_on: vec![],
                status: PlanTaskStatus::Pending,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
                sub_tasks: vec![],
            }],
            estimated_effort: Some(EffortEstimate {
                tool_calls: Some(5),
                seconds: Some(40),
            }),
            status: PlanStatus::Pending,
            created_at: 1700000000,
            resolved_at: Some(1700000060),
            host_message_id: Some("msg-1".to_string()),
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn plan_serde_round_trip_minimal() {
        let plan = Plan {
            id: "test-id".to_string(),
            title: "Minimal".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "Step".to_string(),
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
            }],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1700000000,
            resolved_at: None,
            host_message_id: None,
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("\"estimatedEffort\""), "{}", json);
        assert!(!json.contains("\"resolvedAt\""), "{}", json);
        assert!(!json.contains("\"hostMessageId\""), "{}", json);
        assert!(!json.contains("\"description\""), "{}", json);
        assert!(!json.contains("\"dependsOn\""), "{}", json);
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn plan_status_default_is_pending() {
        assert_eq!(PlanStatus::default(), PlanStatus::Pending);
    }

    #[test]
    fn plan_task_status_default_is_pending() {
        assert_eq!(PlanTaskStatus::default(), PlanTaskStatus::Pending);
    }

    #[test]
    fn plan_json_is_camel_case() {
        let plan = Plan {
            id: "id".to_string(),
            title: "t".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "s".to_string(),
                description: "d".to_string(),
                depends_on: vec![2],
                status: PlanTaskStatus::Running,
                started_at_ms: Some(1000),
                completed_at_ms: Some(2000),
                result: Some(TaskResult {
                    text: "done".to_string(),
                    tool_call_count: 3,
                    token_count: Some(100),
                }),
                error: None,
                waiting_on: vec![],
                delegated_to: None,
                sub_tasks: vec![],
            }],
            estimated_effort: Some(EffortEstimate {
                tool_calls: Some(1),
                seconds: Some(8),
            }),
            status: PlanStatus::Executing,
            created_at: 0,
            resolved_at: Some(1),
            host_message_id: Some("m".to_string()),
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"estimatedEffort\""), "{}", json);
        assert!(json.contains("\"toolCalls\""), "{}", json);
        assert!(json.contains("\"dependsOn\""), "{}", json);
        assert!(json.contains("\"hostMessageId\""), "{}", json);
        assert!(json.contains("\"resolvedAt\""), "{}", json);
        assert!(json.contains("\"startedAtMs\""), "{}", json);
        assert!(json.contains("\"completedAtMs\""), "{}", json);
        assert!(json.contains("\"toolCallCount\""), "{}", json);
        assert!(json.contains("\"tokenCount\""), "{}", json);
    }

    #[test]
    fn pre_6_2a_json_deserializes() {
        let json = r#"{
            "id": "old-id",
            "title": "Old Plan",
            "tasks": [{
                "number": 1,
                "title": "Step",
                "description": "",
                "dependsOn": [],
                "status": "completed"
            }],
            "status": "completed",
            "createdAt": 1700000000
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.tasks[0].started_at_ms, None);
        assert_eq!(plan.tasks[0].completed_at_ms, None);
        assert_eq!(plan.tasks[0].result, None);
        assert_eq!(plan.tasks[0].error, None);
        assert!(plan.tasks[0].waiting_on.is_empty());
        assert_eq!(plan.tasks[0].delegated_to, None);
        assert_eq!(plan.tasks[0].status, PlanTaskStatus::Completed);
    }

    #[test]
    fn delegation_info_round_trip() {
        let task = PlanTask {
            number: 1,
            title: "Step".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: Some(DelegationInfo {
                agent_name: "code-reviewer".to_string(),
                agent_id: Some("agent-123".to_string()),
                delegated_at_ms: 1700000000000,
                spool_task_id: Some("spool-456".to_string()),
            }),
            sub_tasks: vec![],
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"delegatedTo\""));
        assert!(json.contains("\"agentName\":\"code-reviewer\""));
        assert!(json.contains("\"agentId\":\"agent-123\""));
        assert!(json.contains("\"delegatedAtMs\":1700000000000"));
        assert!(json.contains("\"spoolTaskId\":\"spool-456\""));
        let back: PlanTask = serde_json::from_str(&json).unwrap();
        assert_eq!(back, task);
    }

    #[test]
    fn new_task_status_variants_serialize() {
        assert_eq!(
            serde_json::to_string(&PlanTaskStatus::Waiting).unwrap(),
            "\"waiting\""
        );
        assert_eq!(
            serde_json::to_string(&PlanTaskStatus::Cancelled).unwrap(),
            "\"cancelled\""
        );
    }

    #[test]
    fn new_plan_status_cancelled_serializes() {
        assert_eq!(
            serde_json::to_string(&PlanStatus::Cancelled).unwrap(),
            "\"cancelled\""
        );
    }

    #[test]
    fn task_result_round_trip() {
        let tr = TaskResult {
            text: "Completed successfully.".to_string(),
            tool_call_count: 3,
            token_count: Some(150),
        };
        let json = serde_json::to_string(&tr).unwrap();
        let back: TaskResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tr);
        assert!(json.contains("\"toolCallCount\""));
        assert!(json.contains("\"tokenCount\""));
    }

    #[test]
    fn task_result_minimal_skips_token_count() {
        let tr = TaskResult {
            text: "done".to_string(),
            tool_call_count: 0,
            token_count: None,
        };
        let json = serde_json::to_string(&tr).unwrap();
        assert!(!json.contains("\"tokenCount\""));
    }

    #[test]
    fn elapsed_ms_returns_none_when_not_started() {
        let task = PlanTask {
            number: 1,
            title: "t".to_string(),
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
        assert_eq!(task.elapsed_ms(), None);
    }

    #[test]
    fn elapsed_ms_returns_delta_when_running() {
        let task = PlanTask {
            number: 1,
            title: "t".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Running,
            started_at_ms: Some(1000),
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        };
        assert!(task.elapsed_ms().unwrap() >= 0);
    }

    #[test]
    fn elapsed_ms_returns_fixed_delta_when_terminal() {
        let task = PlanTask {
            number: 1,
            title: "t".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Completed,
            started_at_ms: Some(1000),
            completed_at_ms: Some(5000),
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        };
        assert_eq!(task.elapsed_ms(), Some(4000));
    }

    #[test]
    fn paused_serializes_correctly() {
        assert_eq!(
            serde_json::to_string(&PlanTaskStatus::Paused).unwrap(),
            "\"paused\""
        );
        let back: PlanTaskStatus = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(back, PlanTaskStatus::Paused);
    }

    #[test]
    fn plan_deviation_kind_auto_skip_round_trip() {
        let kind = PlanDeviationKind::AutoSkipBlockedTasks { source_task: 2 };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"kind\":\"autoSkipBlockedTasks\""));
        let back: PlanDeviationKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn plan_deviation_kind_agent_revision_round_trip() {
        let kind = PlanDeviationKind::AgentRevision {
            proposed_tasks: vec![PlanTask {
                number: 1,
                title: "t".to_string(),
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
            }],
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"kind\":\"agentRevision\""));
        let back: PlanDeviationKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    // ─── Story 10.6: PlanSubTask tests ───

    #[test]
    fn plan_subtask_serde_round_trip() {
        let plan = Plan {
            id: "id".to_string(),
            title: "t".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "parent".to_string(),
                description: String::new(),
                depends_on: vec![],
                status: PlanTaskStatus::Pending,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
                sub_tasks: vec![
                    PlanSubTask {
                        number: 1,
                        title: "sub-a".to_string(),
                        description: "desc".to_string(),
                        status: PlanTaskStatus::Completed,
                        started_at_ms: Some(1000),
                        completed_at_ms: Some(2000),
                        result: Some(TaskResult {
                            text: "done".to_string(),
                            tool_call_count: 1,
                            token_count: Some(50),
                        }),
                        error: None,
                        delegated_to: None,
                    },
                    PlanSubTask {
                        number: 2,
                        title: "sub-b".to_string(),
                        description: String::new(),
                        status: PlanTaskStatus::Failed,
                        started_at_ms: Some(3000),
                        completed_at_ms: Some(4000),
                        result: None,
                        error: Some("oops".to_string()),
                        delegated_to: None,
                    },
                ],
            }],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"subTasks\""), "{}", json);
        assert!(json.contains("\"sub-a\""), "{}", json);
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn pre_10_6_json_deserializes_with_empty_sub_tasks() {
        let json = r#"{
            "id": "old-id",
            "title": "Old Plan",
            "tasks": [{
                "number": 1,
                "title": "Step",
                "description": "",
                "dependsOn": [],
                "status": "completed"
            }],
            "status": "completed",
            "createdAt": 1700000000
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert!(plan.tasks[0].sub_tasks.is_empty());
    }

    #[test]
    fn sub_task_progress_counts_correctly() {
        let task = PlanTask {
            number: 1,
            title: "t".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Running,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![
                PlanSubTask {
                    number: 1,
                    title: "a".to_string(),
                    description: String::new(),
                    status: PlanTaskStatus::Completed,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    delegated_to: None,
                },
                PlanSubTask {
                    number: 2,
                    title: "b".to_string(),
                    description: String::new(),
                    status: PlanTaskStatus::Failed,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    delegated_to: None,
                },
                PlanSubTask {
                    number: 3,
                    title: "c".to_string(),
                    description: String::new(),
                    status: PlanTaskStatus::Skipped,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    delegated_to: None,
                },
            ],
        };
        assert_eq!(task.sub_task_progress(), (2, 3));
        assert!(task.has_sub_tasks());
    }

    #[test]
    fn no_sub_tasks_progress_is_zero() {
        let task = PlanTask {
            number: 1,
            title: "t".to_string(),
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
        assert_eq!(task.sub_task_progress(), (0, 0));
        assert!(!task.has_sub_tasks());
    }
}
