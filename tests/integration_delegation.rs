//! Integration tests for Story 10.5: Task Delegation to Subagents.
//!
//! Verifies DelegationDecider + LaunchSpecBuilder integration without
//! spawning real subagents.

use rustain::domain::models::{
    AgentDef, PermissionMode, Plan, PlanStatus, PlanTask, PlanTaskStatus,
};
use rustain::domain::services::delegation_decider::DelegationDecider;
use rustain::domain::services::launch_spec_builder::LaunchSpecBuilder;

fn make_task(number: u32, title: &str, description: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: description.to_string(),
        depends_on: vec![],
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
    }
}

fn make_plan(tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: "plan-1".to_string(),
        title: "Test Plan".to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1000,
        resolved_at: Some(2000),
        host_message_id: None,
    }
}

#[test]
fn integration_delegation_decider_suggests_code_reviewer() {
    let plan = make_plan(vec![make_task(
        1,
        "Review code",
        "Please review the authentication module for security issues",
    )]);

    let agents = vec![
        AgentDef {
            name: "code-reviewer".to_string(),
            description: "Security-focused code reviewer".to_string(),
            file: std::path::PathBuf::from("/dev/null"),
            model: None,
            allowed_tools: None,
            exclude_tools: None,
        },
        AgentDef {
            name: "doc-writer".to_string(),
            description: "Writes documentation".to_string(),
            file: std::path::PathBuf::from("/dev/null"),
            model: None,
            allowed_tools: None,
            exclude_tools: None,
        },
    ];

    let suggestion =
        DelegationDecider::suggest(&plan, &plan.tasks[0], &agents, PermissionMode::Normal);

    assert!(
        suggestion.is_some(),
        "should suggest an agent for code review task"
    );
    let s = suggestion.unwrap();
    assert_eq!(s.agent_name, "code-reviewer");
    assert_eq!(s.task_number, 1);
}

#[test]
fn integration_launch_spec_builder_uses_agent_model() {
    let task = make_task(1, "Build", "Compile the project");
    let agent = AgentDef {
        name: "builder".to_string(),
        description: "Build agent".to_string(),
        file: std::path::PathBuf::from("/dev/null"),
        model: Some("gpt-4".to_string()),
        allowed_tools: Some(vec!["bash".to_string(), "read".to_string()]),
        exclude_tools: None,
    };

    let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "default-model", 0, None);

    assert_eq!(spec.effective_model, "gpt-4");
    assert!(spec.prompt.contains("Build"));
    assert!(spec.prompt.contains("Compile the project"));
}

#[test]
fn integration_launch_spec_builder_fallback_model() {
    let task = make_task(1, "Test", "Run unit tests");
    let agent = AgentDef {
        name: "tester".to_string(),
        description: "Test agent".to_string(),
        file: std::path::PathBuf::from("/dev/null"),
        model: None,
        allowed_tools: None,
        exclude_tools: None,
    };

    let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "fallback-model", 0, None);

    assert_eq!(spec.effective_model, "fallback-model");
}

#[test]
fn integration_find_all_eligible_returns_correct_set() {
    let plan = make_plan(vec![
        PlanTask {
            number: 1,
            title: "A".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Completed,
            started_at_ms: Some(1000),
            completed_at_ms: Some(2000),
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
        },
        PlanTask {
            number: 2,
            title: "B".to_string(),
            description: String::new(),
            depends_on: vec![1],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
        },
        PlanTask {
            number: 3,
            title: "C".to_string(),
            description: String::new(),
            depends_on: vec![1],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
        },
        PlanTask {
            number: 4,
            title: "D".to_string(),
            description: String::new(),
            depends_on: vec![2, 3],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
        },
    ]);

    let eligible = rustain::domain::services::plan_runtime::find_all_eligible(&plan);
    assert_eq!(eligible.len(), 2, "tasks 2 and 3 should be eligible");
    assert_eq!(eligible[0].number, 2);
    assert_eq!(eligible[1].number, 3);
}
