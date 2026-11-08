//! Local-host facts for the shared S-QUIT decision (Story 21.2 AC7).
//!
//! Gathers REAL execution and draft facts from every owned tab, the pending
//! attachments and peer composition — never status text or a completed wave's
//! stale handle — then asks the shared `quit_confirm::request` transition. The
//! event loop calls these for every quit route (`q`, Ctrl+Q, idle Ctrl+C,
//! `/quit`) so the guard cannot be bypassed by one ingress.

use crate::adapters::tui::decision_queue::DecisionKind;
use crate::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::Conversation;
use crate::domain::models::StreamingState;
use crate::domain::models::plan::PlanStatus;
use crate::domain::models::tab::TabManager;
use crate::domain::services::turn_queue::TurnQueue;
use crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;

/// Gather the local host's quit facts. The active tab's live values live in the
/// event loop's proxies (`streaming`, `turn_queue`); inactive tabs carry theirs.
pub(crate) fn local_facts(
    state: &TuiState,
    conversation: &Conversation,
    tabs: &TabManager,
    streaming: &StreamingState,
    lifecycle: &TuiTurnLifecycle,
    turn_queue: &TurnQueue,
) -> QuitFacts {
    let mut facts = QuitFacts::idle(QuitHost::Local);
    let active = tabs.active_tab_index();
    for (index, tab) in tabs.tabs().iter().enumerate() {
        let (tab_streaming, queued) = if index == active {
            (streaming, turn_queue.len())
        } else {
            (&tab.streaming, tab.turn_queue.len())
        };
        if tab_streaming.is_streaming
            || lifecycle.is_live(&tab.conversation.id)
            || tab.reducer.open_turn.is_some()
        {
            facts.streams += 1;
        }
        facts.live_tools += tab_streaming.active_tool_calls.len();
        facts.queued_turns += queued;
    }
    if state.is_wave_in_flight() {
        // Only a wave whose fan-out is still in flight counts; a completed
        // wave's retained handle (control-only) is not live work.
        let running = state.wave_run.as_ref().map_or(0, |wave| {
            let snapshot = wave.snapshot();
            snapshot.spoke_count.saturating_sub(snapshot.completed)
        });
        facts.agents = running.max(1);
    }
    facts.composer_draft = !state.input_buffer.trim().is_empty();
    // A queued large image is an unsent attachment: it counts once, as an image. A stale
    // item can no longer be answered, so quitting leaves nothing unanswered for it.
    let large_images = state.decisions.count_live(DecisionKind::LargeImage);
    facts.pending_images = state.pending_images.len() + large_images;
    facts.peer_compose = state.pinned_peer_compose.is_some()
        || crate::adapters::tui::handlers::recipient_actions::pending_composition(state).is_some();
    facts.pending_decisions = usize::from(state.pending_consolidation_card.is_some())
        + (state.decisions.undecided() - large_images)
        - settled_plan_cards(state, conversation, tabs);
    facts
}

/// A plan-card item whose plan already left Pending/Editing is not an open decision
/// (the guard once asked "1 pending decision" for a rejected plan). A card whose plan
/// cannot be found is counted: the guard errs toward asking.
fn settled_plan_cards(state: &TuiState, conversation: &Conversation, tabs: &TabManager) -> usize {
    state
        .decisions
        .items()
        .filter(|item| item.kind == DecisionKind::PlanCard && !item.is_stale())
        .filter(|item| {
            let crate::adapters::tui::decision_queue::DecisionSubject::PlanCard(card) =
                &item.subject
            else {
                return false;
            };
            let plan = conversation.plans.get(&card.plan_id).or_else(|| {
                tabs.tabs()
                    .iter()
                    .find_map(|tab| tab.conversation.plans.get(&card.plan_id))
            });
            plan.is_some_and(|plan| {
                !matches!(plan.status, PlanStatus::Pending | PlanStatus::Editing)
            })
        })
        .count()
}

/// Request quit on the local host. `ExitNow` means nothing is running or
/// unsent in any tab and no frame is open; the caller performs its exit.
pub(crate) fn request_local(
    state: &mut TuiState,
    conversation: &Conversation,
    tabs: &TabManager,
    streaming: &StreamingState,
    lifecycle: &TuiTurnLifecycle,
    turn_queue: &TurnQueue,
) -> QuitRequest {
    let facts = local_facts(state, conversation, tabs, streaming, lifecycle, turn_queue);
    quit_confirm::request(state, facts)
}

/// A quit key seen by a startup/banner interceptor (recovery prompt, carryover
/// prompt, adapter-override banner). They sit ABOVE the normal quit routes, so
/// they must ask the same guard: true only when the host really exits (every owned
/// tab is closed first); otherwise System/Quit is open and the banner stays.
pub(crate) fn quit_from_banner(
    state: &mut TuiState,
    conversation: &mut Conversation,
    tabs: &mut TabManager,
    streaming: &mut StreamingState,
    lifecycle: &mut TuiTurnLifecycle,
    turn_queue: &TurnQueue,
) -> bool {
    if request_local(state, conversation, tabs, streaming, lifecycle, turn_queue)
        != QuitRequest::ExitNow
    {
        state.needs_redraw = true;
        return false;
    }
    lifecycle.interrupt_all(
        tabs,
        conversation,
        streaming,
        &mut state.pending_resolved_model,
    );
    state.should_quit = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
    use crate::adapters::tui::quit_confirm::{QuitKey, QuitKeyOutcome, handle_key};
    use crate::domain::models::FocusState;
    use crate::domain::models::ImageAttachment;
    use crate::domain::models::UserMessage;
    use tokio_util::sync::CancellationToken;

    struct Host {
        state: TuiState,
        tabs: TabManager,
        streaming: StreamingState,
        lifecycle: TuiTurnLifecycle,
        queue: TurnQueue,
        conversation: Conversation,
    }

    fn host() -> Host {
        Host {
            state: TuiState::new(80, 24),
            tabs: TabManager::new(CancellationToken::new()),
            streaming: StreamingState::default(),
            lifecycle: TuiTurnLifecycle::default(),
            queue: TurnQueue::default(),
            conversation: Conversation::default(),
        }
    }

    fn plan_card(
        id: &str,
        status: PlanStatus,
    ) -> (
        crate::adapters::tui::decision_queue::PlanCardPayload,
        crate::domain::models::Plan,
    ) {
        let plan = crate::domain::models::Plan {
            id: id.into(),
            title: "t".into(),
            tasks: vec![],
            estimated_effort: None,
            status,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let card = crate::adapters::tui::decision_queue::PlanCardPayload {
            conversation_id: "c".into(),
            plan_id: id.into(),
            plan_snapshot: plan.clone(),
        };
        (card, plan)
    }

    /// The pending-plan pointer outlives its decision; only a plan that still awaits
    /// one is an open decision (found by the S-QUIT smoke: a rejected plan said
    /// "1 pending decision will go unanswered").
    #[test]
    fn story21_2_resolved_plan_card_is_not_a_pending_decision_but_an_open_one_is() {
        let mut h = host();
        let (card, plan) = plan_card("p", PlanStatus::Rejected);
        decision_queue::raise_plan_card(&mut h.state, card);
        h.conversation.plans.insert("p".into(), plan);
        assert_eq!(
            ask(&mut h),
            QuitRequest::ExitNow,
            "a rejected plan holds no decision"
        );

        let mut open = host();
        let (card, plan) = plan_card("p", PlanStatus::Pending);
        decision_queue::raise_plan_card(&mut open.state, card);
        open.conversation.plans.insert("p".into(), plan);
        assert_eq!(
            ask(&mut open),
            QuitRequest::Confirming,
            "a pending plan is an open decision"
        );

        let mut missing = host();
        decision_queue::raise_plan_card(
            &mut missing.state,
            plan_card("gone", PlanStatus::Pending).0,
        );
        assert_eq!(
            ask(&mut missing),
            QuitRequest::Confirming,
            "an unverifiable card still asks"
        );
        // Every status: only Pending and Editing are still awaiting a decision.
        for (status, open) in [
            (PlanStatus::Pending, true),
            (PlanStatus::Editing, true),
            (PlanStatus::Executing, false),
            (PlanStatus::Completed, false),
            (PlanStatus::Rejected, false),
            (PlanStatus::Cancelled, false),
        ] {
            let mut each = host();
            let (card, plan) = plan_card("s", status);
            decision_queue::raise_plan_card(&mut each.state, card);
            each.conversation.plans.insert("s".into(), plan);
            let expected = if open {
                QuitRequest::Confirming
            } else {
                QuitRequest::ExitNow
            };
            assert_eq!(ask(&mut each), expected, "{status:?}");
        }
    }

    fn ask(h: &mut Host) -> QuitRequest {
        request_local(
            &mut h.state,
            &h.conversation,
            &h.tabs,
            &h.streaming,
            &h.lifecycle,
            &h.queue,
        )
    }

    #[test]
    fn story21_2_idle_host_exits_directly() {
        let mut h = host();
        assert_eq!(ask(&mut h), QuitRequest::ExitNow);
        assert!(h.state.quit_confirm.is_none());
    }

    #[test]
    fn story21_2_every_work_and_draft_signal_raises_the_guard_and_cancel_changes_nothing() {
        type Arm = Box<dyn Fn(&mut Host)>;
        let cases: Vec<(&str, Arm)> = vec![
            (
                "composer draft",
                Box::new(|h| h.state.input_buffer = "unsent".into()),
            ),
            (
                "active stream",
                Box::new(|h| h.streaming.is_streaming = true),
            ),
            (
                "pending image",
                Box::new(|h| {
                    h.state.pending_images.push(ImageAttachment {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    })
                }),
            ),
            (
                "queued turn on the active tab",
                Box::new(|h| {
                    h.queue
                        .enqueue(UserMessage {
                            content: "later".into(),
                            images: vec![],
                        })
                        .unwrap();
                }),
            ),
            (
                "in-flight wave",
                Box::new(|h| {
                    h.state.begin_wave_fanout();
                }),
            ),
            (
                "inactive tab stream",
                Box::new(|h| {
                    h.tabs.create_tab();
                    h.tabs.switch_to_index(1); // 1-based: the original tab is active; the work is on the INACTIVE one
                    h.tabs.tabs_mut()[1].streaming.is_streaming = true;
                }),
            ),
            (
                "inactive tab queued draft",
                Box::new(|h| {
                    h.tabs.create_tab();
                    h.tabs.switch_to_index(1); // 1-based: the original tab is active; the work is on the INACTIVE one
                    h.tabs.tabs_mut()[1]
                        .turn_queue
                        .enqueue(UserMessage {
                            content: "bg".into(),
                            images: vec![],
                        })
                        .unwrap();
                }),
            ),
        ];
        for (label, arm) in cases {
            let mut h = host();
            h.state.focus = FocusState::Chat;
            arm(&mut h);
            let draft = h.state.input_buffer.clone();
            let stream = h.streaming.is_streaming;
            let queued = h.queue.len();
            assert_eq!(
                ask(&mut h),
                QuitRequest::Confirming,
                "{label} must raise S-QUIT"
            );
            // Cancelling never interrupts work, drains the queue or edits the draft.
            assert_eq!(
                handle_key(&mut h.state, QuitKey::Esc),
                QuitKeyOutcome::Cancelled,
                "{label}"
            );
            assert_eq!(h.state.input_buffer, draft, "{label}: draft untouched");
            assert_eq!(
                h.streaming.is_streaming, stream,
                "{label}: stream untouched"
            );
            assert_eq!(h.queue.len(), queued, "{label}: queue untouched");
            assert_eq!(h.state.focus, FocusState::Chat, "{label}: caller restored");
        }
    }

    #[test]
    fn story21_2_completed_wave_handle_is_not_live_work() {
        let mut h = host();
        let id = h.state.begin_wave_fanout();
        h.state.retire_wave_fanout(Some(id));
        assert!(!h.state.is_wave_in_flight());
        assert_eq!(ask(&mut h), QuitRequest::ExitNow);
    }

    #[test]
    fn story21_2_second_request_never_stacks_a_frame() {
        let mut h = host();
        h.state.input_buffer = "draft".into();
        assert_eq!(ask(&mut h), QuitRequest::Confirming);
        assert_eq!(ask(&mut h), QuitRequest::Confirming);
        assert_eq!(
            handle_key(&mut h.state, QuitKey::Char('y')),
            QuitKeyOutcome::Confirmed
        );
        assert!(
            h.state.quit_confirm.is_none(),
            "one y closes the only frame"
        );
    }

    fn banner_quit(h: &mut Host) -> bool {
        quit_from_banner(
            &mut h.state,
            &mut h.conversation,
            &mut h.tabs,
            &mut h.streaming,
            &mut h.lifecycle,
            &h.queue,
        )
    }

    /// One scenario per decision-pane kind: the item is pending AND there is other
    /// work (an inactive tab streaming) or an unsent draft; Ctrl+Q must open
    /// System/Quit, keep the item and never set `should_quit`. The item itself is
    /// counted as a pending decision (AC10), so even an otherwise idle host asks.
    fn assert_pane_quit_asks(kind: DecisionKind, arm: impl Fn(&mut Host)) {
        let mut h = host();
        decision_queue::raise(
            &mut h.state,
            DecisionDraft::new(kind, "pending"),
            Arrival::Requested,
        );
        arm(&mut h);
        assert!(!banner_quit(&mut h), "{kind:?}: Ctrl+Q must ask first");
        assert!(!h.state.should_quit, "{kind:?}: no silent exit");
        assert!(h.state.quit_confirm.is_some(), "{kind:?}: System/Quit open");
        assert!(h.state.decisions.holds(kind), "{kind:?}: the item stays");
        // Positive control: the same host with the item resolved exits.
        let mut idle = host();
        assert!(banner_quit(&mut idle), "{kind:?}: idle host exits");
        assert!(idle.state.should_quit);
    }

    #[test]
    fn story21_2_recovery_prompt_ctrl_q_asks_when_other_work_exists() {
        assert_pane_quit_asks(DecisionKind::Recovery, |h| {
            h.tabs.create_tab();
            h.tabs.switch_to_index(1);
            h.tabs.tabs_mut()[1].streaming.is_streaming = true;
        });
    }

    #[test]
    fn story21_2_carryover_prompt_ctrl_q_asks_when_a_draft_exists() {
        assert_pane_quit_asks(DecisionKind::Carryover, |h| {
            h.state.input_buffer = "unsent".into()
        });
    }

    #[test]
    fn story21_2_budget_decision_ctrl_q_asks_when_the_active_tab_streams() {
        // Was the override-status banner (dead prompt, deleted in S1/A11): the
        // third scenario now exercises the daily-budget item.
        assert_pane_quit_asks(DecisionKind::Budget, |h| h.streaming.is_streaming = true);
    }

    #[test]
    fn story21_1_every_queue_kind_is_counted_once_as_a_decision_or_an_image() {
        for kind in [
            DecisionKind::Recovery,
            DecisionKind::Carryover,
            DecisionKind::Budget,
            DecisionKind::ContextWarning,
            DecisionKind::LargeImage,
            DecisionKind::DeleteConversations,
        ] {
            let mut h = host();
            decision_queue::raise(
                &mut h.state,
                DecisionDraft::new(kind, "x"),
                Arrival::Requested,
            );
            let facts = local_facts(
                &h.state,
                &h.conversation,
                &h.tabs,
                &h.streaming,
                &h.lifecycle,
                &h.queue,
            );
            // A queued large image is an unsent attachment: it counts once, as an
            // image (AC5); every other kind counts once, as a pending decision.
            let (decisions, images) = if kind == DecisionKind::LargeImage {
                (0, 1)
            } else {
                (1, 0)
            };
            assert_eq!(facts.pending_decisions, decisions, "{kind:?}");
            assert_eq!(facts.pending_images, images, "{kind:?}");
        }
    }

    #[test]
    fn story21_1_a_closed_conversations_decision_no_longer_blocks_quitting() {
        let mut h = host();
        decision_queue::raise(
            &mut h.state,
            DecisionDraft::new(DecisionKind::ContextWarning, "low").conversation("c1"),
            Arrival::Automatic,
        );
        let pending = |h: &Host| {
            local_facts(
                &h.state,
                &h.conversation,
                &h.tabs,
                &h.streaming,
                &h.lifecycle,
                &h.queue,
            )
            .pending_decisions
        };
        assert_eq!(pending(&h), 1, "positive control: the live item counts");
        // The tab holding c1 was closed: the host's draw prunes the orphan.
        let remaining = crate::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some("c2".into()),
            titles: vec![("c2".into(), "two".into())],
            ..Default::default()
        };
        decision_queue::prune_closed(&mut h.state, &remaining);
        assert_eq!(pending(&h), 0, "an orphaned item never makes quit ask");
        // Its frame is open: the orphan is stale (counted by the chip) until Close removes it.
        assert_eq!(
            decision_queue::status_cue(decision_queue::waiting(&h.state)).as_deref(),
            Some("Needs you (1) ^X q")
        );
        decision_queue::handle_key(&mut h.state, decision_queue::PaneKey::Esc, &remaining);
        assert_eq!(
            decision_queue::status_cue(decision_queue::waiting(&h.state)),
            None
        );
    }

    /// The shared pane boundary requests guarded quit; Ctrl+C backs without
    /// deciding. Keep the runtime-arm wiring check as well as real-key effects.
    #[test]
    fn story21_2_the_pane_ctrl_q_arm_uses_the_quit_guard() {
        use crate::adapters::tui::{
            app,
            presentation::{self, Surface},
        };
        use crate::domain::events::{DomainInputEvent, DomainKey};
        let source = include_str!("event_loop.rs");
        let arm = source
            .split_once("RouteEffect::QuitRequest =>")
            .unwrap()
            .1
            .split("RouteEffect::QuitConfirmed")
            .next()
            .unwrap();
        assert!(arm.contains("quit_guard::quit_from_banner("), "{arm}");
        assert!(!arm.contains("should_quit = true"), "{arm}");
        let mut h = host();
        decision_queue::raise(
            &mut h.state,
            DecisionDraft::new(DecisionKind::Budget, "waiting"),
            Arrival::Requested,
        );
        let pane = h.state.modal_stack.top().unwrap().clone();
        assert_eq!(
            app::handle_input(
                &mut h.state,
                &DomainInputEvent::SpecialKey(DomainKey::CtrlQ)
            ),
            app::InputAction::Quit
        );
        assert_eq!(h.state.modal_stack.top(), Some(&pane));
        assert!(!banner_quit(&mut h));
        assert!(presentation::is_open(&h.state, Surface::QuitConfirm));
        assert!(!h.state.should_quit);
        assert_eq!(
            handle_key(&mut h.state, QuitKey::Esc),
            QuitKeyOutcome::Cancelled
        );
        assert_eq!(h.state.modal_stack.top(), Some(&pane));
        assert_eq!(
            app::handle_input(
                &mut h.state,
                &DomainInputEvent::SpecialKey(DomainKey::CtrlC)
            ),
            app::InputAction::Consumed
        );
        assert!(!presentation::is_open(&h.state, Surface::Decision));
        assert!(h.state.decisions.holds(DecisionKind::Budget));
        assert!(!h.state.should_quit);
    }

    fn pending(h: &Host) -> usize {
        local_facts(
            &h.state,
            &h.conversation,
            &h.tabs,
            &h.streaming,
            &h.lifecycle,
            &h.queue,
        )
        .pending_decisions
    }

    /// One live item of a 6-C `kind`, number `n` (own conversation, plan, tool id, path),
    /// raised through its producer; reply receivers are kept alive in `keep`.
    fn raise_6c(
        state: &mut TuiState,
        kind: DecisionKind,
        n: usize,
        keep: &mut Vec<Box<dyn std::any::Any>>,
    ) {
        use crate::adapters::tui::decision_queue as dq;
        use crate::adapters::tui::fanout_spec::FanOutSpec;
        use crate::domain::models::AgentId;
        use crate::domain::models::plan::{PlanDeviationKind, PlanTaskStatus};
        use crate::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};
        let (conv, plan) = (format!("c{n}"), format!("p{n}"));
        let _ = match kind {
            DecisionKind::Question => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                keep.push(Box::new(rx));
                dq::raise_question(state, &conv, format!("tu-{n}"), "q?".into(), tx)
            }
            DecisionKind::SkillTrust => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                keep.push(Box::new(rx));
                dq::raise_model_skill_trust(
                    state,
                    format!("s{n}"),
                    format!("/w/s{n}.md").into(),
                    tx,
                )
            }
            DecisionKind::PlanApproval => dq::raise_plan_approval(
                state,
                dq::PlanApprovalPayload {
                    conversation_id: conv,
                    plan_path: format!("/w/{plan}.md").into(),
                    contents: "a".into(),
                    summary: "s".into(),
                },
            ),
            DecisionKind::PlanCard => {
                dq::raise_plan_card(state, plan_card(&plan, PlanStatus::Pending).0)
            }
            DecisionKind::PlanDeviation => dq::raise_plan_deviation(
                state,
                &conv,
                dq::PlanDeviationPayload {
                    plan_id: plan,
                    kind: PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
                    original: 3,
                    changed: vec![2],
                    summary: "s".into(),
                },
            ),
            DecisionKind::SkipCascade => dq::raise_skip_cascade(
                state,
                &conv,
                dq::SkipCascadePending {
                    plan_id: plan,
                    source_task: 1,
                    source_prior_status: PlanTaskStatus::Pending,
                    source_prior_error: None,
                    downstream: vec![2],
                },
            ),
            DecisionKind::CancelPlan => dq::raise_cancel_plan(
                state,
                &conv,
                dq::CancelPlanPayload {
                    plan_id: plan,
                    title: "t".into(),
                    pending: 1,
                    completed: 0,
                },
            ),
            DecisionKind::Delegation => dq::raise_delegation(
                state,
                dq::PendingDelegationCard {
                    conversation_id: conv,
                    plan_id: plan,
                    task_number: 1,
                    suggestion: DelegationSuggestion {
                        task_number: 1,
                        agent_name: "a".into(),
                        reason: DelegationReason::Heuristic,
                        auto_proceed: false,
                    },
                },
            ),
            DecisionKind::SpawnGate => dq::raise_spawn_gate(
                state,
                &conv,
                dq::PendingSpawnGate {
                    spec: FanOutSpec::Identical {
                        count: 5,
                        prompt: "go".into(),
                    },
                    requested: 5,
                    threshold: 2,
                    adjusted: None,
                },
            ),
            DecisionKind::AgentKill => {
                dq::raise_agent_kill(state, &conv, AgentId::new(), "a".into())
            }
            other => panic!("{other:?} is not a 6-C kind"),
        };
    }

    const KINDS_6C: [DecisionKind; 10] = [
        DecisionKind::Question,
        DecisionKind::SkillTrust,
        DecisionKind::PlanApproval,
        DecisionKind::PlanCard,
        DecisionKind::PlanDeviation,
        DecisionKind::SkipCascade,
        DecisionKind::CancelPlan,
        DecisionKind::Delegation,
        DecisionKind::SpawnGate,
        DecisionKind::AgentKill,
    ];

    /// The host's real fact builder, per 6-C kind: an empty queue counts nothing (the
    /// control), one live item counts once, two count twice, and quit then asks.
    #[test]
    fn story21_6_every_6c_kind_is_counted_per_live_item_by_the_host_facts() {
        assert_eq!(pending(&host()), 0, "control: an empty queue");
        assert_eq!(ask(&mut host()), QuitRequest::ExitNow);
        for kind in KINDS_6C {
            let mut h = host();
            let mut keep = Vec::new();
            raise_6c(&mut h.state, kind, 1, &mut keep);
            assert_eq!(pending(&h), 1, "{kind:?}: one");
            raise_6c(&mut h.state, kind, 2, &mut keep);
            assert_eq!(pending(&h), 2, "{kind:?}: two");
            assert_eq!(h.state.decisions.count(kind), 2, "{kind:?}: two queued");
            assert_eq!(ask(&mut h), QuitRequest::Confirming, "{kind:?}");
        }
    }

    /// A stale item can no longer be answered: it never makes quit ask (the live
    /// neighbour of the same kind still counts, so the exclusion is not an empty queue).
    #[test]
    fn story21_6_a_stale_6c_item_adds_nothing_to_the_host_facts() {
        use crate::adapters::tui::decision_queue as dq;
        let mut h = host();
        let mut keep = Vec::new();
        raise_6c(&mut h.state, DecisionKind::Question, 1, &mut keep);
        raise_6c(&mut h.state, DecisionKind::Question, 2, &mut keep);
        assert_eq!(pending(&h), 2);
        drop(keep.remove(0));
        assert!(dq::tick(&mut h.state), "the dead turn is swept");
        assert_eq!(pending(&h), 1, "the answered-by-nobody question");
        for kind in [
            DecisionKind::PlanCard,
            DecisionKind::PlanDeviation,
            DecisionKind::SkipCascade,
            DecisionKind::CancelPlan,
            DecisionKind::Delegation,
        ] {
            let mut h = host();
            let mut keep = Vec::new();
            raise_6c(&mut h.state, kind, 1, &mut keep);
            raise_6c(&mut h.state, kind, 2, &mut keep);
            dq::withdraw_plan(&mut h.state, "p1");
            // A Requested item that took the frame hides the withdrawn plan's item (removed,
            // not stale); whichever stays, the withdrawn plan's item adds nothing to the facts.
            assert!(
                !h.state.decisions.is_empty(),
                "{kind:?}: the other plan's item stays"
            );
            assert_eq!(pending(&h), 1, "{kind:?}: the withdrawn plan's item");
        }
        let mut h = host();
        let gone = crate::domain::models::AgentId::new();
        dq::raise_agent_kill(&mut h.state, "c1", gone.clone(), "a".into());
        dq::raise_agent_kill(
            &mut h.state,
            "c1",
            crate::domain::models::AgentId::new(),
            "b".into(),
        );
        dq::withdraw_agent_kill(&mut h.state, &gone);
        assert_eq!(pending(&h), 1, "the gone agent's kill confirmation");
    }
}
