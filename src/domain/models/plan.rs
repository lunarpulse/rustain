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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    /// Plain-text concatenation of the assistant message that closed the task's turn.
    /// Truncated to 4 KiB at storage time.
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
        assert_eq!(plan.tasks[0].status, PlanTaskStatus::Completed);
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
        };
        assert_eq!(task.elapsed_ms(), Some(4000));
    }
}
