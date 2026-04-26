//! Domain model for inline plan proposals (Story 6-1a).
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
    }
}
