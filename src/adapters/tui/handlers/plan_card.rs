//! Standalone, current-tab hosts for inline plan cards.
//!
//! The plan remains conversation data, but its display host is not a turn or
//! model history. Resolution and editor dispatch remain in the event loop.

use crate::adapters::tui::state::{PendingPlanCard, TuiState};
use crate::domain::events::AppEvent;
use crate::domain::models::plan::{Plan, PlanStatus};
use crate::domain::models::{
    ChatMessage, ContentBlockType, Conversation, MessageRole, NoticeLevel, PermissionMode,
};

/// Present a plan and return the ordered YOLO events for the caller to send.
/// `open_turn_id` prevents even an accidental collision with an uncommitted turn.
pub fn handle_plan_proposed(
    conversation: &mut Conversation,
    state: &mut TuiState,
    conversation_id: &str,
    mut plan: Plan,
    permission_mode: PermissionMode,
    open_turn_id: Option<&str>,
) -> Option<(AppEvent, AppEvent)> {
    if conversation.id != conversation_id {
        tracing::warn!("PlanProposed for unknown conversation {}", conversation_id);
        return None;
    }

    let host_id = loop {
        let id = crate::domain::models::generate_message_id();
        if open_turn_id != Some(id.as_str())
            && !conversation.messages.iter().any(|message| message.id == id)
            && !conversation.turns.iter().any(|turn| turn.id.0 == id)
        {
            break id;
        }
    };
    conversation.messages.push(ChatMessage {
        id: host_id.clone(),
        role: MessageRole::Assistant,
        content_blocks: vec![
            ContentBlockType::PlanCard,
            ContentBlockType::EphemeralDisplay,
        ],
        created_at: crate::domain::models::session_meta::now_unix(),
        ..Default::default()
    });

    plan.host_message_id = Some(host_id);
    let plan_id = plan.id.clone();
    let events = if permission_mode == PermissionMode::Yolo {
        let notice = AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: NoticeLevel::Info,
            message: format!(
                "Plan auto-approved (YOLO mode): {} — {} tasks",
                plan.title,
                plan.tasks.len()
            ),
        };
        plan.status = PlanStatus::Executing;
        plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
        conversation.plans.insert(plan_id.clone(), plan);
        Some((
            notice,
            AppEvent::PlanExecutionStarted {
                conversation_id: conversation.id.clone(),
                plan_id,
            },
        ))
    } else {
        conversation.plans.insert(plan_id.clone(), plan.clone());
        if let Some(old_pending) = &state.pending_plan_card {
            if let Some(old_plan) = conversation.plans.get_mut(&old_pending.plan_id) {
                tracing::warn!("Superseding pending plan card: {}", old_pending.plan_id);
                old_plan.status = PlanStatus::Rejected;
                old_plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
            }
        }
        state.pending_plan_card = Some(PendingPlanCard {
            conversation_id: conversation.id.clone(),
            plan_id,
            plan_snapshot: plan,
        });
        None
    };
    state.needs_redraw = true;
    events
}

/// Apply the selected card's state transition; dispatch retains event/editor I/O.
pub fn handle_plan_decision(
    conversation: &mut Conversation,
    plan_id: &str,
    decision: &crate::domain::models::plan::PlanDecision,
) {
    use crate::domain::models::plan::PlanDecision;
    if let Some(plan) = conversation.plans.get_mut(plan_id) {
        match decision {
            PlanDecision::Approve | PlanDecision::Reject => {
                plan.status = if *decision == PlanDecision::Approve {
                    PlanStatus::Executing
                } else {
                    PlanStatus::Rejected
                };
                plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
            }
            PlanDecision::Edit => plan.status = PlanStatus::Editing,
            PlanDecision::AutoApproveYolo => {}
        }
    }
}

/// Re-present an edited (or cancelled/invalid-edit) card on its original host.
pub fn handle_plan_edit_completed(
    conversation: &mut Conversation,
    state: &mut TuiState,
    plan_id: &str,
    edited_plan: Option<Plan>,
) {
    if let Some(stored) = conversation.plans.get_mut(plan_id) {
        if let Some(edited) = edited_plan {
            stored.title = edited.title;
            stored.tasks = edited.tasks;
            stored.estimated_effort = edited.estimated_effort;
        }
        stored.status = PlanStatus::Pending;
        state.pending_plan_card = Some(PendingPlanCard {
            conversation_id: conversation.id.clone(),
            plan_id: plan_id.to_owned(),
            plan_snapshot: stored.clone(),
        });
    }
    state.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::app::{InputAction, handle_input};
    use crate::domain::events::DomainInputEvent;
    use crate::domain::models::{StopReason, StreamChunk};
    use crate::domain::services::reducer::{reduce, test_reducer_state};

    fn plan(id: &str) -> Plan {
        Plan {
            id: id.into(),
            title: format!("plan {id}"),
            tasks: vec![],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1,
            resolved_at: None,
            host_message_id: None,
        }
    }

    fn row(id: &str, role: MessageRole) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            role,
            content: "durable content".into(),
            content_blocks: vec![ContentBlockType::Text],
            ..Default::default()
        }
    }

    fn painted(conversation: &Conversation, state: &TuiState) -> String {
        use crate::adapters::tui::state::TabRenderState;
        use crate::adapters::tui::theme::Theme;
        use crate::adapters::tui::widgets::chat_pane;
        use crate::domain::clock::MockClock;
        use crate::domain::models::{StreamingState, ViewState};
        use ratatui::{Terminal, backend::TestBackend};
        use std::collections::{BTreeMap, HashMap};

        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        let mut tab = TabRenderState::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render_with_search(
                    frame,
                    area,
                    conversation,
                    None,
                    &StreamingState::default(),
                    &ViewState::default(),
                    &MockClock::at_wall_ms(1000),
                    0,
                    true,
                    &Theme::dark(),
                    &mut tab,
                    &HashMap::new(),
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    &[],
                    state.pending_plan_card.as_ref(),
                    None,
                    None,
                    None,
                    None,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area().height)
            .map(|y| {
                (0..buffer.area().width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn hosts_are_distinct_from_history_and_open_turn_and_survive_commit() {
        for scenario in 0..4 {
            let mut conversation = Conversation {
                id: "conversation".into(),
                ..Default::default()
            };
            let mut state = TuiState::new(80, 24);
            let (mut reducer, clock) = test_reducer_state(1000);
            if scenario == 1 {
                conversation.messages.push(row("user", MessageRole::User));
            } else if scenario >= 2 {
                reduce(
                    &mut reducer,
                    StreamChunk::Text {
                        content: "durable content".into(),
                        parent_tool_use_id: None,
                    },
                    &clock,
                );
                reduce(
                    &mut reducer,
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                    &clock,
                );
                conversation
                    .turns
                    .push(reducer.committed_turn.take().unwrap());
                conversation.rebuild_messages_mirror();
            }
            if scenario == 3 {
                reduce(
                    &mut reducer,
                    StreamChunk::Text {
                        content: "open".into(),
                        parent_tool_use_id: None,
                    },
                    &clock,
                );
            }
            let before = serde_json::to_value(&conversation.messages).unwrap();
            let turn_count = conversation.turns.len();
            let open_id = reducer.open_turn.as_ref().map(|turn| turn.id.0.as_str());
            assert!(
                handle_plan_proposed(
                    &mut conversation,
                    &mut state,
                    "conversation",
                    plan("p"),
                    PermissionMode::Normal,
                    open_id
                )
                .is_none()
            );
            let host = conversation.plans["p"].host_message_id.clone().unwrap();
            assert_ne!(open_id, Some(host.as_str()));
            assert!(!conversation.turns.iter().any(|turn| turn.id.0 == host));
            assert_eq!(conversation.turns.len(), turn_count);
            assert_eq!(
                serde_json::to_value(&conversation.messages[..conversation.messages.len() - 1])
                    .unwrap(),
                before
            );
            let message = conversation.messages.last().unwrap();
            assert_eq!(message.id, host);
            assert!(message.is_ephemeral_display());
            assert!(message.content_blocks.contains(&ContentBlockType::PlanCard));
            assert!(painted(&conversation, &state).contains("plan p"));

            if reducer.open_turn.is_none() {
                reduce(
                    &mut reducer,
                    StreamChunk::Text {
                        content: "later".into(),
                        parent_tool_use_id: None,
                    },
                    &clock,
                );
            }
            reduce(
                &mut reducer,
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
                &clock,
            );
            conversation
                .turns
                .push(reducer.committed_turn.take().unwrap());
            conversation.rebuild_messages_mirror();
            assert!(
                conversation
                    .messages
                    .iter()
                    .any(|message| message.id == host && message.is_ephemeral_display())
            );
            assert_eq!(state.pending_plan_card.as_ref().unwrap().plan_id, "p");
            assert!(painted(&conversation, &state).contains("plan p"));
            for (key, action) in [
                ('y', InputAction::PlanCardApprove),
                ('n', InputAction::PlanCardReject),
                ('e', InputAction::PlanCardEdit),
            ] {
                assert_eq!(
                    handle_input(&mut state, &DomainInputEvent::KeyPress(key)),
                    action
                );
            }
        }
    }

    #[test]
    fn supersession_rejects_old_plan_and_keeps_distinct_hosts() {
        let mut conversation = Conversation {
            id: "c".into(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        handle_plan_proposed(
            &mut conversation,
            &mut state,
            "c",
            plan("old"),
            PermissionMode::Normal,
            None,
        );
        handle_plan_proposed(
            &mut conversation,
            &mut state,
            "c",
            plan("new"),
            PermissionMode::Normal,
            None,
        );
        assert_eq!(conversation.plans["old"].status, PlanStatus::Rejected);
        assert!(conversation.plans["old"].resolved_at.is_some());
        assert_eq!(conversation.plans["new"].status, PlanStatus::Pending);
        assert_ne!(
            conversation.plans["old"].host_message_id,
            conversation.plans["new"].host_message_id
        );
        assert_eq!(state.pending_plan_card.as_ref().unwrap().plan_id, "new");
        conversation.rebuild_messages_mirror();
        assert_eq!(conversation.messages.len(), 2);
    }

    #[test]
    fn yolo_emits_notice_then_execution_and_preserves_pending_state() {
        let mut conversation = Conversation {
            id: "c".into(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        handle_plan_proposed(
            &mut conversation,
            &mut state,
            "c",
            plan("pending"),
            PermissionMode::Normal,
            None,
        );
        let (notice, start) = handle_plan_proposed(
            &mut conversation,
            &mut state,
            "c",
            plan("auto"),
            PermissionMode::Yolo,
            None,
        )
        .unwrap();
        assert!(
            matches!(notice, AppEvent::SystemNotice { conversation_id: Some(id), level: NoticeLevel::Info, message } if id == "c" && message.contains("plan auto") && message.contains("0 tasks"))
        );
        assert!(
            matches!(start, AppEvent::PlanExecutionStarted { conversation_id, plan_id } if conversation_id == "c" && plan_id == "auto")
        );
        assert_eq!(conversation.plans["auto"].status, PlanStatus::Executing);
        assert!(conversation.plans["auto"].resolved_at.is_some());
        assert_eq!(state.pending_plan_card.as_ref().unwrap().plan_id, "pending");
        assert_eq!(conversation.plans["pending"].status, PlanStatus::Pending);
    }

    #[test]
    fn mismatched_conversation_has_no_effect() {
        let mut conversation = Conversation {
            id: "c".into(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        state.needs_redraw = false;
        assert!(
            handle_plan_proposed(
                &mut conversation,
                &mut state,
                "other",
                plan("p"),
                PermissionMode::Normal,
                None
            )
            .is_none()
        );
        assert!(conversation.messages.is_empty());
        assert!(conversation.plans.is_empty());
        assert!(state.pending_plan_card.is_none());
        assert!(!state.needs_redraw);
    }
    #[test]
    fn decisions_and_edit_completion_change_only_the_selected_plan_and_keep_host() {
        use crate::domain::models::plan::PlanDecision;
        for (key, decision, expected) in [
            ('y', PlanDecision::Approve, PlanStatus::Executing),
            ('n', PlanDecision::Reject, PlanStatus::Rejected),
            ('e', PlanDecision::Edit, PlanStatus::Editing),
        ] {
            let mut conversation = Conversation {
                id: "c".into(),
                ..Default::default()
            };
            let mut state = TuiState::new(80, 24);
            conversation.plans.insert("control".into(), plan("control"));
            handle_plan_proposed(
                &mut conversation,
                &mut state,
                "c",
                plan("selected"),
                PermissionMode::Normal,
                None,
            );
            let host = conversation.plans["selected"].host_message_id.clone();
            let action = handle_input(&mut state, &DomainInputEvent::KeyPress(key));
            assert!(matches!(
                (action, &decision),
                (InputAction::PlanCardApprove, PlanDecision::Approve)
                    | (InputAction::PlanCardReject, PlanDecision::Reject)
                    | (InputAction::PlanCardEdit, PlanDecision::Edit)
            ));
            handle_plan_decision(&mut conversation, "selected", &decision);
            assert_eq!(conversation.plans["selected"].status, expected);
            assert_eq!(conversation.plans["control"].status, PlanStatus::Pending);
            assert_eq!(conversation.plans["selected"].host_message_id, host);
            if decision == PlanDecision::Edit {
                let mut edited = plan("editor-output-id-is-not-authority");
                edited.title = "edited title".into();
                handle_plan_edit_completed(&mut conversation, &mut state, "selected", Some(edited));
                assert_eq!(conversation.plans["selected"].title, "edited title");
                assert_eq!(conversation.plans["selected"].status, PlanStatus::Pending);
                assert_eq!(
                    state.pending_plan_card.as_ref().unwrap().plan_id,
                    "selected"
                );
                assert_eq!(
                    state
                        .pending_plan_card
                        .as_ref()
                        .unwrap()
                        .plan_snapshot
                        .title,
                    "edited title"
                );
                handle_plan_decision(&mut conversation, "selected", &PlanDecision::Edit);
                handle_plan_edit_completed(&mut conversation, &mut state, "selected", None);
                assert_eq!(conversation.plans["selected"].title, "edited title");
                conversation.rebuild_messages_mirror();
                assert!(painted(&conversation, &state).contains("edited title"));
                assert_eq!(conversation.plans["selected"].host_message_id, host);
            } else {
                assert!(conversation.plans["selected"].resolved_at.is_some());
            }
        }
    }
}
