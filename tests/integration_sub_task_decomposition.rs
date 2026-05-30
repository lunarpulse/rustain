//! Integration tests: Story 10.6 — Sub-Task Decomposition & Result Aggregation.
//!
//! Verifies result aggregation and failure policies for decomposed parent tasks.

use rustain::domain::models::{DelegationInfo, PlanSubTask, PlanTask, PlanTaskStatus, TaskResult};
use rustain::domain::services::plan_runtime::PlanRuntime;

fn make_task(number: u32, title: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
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
    }
}

fn make_sub_task(number: u32, title: &str, status: PlanTaskStatus) -> PlanSubTask {
    PlanSubTask {
        number,
        title: title.to_string(),
        description: String::new(),
        status,
        started_at_ms: if status == PlanTaskStatus::Pending {
            None
        } else {
            Some(1000)
        },
        completed_at_ms: if status == PlanTaskStatus::Running || status == PlanTaskStatus::Pending {
            None
        } else {
            Some(5000)
        },
        result: if status == PlanTaskStatus::Completed {
            Some(TaskResult {
                text: format!("Result for {}", title),
                tool_call_count: 1,
                token_count: Some(50),
            })
        } else {
            None
        },
        error: if status == PlanTaskStatus::Failed {
            Some(format!("Error in {}", title))
        } else {
            None
        },
        delegated_to: None,
    }
}

/// T8(a): parent with 3 local sub-tasks → aggregated result.
#[test]
fn integration_sub_task_aggregation_three_locals() {
    let mut task = make_task(1, "Parent");
    task.sub_tasks = vec![
        make_sub_task(1, "Sub A", PlanTaskStatus::Completed),
        make_sub_task(2, "Sub B", PlanTaskStatus::Completed),
        make_sub_task(3, "Sub C", PlanTaskStatus::Completed),
    ];

    let result = PlanRuntime::aggregate_sub_task_results(&task);

    assert!(result.text.contains("Sub A"));
    assert!(result.text.contains("Sub B"));
    assert!(result.text.contains("Sub C"));
    assert_eq!(result.tool_call_count, 3);
    assert_eq!(result.token_count, Some(150));
}

/// T8(b): mixed local + delegated sub-tasks → aggregation includes pointer strings.
#[test]
fn integration_sub_task_aggregation_mixed_with_delegated() {
    let mut task = make_task(1, "Parent");
    let mut sub_a = make_sub_task(1, "Local Sub", PlanTaskStatus::Completed);
    sub_a.delegated_to = Some(DelegationInfo {
        agent_id: Some("agent-1".to_string()),
        agent_name: "local-agent".to_string(),
        spool_task_id: Some("spool-123".to_string()),
        delegated_at_ms: 1000,
    });

    let mut sub_b = make_sub_task(2, "Delegated Sub", PlanTaskStatus::Completed);
    sub_b.delegated_to = Some(DelegationInfo {
        agent_id: Some("agent-2".to_string()),
        agent_name: "remote-agent".to_string(),
        spool_task_id: Some("spool-456".to_string()),
        delegated_at_ms: 1000,
    });

    task.sub_tasks = vec![sub_a, sub_b];

    let result = PlanRuntime::aggregate_sub_task_results(&task);

    assert!(result.text.contains("Local Sub"));
    assert!(result.text.contains("Delegated Sub"));
    assert!(result.text.contains("read_task_output(\"spool-456\")"));
    assert_eq!(result.tool_call_count, 2);
}

/// T8(c): fail_fast — failed sub-task causes parent to reflect failure in aggregation.
#[test]
fn integration_fail_fast_parent_reflects_failure() {
    let mut task = make_task(1, "Parent");
    task.sub_tasks = vec![
        make_sub_task(1, "Sub A", PlanTaskStatus::Completed),
        make_sub_task(2, "Sub B", PlanTaskStatus::Failed),
        make_sub_task(3, "Sub C", PlanTaskStatus::Pending),
    ];

    let result = PlanRuntime::aggregate_sub_task_results(&task);

    assert!(result.text.contains("Sub A"));
    assert!(result.text.contains("Sub B"));
    assert_eq!(result.tool_call_count, 1);
}

/// T8(d): best_effort — partial completion includes all sub-task outcomes.
#[test]
fn integration_best_effort_partial_completion() {
    let mut task = make_task(1, "Parent");
    task.sub_tasks = vec![
        make_sub_task(1, "Sub A", PlanTaskStatus::Completed),
        make_sub_task(2, "Sub B", PlanTaskStatus::Failed),
        make_sub_task(3, "Sub C", PlanTaskStatus::Skipped),
    ];

    let result = PlanRuntime::aggregate_sub_task_results(&task);

    // Should list all sub-tasks with their status icons
    assert!(result.text.contains("✅"));
    assert!(result.text.contains("❌"));
    assert!(result.text.contains("⏭️"));
    assert!(result.text.contains("Sub A"));
    assert!(result.text.contains("Sub B"));
    assert!(result.text.contains("Sub C"));
}

/// T8(a): aggregation sums tool_call_count and token_count correctly.
#[test]
fn integration_sub_task_aggregation_sums_counts() {
    let mut task = make_task(1, "Parent");
    let mut sub_a = make_sub_task(1, "Sub A", PlanTaskStatus::Completed);
    sub_a.result = Some(TaskResult {
        text: "A".to_string(),
        tool_call_count: 5,
        token_count: Some(100),
    });
    let mut sub_b = make_sub_task(2, "Sub B", PlanTaskStatus::Completed);
    sub_b.result = Some(TaskResult {
        text: "B".to_string(),
        tool_call_count: 3,
        token_count: Some(200),
    });

    task.sub_tasks = vec![sub_a, sub_b];

    let result = PlanRuntime::aggregate_sub_task_results(&task);
    assert_eq!(result.tool_call_count, 8);
    assert_eq!(result.token_count, Some(300));
}
