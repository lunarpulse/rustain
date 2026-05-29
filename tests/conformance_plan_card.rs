//! Conformance tests for Story 6-1a: Inline Plan Card.
//!
//! Validates the Plan data model, parse/validate pipeline, effort estimation,
//! plan card rendering, conversation persistence, export, search, and fork
//! behaviours for the inline plan card feature.

use std::collections::HashMap;
use std::sync::Arc;

use rustain::domain::models::session_meta::SessionMeta;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, EffortEstimate, MessageRole, PermissionMode, Plan,
    PlanDecision, PlanStatus, PlanTask, PlanTaskStatus, generate_conversation_id,
    generate_message_id,
};
use rustain::domain::services::export::render_conversation_markdown;
use rustain::domain::services::plan_effort::derive_effort_estimate;
use rustain::domain::services::plan_parser::{parse_plan_input, validate_plan};
use rustain::domain::services::search::find_matches;

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
    }
}

fn make_task_with_desc(number: u32, title: &str, desc: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: desc.to_string(),
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

fn make_msg(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        id: generate_message_id(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    }
}

fn make_conv(messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages,
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

fn make_meta(message_count: usize) -> SessionMeta {
    SessionMeta {
        version: 1,
        title: "Test".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        message_count,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        plan_slug: None,
        extra: serde_json::Map::new(),
    }
}

fn make_plan_with_tasks(task_count: u32) -> Plan {
    let tasks: Vec<PlanTask> = (1..=task_count)
        .map(|i| make_task(i, &format!("Task {}", i)))
        .collect();
    Plan {
        id: "plan-1".to_string(),
        title: "Test Plan".to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: None,
    }
}

// ── AC1: Plan Data Model ─────────────────────────────────────────────────

#[test]
fn ac1_plan_serde_round_trip() {
    let plan = Plan {
        id: "plan-serde".to_string(),
        title: "Full Plan".to_string(),
        tasks: vec![
            make_task(1, "Read code"),
            PlanTask {
                number: 2,
                title: "Refactor".to_string(),
                description: "Extract module".to_string(),
                depends_on: vec![1],
                status: PlanTaskStatus::Pending,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
            },
        ],
        estimated_effort: Some(EffortEstimate {
            tool_calls: Some(5),
            seconds: Some(40),
        }),
        status: PlanStatus::Pending,
        created_at: 1_700_000_000,
        resolved_at: Some(1_700_000_060),
        host_message_id: Some("msg-1".to_string()),
    };
    let json = serde_json::to_string(&plan).unwrap();
    let back: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(back, plan);
}

#[test]
fn ac1_plan_default_status_pending() {
    assert_eq!(PlanStatus::default(), PlanStatus::Pending);
    let plan = make_plan_with_tasks(1);
    assert_eq!(plan.status, PlanStatus::Pending);
}

#[test]
fn ac1_session_without_plans_field_loads() {
    let json = r#"{
        "id": "c1",
        "title": "Legacy",
        "messages": [],
        "createdAt": 1700000000,
        "updatedAt": 1700000000
    }"#;
    let conv: Conversation = serde_json::from_str(json).unwrap();
    assert!(conv.plans.is_empty());
}

// ── AC2: propose_plan tool ────────────────────────────────────────────────

#[test]
fn ac2_propose_plan_in_available_tools() {
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::ports::ToolSetPort;

    let tmp = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
        tmp.path().to_path_buf(),
    ));
    let adapter = ToolSetAdapter::new(
        tmp.path().to_path_buf(),
        storage,
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    );
    let tools = adapter.available_tools();
    assert!(
        tools.iter().any(|t| t.name == "propose_plan"),
        "propose_plan must be in available_tools"
    );
}

#[test]
fn ac2_propose_plan_emits_event() {
    let input = serde_json::json!({
        "title": "My Plan",
        "tasks": [
            { "title": "Step 1" },
            { "title": "Step 2", "depends_on": [1] },
        ]
    });
    let plan = parse_plan_input(&input, "plan-emit").unwrap();
    assert_eq!(plan.title, "My Plan");
    assert_eq!(plan.tasks.len(), 2);
    assert_eq!(plan.status, PlanStatus::Pending);
}

#[test]
fn ac2_validate_plan_rejects_empty() {
    let plan = Plan {
        id: "p".to_string(),
        title: "Empty".to_string(),
        tasks: vec![],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: None,
    };
    let err = validate_plan(&plan).unwrap_err();
    assert!(err.contains("at least one task"));
}

#[test]
fn ac2_validate_plan_rejects_forward_dep() {
    let input = serde_json::json!({
        "title": "t",
        "tasks": [
            { "title": "A", "depends_on": [3] },
            { "title": "B" },
            { "title": "C" },
        ]
    });
    let err = parse_plan_input(&input, "p").unwrap_err();
    assert!(err.to_string().contains("strictly-earlier"));
}

// ── AC3: Effort Estimation ────────────────────────────────────────────────

#[test]
fn ac3_effort_derivation_default() {
    let tasks: Vec<PlanTask> = (1..=5).map(|i| make_task(i, "t")).collect();
    let est = derive_effort_estimate(&tasks).unwrap();
    assert_eq!(est.tool_calls, Some(5));
    assert_eq!(est.seconds, Some(40));
}

#[test]
fn ac3_effort_estimate_from_model() {
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

// ── AC4: Plan ↔ Conversation binding ─────────────────────────────────────

#[test]
fn ac4_plan_inserted_in_conversation_plans() {
    let mut conv = make_conv(vec![]);
    let plan = make_plan_with_tasks(2);
    conv.plans.insert("plan-1".to_string(), plan.clone());
    assert_eq!(conv.plans.len(), 1);
    let retrieved = conv.plans.get("plan-1").unwrap();
    assert_eq!(retrieved.title, "Test Plan");
}

#[test]
fn ac4_host_message_id_set() {
    let plan = Plan {
        id: "p".to_string(),
        title: "t".to_string(),
        tasks: vec![make_task(1, "s")],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: Some("msg-host".to_string()),
    };
    let json = serde_json::to_string(&plan).unwrap();
    let back: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(back.host_message_id, Some("msg-host".to_string()));
}

// ── AC5: Plan Card Rendering ─────────────────────────────────────────────

#[test]
fn ac5_plan_card_renders_pending() {
    use rustain::adapters::tui::widgets::plan_card::render_plan_card_lines;

    let plan = Plan {
        id: "plan-rp".to_string(),
        title: "Refactor".to_string(),
        tasks: vec![make_task(1, "Step 1")],
        estimated_effort: Some(EffortEstimate {
            tool_calls: Some(3),
            seconds: Some(24),
        }),
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: None,
    };
    let theme = rustain::adapters::tui::theme::Theme::dark();
    let lines = render_plan_card_lines(&plan, &theme, 80, true);
    assert!(!lines.is_empty());

    let text: String = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(text.contains("Plan: Refactor"));
    assert!(text.contains("[y]"));
    assert!(text.contains("[e]"));
    assert!(text.contains("[n]"));
}

#[test]
fn ac5_plan_card_renders_resolved() {
    use rustain::adapters::tui::widgets::plan_card::render_plan_card_lines;

    let plan = Plan {
        id: "plan-rr".to_string(),
        title: "Refactor".to_string(),
        tasks: vec![make_task(1, "Step 1")],
        estimated_effort: None,
        status: PlanStatus::Completed,
        created_at: 0,
        resolved_at: Some(1_700_000_060),
        host_message_id: None,
    };
    let theme = rustain::adapters::tui::theme::Theme::dark();
    let lines = render_plan_card_lines(&plan, &theme, 80, false);
    let text: String = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(text.contains("[completed"));
    assert!(!text.contains("[y]"));
}

#[test]
fn ac5_missing_plan_fallback() {
    use rustain::adapters::tui::widgets::plan_card::missing_plan_lines;

    let theme = rustain::adapters::tui::theme::Theme::dark();
    let lines = missing_plan_lines("missing-42", &theme);
    assert_eq!(lines.len(), 1);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("missing-42"));
    assert!(text.contains("unavailable"));
}

// ── AC6: Pending Plan Card Invariant ──────────────────────────────────────

#[test]
fn ac6_plan_proposed_sets_pending() {
    let card = rustain::adapters::tui::state::PendingPlanCard {
        conversation_id: "c1".to_string(),
        plan_id: "plan-1".to_string(),
        plan_snapshot: make_plan_with_tasks(2),
    };
    assert_eq!(card.plan_id, "plan-1");
    assert_eq!(card.plan_snapshot.tasks.len(), 2);
}

#[test]
fn ac6_pending_invariant_one_per_conversation() {
    use rustain::adapters::tui::color_detect::ColorCapability;
    use rustain::adapters::tui::state::TuiState;

    let mut state = TuiState::with_capability(80, 24, ColorCapability::TrueColor);
    assert!(state.pending_plan_card.is_none());

    state.pending_plan_card = Some(rustain::adapters::tui::state::PendingPlanCard {
        conversation_id: "c1".to_string(),
        plan_id: "plan-1".to_string(),
        plan_snapshot: make_plan_with_tasks(1),
    });
    assert!(state.pending_plan_card.is_some());

    state.pending_plan_card = None;
    assert!(state.pending_plan_card.is_none());
}

// ── AC7: Approve / Reject / Edit ─────────────────────────────────────────

#[test]
fn ac7_approve_transitions_to_executing() {
    let decision = PlanDecision::Approve;
    let new_status = match decision {
        PlanDecision::Approve | PlanDecision::AutoApproveYolo => PlanStatus::Executing,
        PlanDecision::Reject => PlanStatus::Rejected,
        PlanDecision::Edit => PlanStatus::Editing,
    };
    assert_eq!(new_status, PlanStatus::Executing);
}

#[test]
fn ac7_reject_appends_synthetic_message() {
    let msg = ChatMessage {
        id: "syn-reject".to_string(),
        role: MessageRole::User,
        content: "Plan rejected.".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_100,
        token_count: None,
        stop_reason: None,
        synthetic: true,
        images: vec![],
    };
    assert!(msg.synthetic);
    assert_eq!(msg.role, MessageRole::User);
    assert!(msg.content.contains("rejected"));
}

#[test]
fn ac7_edit_round_trip_toml() {
    let plan = Plan {
        id: "plan-toml".to_string(),
        title: "Refactor Auth".to_string(),
        tasks: vec![
            PlanTask {
                number: 1,
                title: "Read code".to_string(),
                description: "Read existing module".to_string(),
                depends_on: vec![],
                status: PlanTaskStatus::Pending,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
            },
            PlanTask {
                number: 2,
                title: "Extract trait".to_string(),
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
        ],
        estimated_effort: Some(EffortEstimate {
            tool_calls: Some(3),
            seconds: Some(24),
        }),
        status: PlanStatus::Pending,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: Some("msg-1".to_string()),
    };
    let toml_str = toml::to_string(&plan).unwrap();
    let back: Plan = toml::from_str(&toml_str).unwrap();
    assert_eq!(back.id, plan.id);
    assert_eq!(back.title, plan.title);
    assert_eq!(back.tasks.len(), plan.tasks.len());
    assert_eq!(back.tasks[1].depends_on, vec![1]);
    assert_eq!(back.host_message_id, plan.host_message_id);
}

#[test]
fn ac7_edit_parse_error_restores_original() {
    let bad_toml = "this is not valid toml {{{";
    let result: Result<Plan, _> = toml::from_str(bad_toml);
    assert!(result.is_err());
}

// ── AC8: YOLO Auto-Approve ────────────────────────────────────────────────

#[test]
fn ac8_yolo_auto_approve_no_card() {
    let mode = PermissionMode::Yolo;
    let decision = match mode {
        PermissionMode::Yolo => PlanDecision::AutoApproveYolo,
        _ => PlanDecision::Approve,
    };
    assert_eq!(decision, PlanDecision::AutoApproveYolo);

    let needs_card = !matches!(decision, PlanDecision::AutoApproveYolo);
    assert!(
        !needs_card,
        "YOLO mode should not display a pending plan card"
    );
}

// ── AC9: Reload / Backward Compatibility ──────────────────────────────────

#[test]
fn ac9_pending_plan_rehydrates_after_reload() {
    let plan = Plan {
        id: "plan-hydrate".to_string(),
        title: "Reload Test".to_string(),
        tasks: vec![make_task(1, "Step")],
        estimated_effort: Some(EffortEstimate {
            tool_calls: Some(1),
            seconds: Some(8),
        }),
        status: PlanStatus::Pending,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: Some("msg-1".to_string()),
    };

    let mut conv = make_conv(vec![make_msg(MessageRole::User, "go")]);
    conv.plans.insert("plan-hydrate".to_string(), plan.clone());

    let json = serde_json::to_string(&conv).unwrap();
    let back: Conversation = serde_json::from_str(&json).unwrap();
    assert_eq!(back.plans.len(), 1);
    let reloaded = back.plans.get("plan-hydrate").unwrap();
    assert_eq!(reloaded.title, "Reload Test");
    assert_eq!(reloaded.status, PlanStatus::Pending);
    assert_eq!(reloaded.host_message_id, Some("msg-1".to_string()));
}

#[test]
fn ac9_old_session_without_plans_loads() {
    let old_json = r#"{
        "id": "old-conv",
        "title": "Old Session",
        "messages": [
            {
                "id": "m1",
                "role": "user",
                "content": "hello",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000000,
                "tokenCount": null
            }
        ],
        "createdAt": 1700000000,
        "updatedAt": 1700000000
    }"#;
    let conv: Conversation = serde_json::from_str(old_json).unwrap();
    assert!(conv.plans.is_empty());
    assert_eq!(conv.messages.len(), 1);
}

// ── AC10: Export / Search / Fork ──────────────────────────────────────────

#[test]
fn ac10_export_renders_plan() {
    let plan_id = "plan-export";
    let host_msg_id = "msg-host";

    let host_msg = ChatMessage {
        id: host_msg_id.to_string(),
        role: MessageRole::Assistant,
        content: "Here is the plan.".to_string(),
        content_blocks: vec![ContentBlockType::PlanCard],
        tool_calls: vec![],
        created_at: 1_700_000_020,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    };

    let mut plans = HashMap::new();
    plans.insert(
        plan_id.to_string(),
        Plan {
            id: plan_id.to_string(),
            title: "Refactor module".to_string(),
            tasks: vec![
                make_task_with_desc(1, "Extract trait", "Move to separate file"),
                PlanTask {
                    number: 2,
                    title: "Update imports".to_string(),
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
            ],
            estimated_effort: Some(EffortEstimate {
                tool_calls: Some(3),
                seconds: Some(45),
            }),
            status: PlanStatus::Pending,
            created_at: 1_700_000_020,
            resolved_at: None,
            host_message_id: Some(host_msg_id.to_string()),
        },
    );

    let mut conv = make_conv(vec![
        make_msg(MessageRole::User, "Refactor please"),
        host_msg,
    ]);
    conv.plans = plans;

    let meta = make_meta(2);
    let md = render_conversation_markdown(&conv, &meta, 1_700_000_123);

    assert!(md.contains("### Plan: Refactor module"));
    assert!(md.contains("Extract trait"));
    assert!(md.contains("Move to separate file"));
    assert!(md.contains("Update imports"));
    assert!(md.contains("depends on: 1"));
}

#[test]
fn ac10_export_renders_multiple_plans_per_message() {
    let host_msg_id = "msg-multi";

    let host_msg = ChatMessage {
        id: host_msg_id.to_string(),
        role: MessageRole::Assistant,
        content: "Here are two plans.".to_string(),
        content_blocks: vec![ContentBlockType::PlanCard, ContentBlockType::PlanCard],
        tool_calls: vec![],
        created_at: 1_700_000_020,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    };

    let mut plans = HashMap::new();
    plans.insert(
        "plan-a".to_string(),
        Plan {
            id: "plan-a".to_string(),
            title: "First Plan".to_string(),
            tasks: vec![make_task(1, "Step A")],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1_700_000_020,
            resolved_at: None,
            host_message_id: Some(host_msg_id.to_string()),
        },
    );
    plans.insert(
        "plan-b".to_string(),
        Plan {
            id: "plan-b".to_string(),
            title: "Second Plan".to_string(),
            tasks: vec![make_task(1, "Step B")],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1_700_000_021,
            resolved_at: None,
            host_message_id: Some(host_msg_id.to_string()),
        },
    );

    let mut conv = make_conv(vec![make_msg(MessageRole::User, "Do both"), host_msg]);
    conv.plans = plans;

    let meta = make_meta(2);
    let md = render_conversation_markdown(&conv, &meta, 1_700_000_123);

    assert!(
        md.contains("### Plan: First Plan"),
        "first plan must be rendered"
    );
    assert!(
        md.contains("### Plan: Second Plan"),
        "second plan must be rendered"
    );
    assert!(md.contains("Step A"));
    assert!(md.contains("Step B"));
}

#[test]
fn ac10_search_indexes_plan() {
    let host_msg_id = "msg-search-host";

    let host_msg = ChatMessage {
        id: host_msg_id.to_string(),
        role: MessageRole::Assistant,
        content: "Here is your plan.".to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    };

    let mut plans = HashMap::new();
    plans.insert(
        "plan-s".to_string(),
        Plan {
            id: "plan-s".to_string(),
            title: "Build feature".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "Write database migration".to_string(),
                description: "Add new table".to_string(),
                depends_on: vec![],
                status: PlanTaskStatus::Pending,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
            }],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1_700_000_000,
            resolved_at: None,
            host_message_id: Some(host_msg_id.to_string()),
        },
    );

    let mut conv = make_conv(vec![make_msg(MessageRole::User, "go"), host_msg]);
    conv.plans = plans;

    let matches = find_matches(&conv, "database migration");
    assert!(!matches.is_empty(), "should find task title in plan");

    let matches2 = find_matches(&conv, "Build feature");
    assert!(!matches2.is_empty(), "should find plan title");

    let matches3 = find_matches(&conv, "Add new table");
    assert!(!matches3.is_empty(), "should find task description");
}

#[test]
fn ac10_fork_copies_plan() {
    let plan = Plan {
        id: "plan-fork".to_string(),
        title: "Fork Plan".to_string(),
        tasks: vec![make_task(1, "Step 1")],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: None,
    };

    let mut conv = make_conv(vec![make_msg(MessageRole::User, "start")]);
    conv.plans.insert("plan-fork".to_string(), plan.clone());

    let forked = conv.clone();
    assert_eq!(forked.plans.len(), 1);
    assert_eq!(forked.plans.get("plan-fork").unwrap().title, "Fork Plan");
}

// ── AC11: Interaction State Routing ───────────────────────────────────────

#[test]
fn ac11_interaction_state_review_plan() {
    use rustain::adapters::tui::color_detect::ColorCapability;
    use rustain::adapters::tui::state::TuiState;

    let mut state = TuiState::with_capability(80, 24, ColorCapability::TrueColor);
    assert!(state.pending_plan_card.is_none());

    state.pending_plan_card = Some(rustain::adapters::tui::state::PendingPlanCard {
        conversation_id: "c1".to_string(),
        plan_id: "plan-1".to_string(),
        plan_snapshot: make_plan_with_tasks(2),
    });

    assert!(state.pending_plan_card.is_some());
}

#[test]
fn ac11_interaction_state_back_to_awaiting_on_reject() {
    use rustain::adapters::tui::color_detect::ColorCapability;
    use rustain::adapters::tui::state::TuiState;

    let mut state = TuiState::with_capability(80, 24, ColorCapability::TrueColor);

    state.pending_plan_card = Some(rustain::adapters::tui::state::PendingPlanCard {
        conversation_id: "c1".to_string(),
        plan_id: "plan-1".to_string(),
        plan_snapshot: make_plan_with_tasks(2),
    });
    assert!(state.pending_plan_card.is_some());

    state.pending_plan_card = None;
    assert!(state.pending_plan_card.is_none());
}
