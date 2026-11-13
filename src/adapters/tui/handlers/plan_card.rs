//! Standalone, current-tab hosts for inline plan cards.
//!
//! The plan remains conversation data, but its display host is not a turn or
//! model history. Resolution and editor dispatch remain in the event loop.

use crate::adapters::tui::decision_queue::{self, DecisionSubject, PlanCardPayload};
use crate::adapters::tui::state::TuiState;
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
        if let Some(old_id) = pending_plan_id(&state.decisions, &conversation.id).map(str::to_owned)
        {
            if let Some(old_plan) = conversation.plans.get_mut(&old_id) {
                tracing::warn!("Superseding pending plan card: {}", old_id);
                old_plan.status = PlanStatus::Rejected;
                old_plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
            }
            decision_queue::withdraw_plan_card(state, &old_id);
        }
        decision_queue::raise_plan_card(
            state,
            PlanCardPayload {
                conversation_id: conversation.id.clone(),
                plan_id,
                plan_snapshot: plan,
            },
        );
        None
    };
    state.needs_redraw = true;
    events
}

/// Apply the selected card's state transition; dispatch retains event/editor I/O.
pub fn handle_plan_decision(
    conversation: &mut Conversation,
    state: &mut TuiState,
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
    // The card settles the plan the conversation's plan approval (`exit_plan_mode`) asks
    // about: left queued it would stay in `Needs you` after the user already decided.
    if matches!(decision, PlanDecision::Approve | PlanDecision::Reject) {
        decision_queue::withdraw_for(
            state,
            decision_queue::DecisionKind::PlanApproval,
            &conversation.id,
        );
    }
}

/// The ordered events a decided card starts. Approving from Plan mode first leaves it (the
/// shared `SetPermissionMode` transition: permissions, sandbox, plan-file exception), or the
/// structured run would start under Plan's read-only rules; every other mode is kept.
pub fn card_decision_events(
    decision: &crate::domain::models::plan::PlanDecision,
    permission_mode: PermissionMode,
    conversation_id: &str,
    plan_id: &str,
) -> Vec<AppEvent> {
    use crate::domain::models::plan::PlanDecision;
    if *decision != PlanDecision::Approve {
        return Vec::new();
    }
    let mut events = Vec::with_capacity(2);
    if permission_mode == PermissionMode::Plan {
        events.push(AppEvent::SetPermissionMode(PermissionMode::Normal));
    }
    events.push(AppEvent::PlanExecutionStarted {
        conversation_id: conversation_id.to_owned(),
        plan_id: plan_id.to_owned(),
    });
    events
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
        decision_queue::raise_plan_card(
            state,
            PlanCardPayload {
                conversation_id: conversation.id.clone(),
                plan_id: plan_id.to_owned(),
                plan_snapshot: stored.clone(),
            },
        );
    }
    state.needs_redraw = true;
}

/// The plan whose LIVE card this conversation holds, if any (a new proposal supersedes
/// it; the Stage row of that plan paints the pending marker). A superseded card kept
/// stale behind an open frame is not pending.
pub fn pending_plan_id<'a>(
    decisions: &'a decision_queue::DecisionQueue,
    conversation_id: &str,
) -> Option<&'a str> {
    decisions.items().find_map(|item| match &item.subject {
        DecisionSubject::PlanCard(card)
            if card.conversation_id == conversation_id && !item.is_stale() =>
        {
            Some(card.plan_id.as_str())
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::app::{InputAction, handle_input};
    use crate::adapters::tui::decision_queue::{
        DecisionKind, plan_card_pending, plan_card_snapshot,
    };
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
            origin_restrictions: None,
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
    fn pending_id(state: &TuiState) -> Option<String> {
        state
            .decisions
            .items()
            .find_map(|item| match &item.subject {
                DecisionSubject::PlanCard(card) => Some(card.plan_id.clone()),
                _ => None,
            })
    }

    fn painted(conversation: &Conversation, state: &TuiState) -> String {
        use crate::adapters::tui::state::TabRenderState;
        use crate::adapters::tui::theme::Theme;
        use crate::adapters::tui::widgets::chat_pane;
        use crate::domain::clock::MockClock;
        use crate::domain::models::{StreamingState, ViewState};
        use ratatui::{Terminal, backend::TestBackend};
        use std::collections::BTreeMap;

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
                    state.scroll_offset(),
                    state.auto_scroll(),
                    &Theme::dark(),
                    &mut tab,
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    &[],
                    pending_id(state).as_deref(),
                    None,
                    None,
                    Default::default(),
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
            assert!(plan_card_pending(&state, "p"));
            assert!(painted(&conversation, &state).contains("plan p"));
            // Story 21.6: y/n/e belong to the plan's decision frame, never to Chat.
            state.focus = crate::domain::models::FocusState::Chat;
            for key in ['y', 'n', 'e'] {
                let action = handle_input(&mut state, &DomainInputEvent::KeyPress(key));
                assert!(
                    !matches!(
                        action,
                        InputAction::PlanCardApprove
                            | InputAction::PlanCardReject
                            | InputAction::PlanCardEdit
                    ),
                    "{key}: {action:?}"
                );
            }
        }
    }

    #[test]
    fn story21_3_plan_arrival_while_reading_does_not_activate_an_unseen_card() {
        use crate::domain::models::FocusState;
        for edited in [false, true] {
            let mut conversation = Conversation {
                id: "c".into(),
                ..Default::default()
            };
            conversation.messages = (0..100)
                .map(|i| row(&format!("row-{i}"), MessageRole::User))
                .collect();
            let mut state = TuiState::new(80, 24);
            state.set_scroll_offset(100);
            state.set_auto_scroll(false);
            handle_plan_proposed(
                &mut conversation,
                &mut state,
                "c",
                plan("unseen"),
                PermissionMode::Normal,
                None,
            );
            if edited {
                state.focus = FocusState::Input;
                handle_plan_edit_completed(&mut conversation, &mut state, "unseen", None);
            }
            assert!(!painted(&conversation, &state).contains("plan unseen"));
            // Story 21.6: the plan is a queue item, so its arrival is the queue's call
            // (RU-6); what it must never do is scroll the reader or answer the plan.
            assert!(plan_card_pending(&state, "unseen"), "edited={edited}");
            for key in ['y', 'n'] {
                let action = handle_input(&mut state, &DomainInputEvent::KeyPress(key));
                assert!(
                    !matches!(
                        action,
                        InputAction::PlanCardApprove | InputAction::PlanCardReject
                    ),
                    "{key}: {action:?}"
                );
            }
            assert_eq!(conversation.plans["unseen"].status, PlanStatus::Pending);
            assert!(plan_card_pending(&state, "unseen"));
            assert_eq!(state.scroll_offset(), 100);
            assert!(!state.auto_scroll());
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
        assert!(plan_card_pending(&state, "new"));
        assert!(!plan_card_pending(&state, "old"));
        // The superseded card was open as a frame: it turns stale (Close removes it).
        assert_eq!(state.decisions.count_live(DecisionKind::PlanCard), 1);
        conversation.rebuild_messages_mirror();
        assert_eq!(conversation.messages.len(), 2);
        // A third proposal while the first frame is still open (kept stale): the live
        // plan is superseded, never the retained stale one.
        handle_plan_proposed(
            &mut conversation,
            &mut state,
            "c",
            plan("third"),
            PermissionMode::Normal,
            None,
        );
        assert_eq!(conversation.plans["new"].status, PlanStatus::Rejected);
        assert_eq!(conversation.plans["third"].status, PlanStatus::Pending);
        assert_eq!(state.decisions.count_live(DecisionKind::PlanCard), 1);
        assert!(plan_card_pending(&state, "third"));
        assert!(!plan_card_pending(&state, "new"));
        assert!(
            state
                .decisions
                .items()
                .any(|item| item.source == "old" && item.is_stale()),
            "the open first frame stays, Close-only"
        );
        assert_eq!(pending_plan_id(&state.decisions, "c"), Some("third"));
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
        assert!(plan_card_pending(&state, "pending"));
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
        assert_eq!(state.decisions.count(DecisionKind::PlanCard), 0);
        assert!(!state.needs_redraw);
    }
    #[test]
    fn decisions_and_edit_completion_change_only_the_selected_plan_and_keep_host() {
        use crate::domain::models::plan::PlanDecision;
        for (decision, expected) in [
            (PlanDecision::Approve, PlanStatus::Executing),
            (PlanDecision::Reject, PlanStatus::Rejected),
            (PlanDecision::Edit, PlanStatus::Editing),
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
            // Story 21.6: the plan is answerable only through its decision frame; the
            // card is raised (and stays raised) until that frame resolves it.
            let host = conversation.plans["selected"].host_message_id.clone();
            assert!(plan_card_pending(&state, "selected"));
            handle_plan_decision(&mut conversation, &mut state, "selected", &decision);
            assert_eq!(conversation.plans["selected"].status, expected);
            assert_eq!(conversation.plans["control"].status, PlanStatus::Pending);
            assert_eq!(conversation.plans["selected"].host_message_id, host);
            if decision == PlanDecision::Edit {
                let mut edited = plan("editor-output-id-is-not-authority");
                edited.title = "edited title".into();
                handle_plan_edit_completed(&mut conversation, &mut state, "selected", Some(edited));
                assert_eq!(conversation.plans["selected"].title, "edited title");
                assert_eq!(conversation.plans["selected"].status, PlanStatus::Pending);
                assert!(plan_card_pending(&state, "selected"));
                assert_eq!(
                    plan_card_snapshot(&state, "selected").unwrap().title,
                    "edited title"
                );
                handle_plan_decision(
                    &mut conversation,
                    &mut state,
                    "selected",
                    &PlanDecision::Edit,
                );
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

    /// Deciding the plan card (approve or reject) settles the plan its conversation's
    /// plan approval (`exit_plan_mode`) asks about: that approval must not linger in
    /// `Needs you`. Another conversation's approval, and an edit, leave it alone.
    #[test]
    fn deciding_the_plan_card_retires_the_conversations_plan_approval() {
        use crate::adapters::tui::decision_queue::{
            PlanApprovalPayload, raise_plan_approval, waiting,
        };
        use crate::domain::models::plan::PlanDecision;
        let approval = |conversation: &str, path: &str| PlanApprovalPayload {
            conversation_id: conversation.into(),
            plan_path: path.into(),
            contents: "# plan".into(),
            summary: "Plan ready for review.".into(),
        };
        for (decision, retires) in [
            (PlanDecision::Approve, true),
            (PlanDecision::Reject, true),
            (PlanDecision::Edit, false),
        ] {
            let mut conversation = Conversation {
                id: "c".into(),
                ..Default::default()
            };
            let mut state = TuiState::new(80, 24);
            handle_plan_proposed(
                &mut conversation,
                &mut state,
                "c",
                plan("p"),
                PermissionMode::Normal,
                None,
            );
            raise_plan_approval(&mut state, approval("c", "/w/plan.md"));
            raise_plan_approval(&mut state, approval("other", "/w/plan.md"));
            assert_eq!(waiting(&state), 3);
            // The card item is answered (it leaves the queue) before the event is applied.
            decision_queue::withdraw_plan_card(&mut state, "p");
            handle_plan_decision(&mut conversation, &mut state, "p", &decision);
            let mine = state
                .decisions
                .items()
                .filter(|item| {
                    item.kind == DecisionKind::PlanApproval
                        && item.conversation_id.as_deref() == Some("c")
                })
                .count();
            assert_eq!(mine, usize::from(!retires), "{decision:?}");
            assert_eq!(
                state.decisions.count(DecisionKind::PlanApproval),
                1 + usize::from(!retires),
                "another conversation's approval stays: {decision:?}"
            );
        }
    }

    /// Approving a card from Plan mode leaves Plan mode before the single execution start;
    /// other modes are kept; reject and edit never exit.
    #[test]
    fn card_approval_leaves_plan_mode_before_execution_starts() {
        use crate::domain::models::plan::PlanDecision;
        let shape = |events: &[AppEvent]| -> Vec<String> {
            events
                .iter()
                .map(|event| match event {
                    AppEvent::SetPermissionMode(mode) => format!("mode:{mode:?}"),
                    AppEvent::PlanExecutionStarted { plan_id, .. } => format!("start:{plan_id}"),
                    other => format!("other:{other:?}"),
                })
                .collect()
        };
        assert_eq!(
            shape(&card_decision_events(
                &PlanDecision::Approve,
                PermissionMode::Plan,
                "c",
                "p"
            )),
            ["mode:Normal", "start:p"]
        );
        for kept in [
            PermissionMode::Normal,
            PermissionMode::AutoEdit,
            PermissionMode::Yolo,
        ] {
            assert_eq!(
                shape(&card_decision_events(
                    &PlanDecision::Approve,
                    kept,
                    "c",
                    "p"
                )),
                ["start:p"],
                "{kept:?}"
            );
        }
        for decision in [PlanDecision::Reject, PlanDecision::Edit] {
            assert!(card_decision_events(&decision, PermissionMode::Plan, "c", "p").is_empty());
        }
    }
}
