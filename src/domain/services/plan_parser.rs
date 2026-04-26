//! Plan input parsing and validation for the `propose_plan` tool (Story 6-1a AC2).
//!
//! Validation rules:
//! - 1–20 tasks (non-empty, ≤20)
//! - No empty plan or task titles
//! - `depends_on` must reference strictly-earlier task numbers that exist
//! - Duplicate task numbers are rejected (defensive — auto-assigned at parse time)

use crate::domain::errors::ToolError;
use crate::domain::models::{
    EffortEstimate, Plan, PlanStatus, PlanTask, PlanTaskStatus,
};
use crate::domain::services::plan_effort::derive_effort_estimate;

const MAX_PLAN_TITLE_LEN: usize = 80;
const MAX_TASK_TITLE_LEN: usize = 120;
const MAX_TASK_DESCRIPTION_LEN: usize = 2000;
const MAX_TASKS: usize = 20;

fn parse_u32_field(
    input: &serde_json::Value,
    key: &str,
) -> Result<Option<u32>, ToolError> {
    input
        .get(key)
        .map(|v| {
            v.as_u64()
                .and_then(|n| if n <= u32::MAX as u64 { Some(n as u32) } else { None })
                .ok_or_else(|| {
                    ToolError::InvalidInput(format!(
                        "propose_plan: '{}' must be an integer between 0 and {}",
                        key,
                        u32::MAX
                    ))
                })
        })
        .transpose()
}

fn parse_plan_input_inner(
    input: &serde_json::Value,
    plan_id: &str,
) -> Result<Plan, ToolError> {
    let title = input
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ToolError::InvalidInput("propose_plan: missing or non-string 'title'".to_string())
        })?;

    let tasks_arr = input
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            ToolError::InvalidInput("propose_plan: missing or non-array 'tasks'".to_string())
        })?;

    if tasks_arr.is_empty() {
        return Err(ToolError::InvalidInput(
            "propose_plan: plan must have at least one task".to_string(),
        ));
    }
    if tasks_arr.len() > MAX_TASKS {
        return Err(ToolError::InvalidInput(
            "propose_plan: plan must have at most 20 tasks".to_string(),
        ));
    }

    let mut tasks = Vec::with_capacity(tasks_arr.len());
    for (i, t) in tasks_arr.iter().enumerate() {
        let task_title = t
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput(format!(
                    "propose_plan: task {}: missing or non-string 'title'",
                    i + 1
                ))
            })?
            .to_string();

        let description = t
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let depends_on = t
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .map(|(j, v)| {
                        v.as_u64()
                            .and_then(|n| {
                                if n <= u32::MAX as u64 {
                                    Some(n as u32)
                                } else {
                                    None
                                }
                            })
                            .ok_or_else(|| {
                                ToolError::InvalidInput(format!(
                                    "propose_plan: task {}: depends_on[{}] must be a positive integer ≤ {}",
                                    i + 1,
                                    j,
                                    u32::MAX
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        tasks.push(PlanTask {
            number: (i + 1) as u32,
            title: task_title,
            description,
            depends_on,
            status: PlanTaskStatus::Pending,
        });
    }

    let model_tool_calls = parse_u32_field(input, "estimated_tool_calls")?;
    let model_seconds = parse_u32_field(input, "estimated_seconds")?;

    let estimated_effort = if model_tool_calls.is_some() || model_seconds.is_some() {
        Some(EffortEstimate {
            tool_calls: model_tool_calls,
            seconds: model_seconds,
        })
    } else {
        derive_effort_estimate(&tasks)
    };

    let now = crate::domain::models::now_unix();
    let plan = Plan {
        id: plan_id.to_string(),
        title: title.to_string(),
        tasks,
        estimated_effort,
        status: PlanStatus::Pending,
        created_at: now,
        resolved_at: None,
        host_message_id: None,
    };

    validate_plan(&plan).map_err(|e| ToolError::InvalidInput(format!("propose_plan: {}", e)))?;
    Ok(plan)
}

/// Public entry point matching AC2 spec signature.
pub fn parse_plan_input(input: &serde_json::Value, plan_id: &str) -> Result<Plan, ToolError> {
    parse_plan_input_inner(input, plan_id)
}

pub fn validate_plan(plan: &Plan) -> Result<(), String> {
    if plan.title.trim().is_empty() {
        return Err("plan title cannot be empty".to_string());
    }
    if plan.title.len() > MAX_PLAN_TITLE_LEN {
        return Err(format!("plan title exceeds {} characters", MAX_PLAN_TITLE_LEN));
    }

    if plan.tasks.is_empty() {
        return Err("plan must have at least one task".to_string());
    }
    if plan.tasks.len() > MAX_TASKS {
        return Err("plan must have at most 20 tasks".to_string());
    }

    let valid_numbers: std::collections::HashSet<u32> =
        plan.tasks.iter().map(|t| t.number).collect();

    // Check for duplicate numbers (defensive — auto-assigned in parse).
    if valid_numbers.len() != plan.tasks.len() {
        return Err("duplicate task numbers detected".to_string());
    }

    for task in &plan.tasks {
        if task.title.trim().is_empty() {
            return Err(format!("task {} has an empty title", task.number));
        }
        if task.title.len() > MAX_TASK_TITLE_LEN {
            return Err(format!(
                "task {} title exceeds {} characters",
                task.number, MAX_TASK_TITLE_LEN
            ));
        }
        if task.description.len() > MAX_TASK_DESCRIPTION_LEN {
            return Err(format!(
                "task {} description exceeds {} characters",
                task.number, MAX_TASK_DESCRIPTION_LEN
            ));
        }

        for dep in &task.depends_on {
            if *dep >= task.number {
                return Err(format!(
                    "task {} depends_on {} — must reference a strictly-earlier task",
                    task.number, dep
                ));
            }
            if !valid_numbers.contains(dep) {
                return Err(format!(
                    "task {} depends_on {} — referenced task does not exist",
                    task.number, dep
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_input() -> serde_json::Value {
        serde_json::json!({
            "title": "Refactor Auth",
            "tasks": [
                { "title": "Read code" },
                { "title": "Extract middleware", "depends_on": [1] },
                { "title": "Write tests", "depends_on": [2] },
            ]
        })
    }

    #[test]
    fn happy_path_with_deps() {
        let plan = parse_plan_input(&json_input(), "plan-1").unwrap();
        assert_eq!(plan.title, "Refactor Auth");
        assert_eq!(plan.tasks.len(), 3);
        assert_eq!(plan.tasks[0].number, 1);
        assert_eq!(plan.tasks[1].depends_on, vec![1]);
        assert_eq!(plan.tasks[2].depends_on, vec![2]);
        assert_eq!(plan.status, PlanStatus::Pending);
        assert!(plan.estimated_effort.is_some());
    }

    #[test]
    fn rejects_empty_tasks() {
        let input = serde_json::json!({ "title": "t", "tasks": [] });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("at least one task"));
    }

    #[test]
    fn rejects_21_tasks() {
        let tasks: Vec<serde_json::Value> = (1..=21)
            .map(|i| serde_json::json!({ "title": format!("Task {}", i) }))
            .collect();
        let input = serde_json::json!({ "title": "t", "tasks": tasks });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("at most 20 tasks"));
    }

    #[test]
    fn rejects_forward_dep() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [
                { "title": "A", "depends_on": [3] },
                { "title": "B" },
                { "title": "C" },
            ]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("strictly-earlier"));
    }

    #[test]
    fn rejects_nonexistent_dep() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [
                { "title": "A" },
                { "title": "B", "depends_on": [0] },
            ]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn rejects_empty_task_title() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [
                { "title": "A" },
                { "title": "" },
            ]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("empty title"));
    }

    #[test]
    fn rejects_empty_plan_title() {
        let input = serde_json::json!({
            "title": "   ",
            "tasks": [{ "title": "A" }]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("plan title cannot be empty"));
    }

    #[test]
    fn rejects_long_plan_title() {
        let input = serde_json::json!({
            "title": "x".repeat(MAX_PLAN_TITLE_LEN + 1),
            "tasks": [{ "title": "A" }]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("plan title exceeds"));
    }

    #[test]
    fn rejects_long_task_title() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [{ "title": "x".repeat(MAX_TASK_TITLE_LEN + 1) }]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("task 1 title exceeds"));
    }

    #[test]
    fn rejects_long_task_description() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [{ "title": "A", "description": "x".repeat(MAX_TASK_DESCRIPTION_LEN + 1) }]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("task 1 description exceeds"));
    }

    #[test]
    fn rejects_invalid_dep_type() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [
                { "title": "A" },
                { "title": "B", "depends_on": [1, "foo"] },
            ]
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("depends_on[1] must be a positive integer"));
    }

    #[test]
    fn rejects_overflow_estimated_tool_calls() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [{ "title": "A" }],
            "estimated_tool_calls": u64::MAX
        });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("must be an integer between 0 and"));
    }

    #[test]
    fn model_provided_effort_overrides_derivation() {
        let input = serde_json::json!({
            "title": "t",
            "tasks": [{ "title": "A" }],
            "estimated_tool_calls": 10,
            "estimated_seconds": 60
        });
        let plan = parse_plan_input(&input, "p").unwrap();
        let est = plan.estimated_effort.unwrap();
        assert_eq!(est.tool_calls, Some(10));
        assert_eq!(est.seconds, Some(60));
    }

    #[test]
    fn missing_title_is_schema_error() {
        let input = serde_json::json!({ "tasks": [{ "title": "A" }] });
        let err = parse_plan_input(&input, "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("missing or non-string 'title'"));
    }

    #[test]
    fn validate_plan_returns_string_errors() {
        let plan = Plan {
            id: "p".to_string(),
            title: "   ".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "A".to_string(),
                description: String::new(),
                depends_on: vec![],
                status: PlanTaskStatus::Pending,
            }],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.contains("plan title cannot be empty"));
    }
}
