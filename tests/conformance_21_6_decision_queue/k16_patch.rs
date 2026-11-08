//! 6-C patch round 1 (L1 findings 01..08): origin-keyed plan approval, per-conversation
//! question drainage and identity, independent approvals, skill-trust lifetime, and the
//! Copy preflight. Every test enters through the production producers and the router.

use std::path::PathBuf;

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, DecisionKind, DecisionSubject, PaneOutcome, PlanApprovalPayload, handle_domain_event,
};
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::{RouteEffect, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
use rustain::domain::events::DomainKey;
use rustain::domain::models::SkillTrustResponse;
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::support::{advance, clocked, facts, key, paint, ready, route, special};

fn approval(conversation: &str, path: &str) -> PlanApprovalPayload {
    PlanApprovalPayload {
        conversation_id: conversation.into(),
        plan_path: PathBuf::from(path),
        contents: format!("# {path}"),
        summary: "summary".into(),
    }
}

fn is_action(effect: &Option<RouteEffect>) -> bool {
    matches!(effect, Some(RouteEffect::Action(_)))
}

/// 6C-L1-01: a plan approval belongs to the tab that proposed it. From a read-only (or
/// any other) tab it can neither open nor be answered, and it is never lost.
#[test]
fn plan_approval_is_decidable_only_in_its_origin_tab() {
    let (mut state, clock) = clocked(80, 24);
    decision_queue::raise_plan_approval(&mut state, approval("c1", "plan.md"));
    // The user is in a read-only tab c2: the approval waits there, counted.
    paint(&mut state, &facts("c2"), 80, 24);
    ready(&mut state, &clock);
    assert!(decision_queue::current(&state, &facts("c2")).is_none());
    for c in ['y', 'a', 'n'] {
        assert!(
            !is_action(&route(&mut state, &key(c), &facts("c2"))),
            "{c} answers nothing from another tab"
        );
    }
    assert_eq!(state.decisions.count_live(DecisionKind::PlanApproval), 1);
    assert_eq!(decision_queue::waiting(&state), 1);
    // Control: back in its own tab it opens and answers exactly once.
    let item = state.decisions.items().next().expect("queued");
    let item_key = decision_queue::DecisionKey {
        kind: item.kind,
        source: item.source.clone(),
        conversation_id: item.conversation_id.clone(),
    };
    assert!(
        decision_queue::open(&mut state, &item_key),
        "opens in its own tab"
    );
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    assert!(is_action(&route(&mut state, &key('y'), &facts("c1"))));
    assert!(state.decisions.is_empty());
}

/// 6C-L1-04: answering one approval leaves every other one live, and the outcome
/// consumer (the `PlanApprovalResolved` arm) retires nothing of the kind.
#[test]
fn answering_one_of_three_approvals_leaves_the_others_live() {
    let (mut state, clock) = clocked(80, 24);
    for path in ["a.md", "b.md", "c.md"] {
        decision_queue::raise_plan_approval(&mut state, approval("c1", path));
    }
    assert_eq!(state.decisions.count_live(DecisionKind::PlanApproval), 3);
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    assert!(is_action(&route(&mut state, &key('n'), &facts("c1"))));
    assert_eq!(
        state.decisions.count_live(DecisionKind::PlanApproval),
        2,
        "the two unanswered approvals stay live"
    );
    // The consumer arm itself no longer withdraws the kind (it ran after every answer).
    let source = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/infrastructure/runtime/event_loop.rs"),
    )
    .unwrap();
    let start = source.find("AppEvent::PlanApprovalResolved {").unwrap();
    let end = start + source[start..].find("AppEvent::PlanProposed").unwrap();
    let arm = &source[start..end];
    assert!(
        !arm.contains("decision_queue::withdraw("),
        "the outcome consumer must not retire other approvals"
    );
}

type Held = oneshot::Receiver<String>;

fn ask(state: &mut TuiState, conversation: &str, tool: &str, question: &str) -> Held {
    let (tx, rx) = oneshot::channel::<String>();
    decision_queue::raise_question(state, conversation, tool.into(), question.into(), tx);
    rx
}

fn deliver(state: &mut TuiState, effect: Option<RouteEffect>) {
    let Some(RouteEffect::Action(action @ InputAction::Decision(_))) = effect else {
        panic!("a question answer is a decision effect, got {effect:?}")
    };
    decision_queue::apply_resolved_action(state, action, reducer::restore_focus_after_modal);
}

/// 6C-L1-03: the same tool use id in two conversations is two questions (the source
/// identity is namespaced by origin), each answered once into its own turn.
#[test]
fn the_same_tool_id_in_two_tabs_is_two_questions_answered_once_each() {
    let (mut state, clock) = clocked(80, 24);
    let mut rx1 = ask(&mut state, "c1", "tool-1", "first?");
    let mut rx2 = ask(&mut state, "c2", "tool-1", "second?");
    assert_eq!(state.decisions.count_live(DecisionKind::Question), 2);
    assert_eq!(rx1.try_recv(), Err(TryRecvError::Empty), "never cancelled");
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    for c in "A".chars() {
        route(&mut state, &key(c), &facts("c1"));
    }
    let sent = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    deliver(&mut state, sent);
    assert_eq!(rx1.try_recv(), Ok("A".to_owned()));
    assert_eq!(rx2.try_recv(), Err(TryRecvError::Empty));
    // The second question opens next with a field of its own.
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    route(&mut state, &key('B'), &facts("c1"));
    let sent = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    deliver(&mut state, sent);
    assert_eq!(rx2.try_recv(), Ok("B".to_owned()));
    assert!(state.decisions.is_empty());
}

/// 6C-L1-02: cancelling or rewinding one conversation abandons only its own questions.
#[test]
fn draining_one_conversation_leaves_other_tabs_questions_waiting() {
    for origin in ["c1", "c2"] {
        let other = if origin == "c1" { "c2" } else { "c1" };
        let (mut state, _clock) = clocked(80, 24);
        let mut mine = ask(&mut state, origin, "tool-1", "mine?");
        let mut theirs = ask(&mut state, other, "tool-1", "theirs?");
        decision_queue::drain_questions(&mut state, "c3");
        assert_eq!(mine.try_recv(), Err(TryRecvError::Empty), "{origin}");
        assert_eq!(theirs.try_recv(), Err(TryRecvError::Empty), "{origin}");
        decision_queue::drain_questions(&mut state, origin);
        assert_eq!(mine.try_recv(), Err(TryRecvError::Closed), "{origin}");
        assert_eq!(
            theirs.try_recv(),
            Err(TryRecvError::Empty),
            "{other}'s turn keeps waiting"
        );
        let live: Vec<String> = state
            .decisions
            .items()
            .filter(|item| !item.is_stale())
            .filter_map(|item| match &item.subject {
                DecisionSubject::Question(payload) => Some(payload.origin_conversation.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(live, vec![other.to_owned()], "{origin}");
    }
}

/// 6C-L1-06: a model prompt whose turn receiver is gone is swept.
#[test]
fn a_model_skill_prompt_with_a_dropped_receiver_is_swept() {
    for dropped in [true, false] {
        let (mut state, _clock) = clocked(80, 24);
        let (tx, rx) = oneshot::channel::<SkillTrustResponse>();
        decision_queue::raise_model_skill_trust(
            &mut state,
            "review".into(),
            PathBuf::from("/w/SKILL.md"),
            tx,
        );
        if dropped {
            drop(rx);
        }
        decision_queue::prune_closed(&mut state, &facts("c1"));
        assert_eq!(
            state.decisions.count_live(DecisionKind::SkillTrust),
            usize::from(!dropped),
            "dropped={dropped}"
        );
    }
}

fn facts_with_tabs(active: &str, tabs: &[&str]) -> LogFacts {
    LogFacts {
        titles: tabs.iter().map(|t| ((*t).into(), (*t).into())).collect(),
        ..facts(active)
    }
}

/// 6C-L1-06: a deferred user-driven prompt whose origin tab closed goes stale (grants
/// nothing) and no longer blocks the next user-driven request.
#[test]
fn a_user_skill_prompt_of_a_closed_tab_is_stale_and_does_not_block_a_new_request() {
    let (mut state, clock) = clocked(80, 24);
    assert!(
        decision_queue::raise_user_skill_trust(
            &mut state,
            "c1",
            "one".into(),
            PathBuf::from("/w/one/SKILL.md"),
            String::new(),
        )
        .is_some()
    );
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    // The same request while the first is live is still the legacy drop.
    assert!(
        decision_queue::raise_user_skill_trust(
            &mut state,
            "c1",
            "two".into(),
            PathBuf::from("/w/two/SKILL.md"),
            String::new(),
        )
        .is_none()
    );
    // The origin tab closes; the user is in c2.
    let after = facts_with_tabs("c2", &["c2"]);
    decision_queue::prune_closed(&mut state, &after);
    assert_eq!(state.decisions.count_live(DecisionKind::SkillTrust), 0);
    advance(&clock, decision_queue::INERT_AFTER_PAINT);
    // No trust is granted for the obsolete item, whatever the user presses.
    for c in ['y', 'n'] {
        assert!(!is_action(&route(&mut state, &key(c), &after)), "{c}");
    }
    assert!(
        decision_queue::raise_user_skill_trust(
            &mut state,
            "c2",
            "two".into(),
            PathBuf::from("/w/two/SKILL.md"),
            String::new(),
        )
        .is_some(),
        "the new slash request is accepted"
    );
    assert_eq!(state.decisions.count_live(DecisionKind::SkillTrust), 1);
}

/// 6C-L1-08: the PlanApproval Copy preflight mirrors the copied text (contents alone).
#[test]
fn plan_approval_copy_at_the_limit_succeeds_with_a_summary_and_one_over_refuses() {
    for (extra, copies) in [(0usize, true), (1, false)] {
        let (mut state, clock) = clocked(80, 24);
        let mut payload = approval("c1", "plan.md");
        payload.contents = "a".repeat(COPY_MAX_BYTES + extra);
        payload.summary = "a non-empty summary".into();
        decision_queue::raise_plan_approval(&mut state, payload);
        decision_queue::note_painted(&mut state);
        advance(&clock, decision_queue::INERT_AFTER_PAINT);
        let outcome = handle_domain_event(&mut state, &key('c'), &facts("c1"), Host::Local);
        if copies {
            match outcome {
                Some(PaneOutcome::Copy(text)) => assert_eq!(text.len(), COPY_MAX_BYTES),
                other => panic!("exactly COPY_MAX_BYTES copies, got {other:?}"),
            }
        } else {
            assert!(!matches!(outcome, Some(PaneOutcome::Copy(_))));
        }
    }
}
