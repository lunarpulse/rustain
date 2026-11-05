//! K11 — decision surfaces through the local front door (Story 21.3 AC15-AC17, A8).
//! Every card is built the way its producer leaves it (slot set, focus as the event
//! loop sets it) and driven by real key events. The attached twins live in
//! `attach_keys.rs` / `attach_loop.rs` (`story21_3_attached_*`).
//!
//! * `every_card_key_decides_once_by_value` is the by-value record of every card key
//!   (green on the hand-written arms before 3-C and after it).
//! * `esc_and_ctrl_c_never_decide_a_card` — `Esc` / `Ctrl+C` decide nothing, post ONE
//!   cue generated from the registry and leave the card pending and answerable.
//! * `typing_in_input_never_answers_a_card` — with any inline card pending, the keys
//!   typed in Input are text.
//! * `a_card_swallows_every_key_it_has_no_row_for` — the isolation sweep.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::fanout_spec::FanOutSpec;
use rustain::adapters::tui::keymap::render::card_cue;
use rustain::adapters::tui::keymap::{Context, Host, Key};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::state::{
    PendingConsolidationCard, PendingDelegationCard, PendingForgetCard, PendingPlanApproval,
    PendingPlanCard, PendingSpawnGate, SkillTrustState, SkipCascadeChoice, SkipCascadePending,
    TuiState,
};
use rustain::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
use rustain::domain::models::FocusState;
use rustain::domain::models::plan::{
    Plan, PlanDecision, PlanDeviationKind, PlanStatus, PlanTaskStatus,
};
use rustain::domain::models::visual::{ConfirmationType, OverlayType};
use rustain::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};

use std::sync::Arc;

use rustain::adapters::tui::state::{FeedbackInputState, PendingPermission};
use rustain::domain::models::ToolRisk;
use rustain::domain::models::tool_call::{ApprovalSource, RequestId};

use super::support::{fresh, is_inert, press};
use crate::conformance_21_11_display_width::k5b_cards::{
    apply_pending, peer_pending, retract_pending,
};

fn confirm(card: ConfirmationType) -> FocusState {
    FocusState::Overlay(OverlayType::Confirmation(card))
}

fn chat() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state
}

fn overlay(card: ConfirmationType) -> TuiState {
    let mut state = fresh();
    state.focus = confirm(card);
    state
}

/// One card: how it is built, whether it is still pending, the context that owns its
/// keys, every key it answers (by value) and whether it consumes every other key.
struct Card {
    name: &'static str,
    context: Context,
    build: fn() -> TuiState,
    pending: fn(&TuiState) -> bool,
    rows: Vec<(Key, InputAction)>,
    /// Focus `Chat` cards that swallow every character (consolidation, forget, deviation,
    /// cancel-plan, skip-cascade); the plan card and delegation only claim their own keys.
    swallows_chars: bool,
    /// Overlay-focus cards consume every key without a row.
    overlay: bool,
}

fn plan() -> Plan {
    Plan {
        id: "p1".into(),
        title: "t".into(),
        tasks: vec![],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: None,
    }
}

fn ch(c: char) -> Key {
    Key::Char(c)
}

fn cards() -> Vec<Card> {
    let yn = |yes: InputAction, no: InputAction| vec![(ch('y'), yes), (ch('n'), no)];
    vec![
        Card {
            name: "skill trust",
            context: Context::SkillTrust,
            build: || {
                let mut s = overlay(ConfirmationType::SkillTrust);
                s.pending_skill_trust = Some(SkillTrustState {
                    skill_name: "s".into(),
                    skill_file: "s.md".into(),
                    response_tx: tokio::sync::oneshot::channel().0,
                    inspect_content: None,
                });
                s
            },
            pending: |s| s.pending_skill_trust.is_some(),
            rows: vec![
                (ch('y'), InputAction::SkillTrustAccept),
                (ch('n'), InputAction::SkillTrustDecline),
                (ch('i'), InputAction::SkillTrustInspect),
                (ch('j'), InputAction::ScrollLineDown),
                (ch('k'), InputAction::ScrollLineUp),
            ],
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "plan approval",
            context: Context::PlanApproval,
            build: || {
                let mut s = overlay(ConfirmationType::PlanApproval);
                s.pending_plan_approval = Some(PendingPlanApproval {
                    conversation_id: "c1".into(),
                    plan_path: "plan.md".into(),
                    contents: String::new(),
                    summary: String::new(),
                });
                s
            },
            pending: |s| s.pending_plan_approval.is_some(),
            rows: vec![
                (ch('y'), InputAction::PlanApproveNormal),
                (ch('a'), InputAction::PlanApproveAutoEdit),
                (ch('n'), InputAction::PlanReject),
                (ch('e'), InputAction::PlanRevise),
            ],
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "plan card",
            context: Context::PlanDecision,
            build: || {
                let mut s = chat();
                s.pending_plan_card = Some(PendingPlanCard {
                    conversation_id: "c1".into(),
                    plan_id: "p1".into(),
                    plan_snapshot: plan(),
                });
                s
            },
            pending: |s| s.pending_plan_card.is_some(),
            rows: vec![
                (ch('y'), InputAction::PlanCardApprove),
                (ch('e'), InputAction::PlanCardEdit),
                (ch('n'), InputAction::PlanCardReject),
            ],
            swallows_chars: false,
            overlay: false,
        },
        Card {
            name: "plan deviation",
            context: Context::PlanDecision,
            build: || {
                let mut s = chat();
                s.task_panel_state.pending_deviation = Some((
                    "p1".into(),
                    PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
                ));
                s
            },
            pending: |s| s.task_panel_state.pending_deviation.is_some(),
            rows: vec![
                (
                    ch('y'),
                    InputAction::PlanDeviationDecided("p1".into(), PlanDecision::Approve),
                ),
                (
                    ch('e'),
                    InputAction::PlanDeviationDecided("p1".into(), PlanDecision::Edit),
                ),
                (
                    ch('n'),
                    InputAction::PlanDeviationDecided("p1".into(), PlanDecision::Reject),
                ),
            ],
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "cancel plan",
            context: Context::Confirm,
            build: || {
                let mut s = chat();
                s.task_panel_state.cancel_plan_confirm = Some("p1".into());
                s
            },
            pending: |s| s.task_panel_state.cancel_plan_confirm.is_some(),
            rows: yn(
                InputAction::CancelPlanConfirm(true),
                InputAction::CancelPlanConfirm(false),
            ),
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "skip cascade",
            context: Context::SkipCascade,
            build: || {
                let mut s = chat();
                s.task_panel_state.skip_cascade_pending = Some(SkipCascadePending {
                    plan_id: "p1".into(),
                    source_task: 1,
                    source_prior_status: PlanTaskStatus::Pending,
                    source_prior_error: None,
                    downstream: vec![2],
                });
                s
            },
            pending: |s| s.task_panel_state.skip_cascade_pending.is_some(),
            rows: vec![
                (
                    ch('s'),
                    InputAction::SkipCascadeAck(SkipCascadeChoice::CascadeSkip),
                ),
                (
                    ch('c'),
                    InputAction::SkipCascadeAck(SkipCascadeChoice::ContinueAnyway),
                ),
                (
                    ch('n'),
                    InputAction::SkipCascadeAck(SkipCascadeChoice::CancelSkip),
                ),
            ],
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "delegation",
            context: Context::Delegation,
            build: || {
                let mut s = chat();
                s.pending_delegation_card = Some(PendingDelegationCard {
                    conversation_id: "c1".into(),
                    plan_id: "p1".into(),
                    task_number: 1,
                    suggestion: DelegationSuggestion {
                        task_number: 1,
                        agent_name: "reviewer".into(),
                        reason: DelegationReason::ExplicitAgentMention,
                        auto_proceed: false,
                    },
                });
                s
            },
            pending: |s| s.pending_delegation_card.is_some(),
            rows: vec![
                (ch('d'), InputAction::DelegateTask),
                (ch('l'), InputAction::RunTaskLocally),
                (ch('x'), InputAction::DelegationCardCancel),
            ],
            swallows_chars: false,
            overlay: false,
        },
        Card {
            name: "spawn gate",
            context: Context::SpawnGate,
            build: || {
                let mut s = chat();
                s.pending_spawn_gate = Some(PendingSpawnGate {
                    spec: FanOutSpec::Identical {
                        count: 3,
                        prompt: "p".into(),
                    },
                    requested: 3,
                    threshold: 2,
                    adjusted: None,
                });
                s
            },
            pending: |s| s.pending_spawn_gate.is_some(),
            rows: vec![
                (ch('c'), InputAction::SpawnGateCap),
                (ch('n'), InputAction::CloseWaveOverlay),
                (Key::Enter, InputAction::SpawnGateConfirm),
                (Key::Left, InputAction::SpawnGateAdjustLeft),
                (Key::Right, InputAction::SpawnGateAdjustRight),
            ],
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "consolidation",
            context: Context::Confirm,
            build: || {
                let mut s = chat();
                s.pending_consolidation_card = Some(PendingConsolidationCard {
                    conversation_id: "c1".into(),
                    proposals: vec![],
                });
                s
            },
            pending: |s| s.pending_consolidation_card.is_some(),
            rows: yn(
                InputAction::ConsolidateAcceptAll,
                InputAction::ConsolidateDeclineAll,
            ),
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "forget",
            context: Context::Forget,
            build: || {
                let mut s = chat();
                s.pending_forget_card = Some(PendingForgetCard {
                    conversation_id: "c1".into(),
                    candidates: vec![],
                    focused_index: 0,
                });
                s
            },
            pending: |s| s.pending_forget_card.is_some(),
            rows: vec![
                (ch('y'), InputAction::ForgetAcceptAll),
                (ch('n'), InputAction::ForgetDeclineAll),
                (ch(' '), InputAction::ForgetToggleSelection),
                (ch('j'), InputAction::ForgetNavigateDown),
                (ch('k'), InputAction::ForgetNavigateUp),
            ],
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "artifact apply",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::ArtifactApply);
                s.pending_artifact_card = Some(apply_pending(vec!["a.rs".into()], false));
                s
            },
            pending: |s| s.pending_artifact_card.is_some(),
            rows: yn(InputAction::ApplyCardAccept, InputAction::ApplyCardDecline),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "peer add",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::PeerAdd);
                s.pending_peer_add = Some(peer_pending("jun", None));
                s
            },
            pending: |s| s.pending_peer_add.is_some(),
            rows: yn(InputAction::PeerAddConfirm, InputAction::PeerAddDecline),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "team retract",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::TeamRetract);
                s.pending_team_retract = Some(retract_pending("card"));
                s
            },
            pending: |s| s.pending_team_retract.is_some(),
            rows: yn(
                InputAction::TeamRetractConfirm,
                InputAction::TeamRetractDecline,
            ),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "fork",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::Fork);
                s.pending_fork_index = Some(0);
                s
            },
            pending: |s| s.pending_fork_index.is_some(),
            rows: yn(InputAction::ForkConfirm, InputAction::ForkCancel),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "rewind",
            context: Context::Rewind,
            build: || {
                let mut s = overlay(ConfirmationType::Rewind);
                s.pending_rewind_index = Some(0);
                s
            },
            pending: |s| s.pending_rewind_index.is_some(),
            rows: vec![
                (ch('y'), InputAction::RewindConfirm),
                (ch('f'), InputAction::RewindForkInstead),
                (ch('n'), InputAction::RewindCancel),
            ],
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "export overwrite",
            context: Context::ExportOverwrite,
            build: || {
                let mut s = overlay(ConfirmationType::ExportOverwrite("out.md".into()));
                s.pending_export = Some(("out.md".into(), String::new()));
                s
            },
            pending: |s| {
                matches!(
                    s.focus,
                    FocusState::Overlay(OverlayType::Confirmation(
                        ConfirmationType::ExportOverwrite(_)
                    ))
                )
            },
            rows: vec![
                (ch('y'), InputAction::ConfirmExportOverwrite),
                (ch('Y'), InputAction::ConfirmExportOverwrite),
                (ch('n'), InputAction::CancelExportOverwrite),
                (ch('N'), InputAction::CancelExportOverwrite),
            ],
            swallows_chars: true,
            overlay: true,
        },
    ]
}

fn cue_records(state: &TuiState) -> Vec<String> {
    state
        .notifications
        .records()
        .filter(|r| r.severity == NoticeSeverity::Info)
        .map(|r| r.message.clone())
        .collect()
}

fn mixed_card(top: &str, context: Context, key: Key, action: InputAction) {
    let fixtures = cards();
    let mut state = (fixtures.iter().find(|c| c.name == top).unwrap().build)();
    state.task_panel_state.pending_deviation = Some((
        "p1".into(),
        PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
    ));
    use rustain::adapters::tui::widgets::{consolidation_card, delegation_card};
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            if let Some(card) = &state.pending_consolidation_card {
                let lines =
                    consolidation_card::render_consolidation_card_lines(card, &state.theme, 80);
                frame.render_widget(ratatui::widgets::Paragraph::new(lines), frame.area());
            } else {
                let card = state.pending_delegation_card.as_ref().unwrap();
                delegation_card::render(frame.area(), frame.buffer_mut(), card, &state.theme);
            }
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    for visible in if top == "consolidation" {
        ["Consolidate memory", "[y] promote all", "[n] decline"]
    } else {
        ["Delegate Task", "[d] Delegate", "[x] Cancel plan"]
    } {
        assert!(screen.contains(visible), "{top}: {screen}");
    }
    assert!(is_inert(&press(&mut state, Key::Esc)));
    assert_eq!(
        cue_records(&state),
        vec![card_cue(context, Host::Local)],
        "{top}"
    );
    assert_eq!(press(&mut state, key), action, "{top}");
}

#[test]
fn consolidation_and_deviation_paint_cue_and_decide_the_same_card() {
    mixed_card(
        "consolidation",
        Context::Confirm,
        ch('y'),
        InputAction::ConsolidateAcceptAll,
    );
}

#[test]
fn delegation_and_deviation_paint_cue_and_decide_the_same_card() {
    mixed_card(
        "delegation",
        Context::Delegation,
        ch('d'),
        InputAction::DelegateTask,
    );
}

#[test]
fn drafted_inline_cards_that_swallow_i_preserve_the_draft_until_a_decision() {
    for card in cards().into_iter().filter(|c| {
        [
            "spawn gate",
            "consolidation",
            "forget",
            "plan deviation",
            "cancel plan",
            "skip cascade",
        ]
        .contains(&c.name)
    }) {
        let mut state = (card.build)();
        state.input_buffer = "preserved draft".into();
        let action = press(&mut state, ch('i'));
        assert_eq!(
            action,
            if ["spawn gate", "consolidation", "forget"].contains(&card.name) {
                InputAction::Consumed
            } else {
                InputAction::Ignored
            }
        );
        assert_eq!(state.focus, FocusState::Chat);
        assert_eq!(state.input_buffer, "preserved draft");
        assert!((card.pending)(&state));
    }
}

#[test]
fn every_card_key_decides_once_by_value() {
    for card in cards() {
        for (key, want) in &card.rows {
            let mut state = (card.build)();
            assert_eq!(press(&mut state, *key), *want, "{}: {key:?}", card.name);
        }
    }
}

#[test]
fn esc_and_ctrl_c_never_decide_a_card() {
    for card in cards() {
        let want_cue = card_cue(card.context, Host::Local);
        for key in [Key::Esc, Key::Ctrl('c')] {
            let mut state = (card.build)();
            let focus = state.focus.clone();
            let action = press(&mut state, key);
            assert!(
                is_inert(&action),
                "{}: {key:?} returned {action:?}",
                card.name
            );
            assert!(
                (card.pending)(&state),
                "{}: {key:?} resolved the card",
                card.name
            );
            assert_eq!(state.focus, focus, "{}: {key:?} moved the focus", card.name);
            assert_eq!(
                cue_records(&state),
                vec![want_cue.clone()],
                "{}: {key:?} cue",
                card.name
            );
            // Positive control: the explicit key still decides, once.
            let (first, want) = &card.rows[0];
            assert_eq!(
                press(&mut state, *first),
                *want,
                "{}: answerable after {key:?}",
                card.name
            );
        }
    }
}

#[test]
fn typing_in_input_never_answers_a_card() {
    for card in cards().iter().filter(|c| !c.overlay) {
        for (key, answer) in &card.rows {
            let Key::Char(c) = key else { continue };
            let mut state = (card.build)();
            state.focus = FocusState::Input;
            let action = press(&mut state, *key);
            assert_ne!(&action, answer, "{}: {c:?} answered from Input", card.name);
            assert_eq!(
                state.input_buffer,
                c.to_string(),
                "{}: {c:?} is text",
                card.name
            );
            assert!(
                (card.pending)(&state),
                "{}: {c:?} resolved the card",
                card.name
            );
        }
    }
}

#[test]
fn a_card_swallows_every_key_it_has_no_row_for() {
    for card in cards() {
        for key in Key::universe() {
            let own = card.rows.iter().any(|(k, _)| *k == key);
            let reserved = matches!(key, Key::Esc | Key::Ctrl('c') | Key::Ctrl('q'));
            let chars_only = !card.overlay && !matches!(key, Key::Char(_));
            if own || reserved || chars_only || (!card.swallows_chars && !card.overlay) {
                continue;
            }
            let mut state = (card.build)();
            let focus = state.focus.clone();
            let action = press(&mut state, key);
            assert!(
                is_inert(&action),
                "{}: {key:?} returned {action:?}",
                card.name
            );
            assert!(
                (card.pending)(&state),
                "{}: {key:?} resolved the card",
                card.name
            );
            assert_eq!(state.focus, focus, "{}: {key:?} moved the focus", card.name);
            assert_eq!(state.input_buffer, "", "{}: {key:?} typed", card.name);
        }
    }
}

/// `Ctrl+D` skips the question (its old `Esc` effect, explicit); `Esc` is a cue; the answer
/// field still types and `Enter` still sends.
#[test]
fn ctrl_d_skips_the_question_and_esc_never_does() {
    let build = || {
        let mut s = overlay(ConfirmationType::Question);
        s.ask_user_question = Some(AskUserQuestionState {
            tool_use_id: "t".into(),
            question: "q?".into(),
            input_buffer: String::new(),
            cursor_position: 0,
            submitted_answer: None,
        });
        s
    };
    let mut state = build();
    for key in [Key::Esc, Key::Ctrl('c')] {
        assert!(is_inert(&press(&mut state, key)));
        assert!(state.ask_user_question.is_some(), "{key:?} must not skip");
    }
    assert_eq!(
        cue_records(&state),
        vec![card_cue(Context::Question, Host::Local)]
    );
    assert_eq!(press(&mut state, ch('h')), InputAction::Consumed);
    assert_eq!(state.ask_user_question.as_ref().unwrap().input_buffer, "h");
    assert_eq!(
        press(&mut state, Key::Enter),
        InputAction::SubmitQuestionAnswer("h".into())
    );
    let mut skip = build();
    assert_eq!(press(&mut skip, Key::Ctrl('d')), InputAction::Consumed);
    assert!(skip.ask_user_question.is_none(), "Ctrl+D skips");
    assert_eq!(skip.focus, FocusState::Input);
}

/// Sub-frame backs stay backs (LR-3): the skill inspect view returns to the trust card.
#[test]
fn sub_frame_backs_stay_backs() {
    let skill_trust = cards()
        .into_iter()
        .find(|c| c.name == "skill trust")
        .unwrap();
    let mut state = (skill_trust.build)();
    state.focus = confirm(ConfirmationType::SkillTrustInspect);
    state.skill_trust_inspect_mode = true;
    assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
    assert_eq!(state.focus, confirm(ConfirmationType::SkillTrust));
    assert!(cue_records(&state).is_empty(), "a back posts no cue");
}

/// The forget card's arrows move its focus.
#[test]
fn forget_arrows_move_the_focus() {
    let forget = cards().into_iter().find(|c| c.name == "forget").unwrap();
    let mut state = (forget.build)();
    assert_eq!(press(&mut state, Key::Down), InputAction::Consumed);
    assert_eq!(press(&mut state, Key::Up), InputAction::Consumed);
}

/// The peer-add card body is shared with the CLI (which may not import the keymap), so its
/// `[y]` / `[n]` are hand-written there: they must still be the `Confirm` rows.
#[test]
fn the_shared_peer_add_card_copy_names_the_confirm_rows() {
    use rustain::adapters::tui::keymap::render::rail;
    use rustain::adapters::tui::keymap::table_card::CardCmd;
    let card = peer_pending("jun", None).card;
    let line = format!(
        "{} Confirm and pin  {} Cancel",
        rail(Context::Confirm, CardCmd::Yes),
        rail(Context::Confirm, CardCmd::No)
    );
    assert!(card.contains(&line), "{card}");
}

/// R7: a toast clips at 80 columns, so the cue names the card's negative key within the
/// first 74 cells of its body (the prefix is 45 cells): `[n] Deny` on the permission card.
#[test]
fn every_cue_names_its_negative_key_inside_the_toast_budget() {
    use rustain::adapters::tui::keymap::render::{CUE_BUDGET_CELLS, is_negative};
    use rustain::adapters::tui::keymap::resolve::rows_of;
    use rustain::adapters::tui::keymap::table_card;
    use rustain::adapters::tui::text_geometry::cells;
    assert_eq!(CUE_BUDGET_CELLS, 74, "80 columns minus the notice prefix");
    let mut contexts: Vec<Context> = vec![Context::Permission];
    for row in table_card::ROWS {
        contexts.push(row.context);
    }
    contexts.dedup();
    // The shared parent and the two sub-frames answer `Esc` with a back, never a cue.
    let cued = |context: &&Context| {
        !matches!(
            **context,
            Context::Card | Context::Feedback | Context::SkillTrustInspect
        )
    };
    let mut checked = 0;
    for context in contexts.iter().filter(cued) {
        for host in [Host::Local, Host::Attached] {
            let labelled: Vec<_> = rows_of(*context, host)
                .filter(|row| !row.label.is_empty())
                .collect();
            let cue = card_cue(*context, host);
            for row in labelled.iter().filter(|row| is_negative(row.command)) {
                let item = format!("[{}] {}", row.long_label(), row.label);
                let at = cue.find(&item).expect("the cue names its negative key");
                let end = cells(&cue[..at + item.len()]);
                assert!(
                    end <= CUE_BUDGET_CELLS,
                    "{context:?}/{host:?}: `{item}` ends at cell {end}: {cue}"
                );
                checked += 1;
            }
            assert!(
                labelled.is_empty() || labelled.iter().any(|row| is_negative(row.command)),
                "{context:?}/{host:?} has no negative key: {cue}"
            );
        }
    }
    assert!(checked >= 14, "{checked} negative keys checked");
    let permission = card_cue(Context::Permission, Host::Local);
    assert!(
        permission.contains("[y] Allow  [n] Deny"),
        "the deny key follows the allow key: {permission}"
    );
}

/// The feedback field sends, erases and backs out: a back is no decision and no cue.
#[test]
fn the_feedback_field_types_erases_sends_and_backs_out() {
    let mut state = overlay(ConfirmationType::PermissionFeedback);
    state.pending_feedback_input = Some(FeedbackInputState {
        buffer: String::new(),
        cursor: 0,
        pending_permission: PendingPermission {
            id: RequestId::new(),
            source: ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            tool_name: "Bash".to_string(),
            input: Arc::new(serde_json::json!({"command": "ls"})),
            risk: ToolRisk::Elevated,
        },
    });
    assert_eq!(
        press(&mut state, ch('h')),
        InputAction::FeedbackInputChar('h')
    );
    assert_eq!(
        press(&mut state, Key::Backspace),
        InputAction::FeedbackInputBackspace
    );
    assert_eq!(
        press(&mut state, Key::Enter),
        InputAction::FeedbackInputSubmit
    );
    assert_eq!(
        press(&mut state, Key::Esc),
        InputAction::FeedbackInputCancel
    );
    assert!(cue_records(&state).is_empty(), "a back posts no cue");
}

/// The question answer erases its last character.
#[test]
fn the_question_answer_erases_its_last_character() {
    let mut state = overlay(ConfirmationType::Question);
    state.ask_user_question = Some(AskUserQuestionState {
        tool_use_id: "t".into(),
        question: "q?".into(),
        input_buffer: "hi".into(),
        cursor_position: 2,
        submitted_answer: None,
    });
    assert_eq!(press(&mut state, Key::Backspace), InputAction::Consumed);
    assert_eq!(state.ask_user_question.as_ref().unwrap().input_buffer, "h");
}

/// A drill-down or a reorder in Chat goes back on `Esc` before the card cues (`back_first`):
/// the card stays pending and no cue is posted by that press.
#[test]
fn esc_goes_back_from_a_drill_down_or_reorder_before_it_cues() {
    let card = cards()
        .into_iter()
        .find(|c| c.name == "delegation")
        .unwrap();
    let mut drill = (card.build)();
    drill.task_panel_state.drill_down_task = Some(3);
    let mut reorder = (card.build)();
    reorder.task_panel_state.reorder_mode_for = Some(2);
    for (what, mut state, back) in [
        ("drill-down", drill, InputAction::Consumed),
        ("reorder", reorder, InputAction::TaskReorderCancel),
    ] {
        assert_eq!(press(&mut state, Key::Esc), back, "{what}: Esc goes back");
        assert!((card.pending)(&state), "{what}: the card is still pending");
        assert!(
            cue_records(&state).is_empty(),
            "{what}: a back posts no cue"
        );
    }
}

/// Every local row of `table_card` has a by-value witness: a `cards()` row, or one of the
/// named tests below. A row added to the table without one fails here. (`TeamProposal` is
/// witnessed in `app.rs` under `a2a` and `Consent` in the attached tests.)
#[test]
fn every_table_card_row_has_a_witness() {
    use rustain::adapters::tui::keymap::table_card;
    let mut witnessed: Vec<(Context, Key)> = Vec::new();
    for card in cards() {
        witnessed.extend(card.rows.iter().map(|(key, _)| (card.context, *key)));
    }
    witnessed.extend([
        // `esc_and_ctrl_c_never_decide_a_card`, once per card.
        (Context::Card, Key::Esc),
        (Context::Card, Key::Ctrl('c')),
        // `the_feedback_field_types_erases_sends_and_backs_out`.
        (Context::Feedback, Key::Enter),
        (Context::Feedback, Key::Esc),
        (Context::Feedback, Key::Backspace),
        // `ctrl_d_skips_the_question_and_esc_never_does`, `the_question_answer_erases_...`.
        (Context::Question, Key::Enter),
        (Context::Question, Key::Ctrl('d')),
        (Context::Question, Key::Backspace),
        // `sub_frame_backs_stay_backs`, `forget_arrows_move_the_focus`.
        (Context::SkillTrustInspect, Key::Esc),
        (Context::Forget, Key::Down),
        (Context::Forget, Key::Up),
    ]);
    let unwitnessed: Vec<String> = table_card::ROWS
        .iter()
        .filter(|row| row.hosts.includes(Host::Local))
        .filter(|row| !matches!(row.context, Context::TeamProposal | Context::Consent))
        .filter(|row| !witnessed.contains(&(row.context, row.keys[0])))
        .map(|row| format!("{:?} {:?}", row.context, row.keys))
        .collect();
    assert!(
        unwitnessed.is_empty(),
        "rows without a witness: {unwitnessed:?}"
    );
}

/// R5: two literals the keymap cannot generate (the domain layer may not import the adapter)
/// are pinned to their registry rows.
#[test]
fn domain_literals_name_the_registry_keys() {
    use rustain::adapters::tui::keymap::render::rail;
    use rustain::adapters::tui::keymap::table_card::CardCmd;
    use rustain::adapters::tui::keymap::table_permission::PermissionCmd;
    use rustain::domain::models::team_policy::IndividualPolicy;
    use rustain::domain::services::approval_presentation::MORE_MARKER;
    use rustain::domain::services::team_policy::{
        explain_effective_policy, resolve_effective_policy,
    };
    assert!(
        MORE_MARKER.ends_with(&format!(
            "{} Review first",
            rail(Context::Permission, PermissionCmd::Review)
        )),
        "{MORE_MARKER}"
    );
    let effective = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
    let explanation = explain_effective_policy(&effective, &[]);
    let guidance = explanation
        .rows
        .iter()
        .find(|row| row.key == "consent:none")
        .and_then(|row| row.guidance.clone())
        .expect("the empty consent projection explains itself");
    let always = format!("({} Always allow)", rail(Context::Consent, CardCmd::Always));
    assert!(guidance.contains(&always), "{guidance}");
}
