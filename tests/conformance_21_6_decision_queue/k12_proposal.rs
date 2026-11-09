//! K12: proposal frames retain their origin, draft fence, and Review sub-state.
use std::sync::Arc;

use super::support::{clocked, facts, key, paint, ready, route, screen, special};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, DecisionEffect, DecisionKind, DecisionSubject, Responder,
};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface};
use rustain::domain::events::DomainKey;
use rustain::domain::models::{SemanticMessageType, TeamProposal};

fn proposal() -> Arc<TeamProposal> {
    Arc::new(TeamProposal {
        id: "p1".into(),
        conversation_id: "c1".into(),
        recipients: vec!["lena".into(), "jun".into()],
        message_type: SemanticMessageType::ArchitectureUpdate,
        body: "first\nsecond\nthird\nfourth\nfifth".into(),
    })
}

#[test]
fn proposal_is_actionable_only_in_its_own_tab() {
    let (mut state, clock) = clocked(80, 24);
    decision_queue::prune_closed(&mut state, &facts("c2"));
    decision_queue::raise_team_proposal(&mut state, proposal(), Responder::Local);
    if presentation::is_open(&state, Surface::Decision) {
        let _ = route(&mut state, &special(DomainKey::Esc), &facts("c2"));
    }
    decision_queue::open_list(&mut state, &facts("c2"));
    let text = screen(&paint(&mut state, &facts("c2"), 80, 24));
    assert!(
        text.contains("Open ") && text.contains("to decide"),
        "{text}"
    );
    let action = route(&mut state, &special(DomainKey::Enter), &facts("c2"));
    let Some(RouteEffect::Action(InputAction::OpenDecisionTab {
        conversation_id,
        key: decision_key,
    })) = action
    else {
        panic!("other-tab proposal must request a tab switch, got {action:?}");
    };
    assert_eq!(conversation_id, "c1");
    assert_eq!(decision_key.kind, DecisionKind::TeamProposal);
    assert!(state.decisions.take_resolved().is_none());
    decision_queue::prune_closed(&mut state, &facts("c1"));
    presentation::reducer::close(&mut state, Surface::DecisionQueue);
    assert!(decision_queue::open(&mut state, &decision_key));
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    assert_eq!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::TeamProposalSend
        )))
    );
    assert!(matches!(
        state.decisions.take_resolved(),
        Some(DecisionSubject::TeamProposal(_))
    ));
    assert!(state.decisions.take_resolved().is_none());
}

#[test]
fn composer_blocks_decisions_and_review_stays_inside_the_frame() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let (mut state, clock) = clocked(width, height);
        decision_queue::prune_closed(&mut state, &facts("c1"));
        decision_queue::raise_team_proposal(&mut state, proposal(), Responder::Local);
        paint(&mut state, &facts("c1"), width, height);
        ready(&mut state, &clock);
        let draft = "unsent draft\n".repeat(20);
        state.input_buffer = draft.clone();
        let text = screen(&paint(&mut state, &facts("c1"), width, height));
        assert!(
            text.contains("Finish or clear your draft to decide."),
            "{text}"
        );
        for c in ['y', 'r', 'n'] {
            assert_eq!(
                route(&mut state, &key(c), &facts("c1")),
                Some(RouteEffect::Consumed)
            );
        }
        assert_eq!(state.input_buffer, draft);
        state.input_buffer.clear();
        let before = super::support::layout_of(&state, width, height).chat_pane;
        assert_eq!(
            route(&mut state, &key('r'), &facts("c1")),
            Some(RouteEffect::Consumed)
        );
        assert_eq!(
            before,
            super::support::layout_of(&state, width, height).chat_pane
        );
        assert!(
            state.decisions.items().any(
                |item| matches!(&item.subject, DecisionSubject::TeamProposal(p) if p.reviewing)
            )
        );
        let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
        assert!(
            state.decisions.items().any(
                |item| matches!(&item.subject, DecisionSubject::TeamProposal(p) if !p.reviewing)
            )
        );
        assert_eq!(
            route(&mut state, &key('y'), &facts("c1")),
            Some(RouteEffect::Action(InputAction::Decision(
                DecisionEffect::TeamProposalSend
            )))
        );
    }
}

#[test]
fn withdrawal_shows_the_original_conversation_copy() {
    let (mut state, _) = clocked(80, 24);
    decision_queue::prune_closed(&mut state, &facts("c1"));
    decision_queue::raise_team_proposal(&mut state, proposal(), Responder::Local);
    decision_queue::withdraw_team_proposal_conversation(&mut state, "c1");
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(
        text.contains("Proposal withdrawn with the conversation. Nothing was sent."),
        "{text}"
    );
    assert_eq!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Consumed)
    );
}

#[test]
fn proposal_body_and_copy_keep_provenance_separate() {
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;
    let (mut state, clock) = clocked(80, 24);
    let copies = super::support::record_copies(&mut state);
    decision_queue::prune_closed(&mut state, &facts("c1"));
    let original = Arc::new(TeamProposal {
        id: "hostile".into(),
        conversation_id: "c1".into(),
        recipients: vec!["peer\nforged".into()],
        message_type: SemanticMessageType::ArchitectureUpdate,
        body: "one\u{1b}[31mtwo\nthree\nfour\nfive\nsix".into(),
    });
    decision_queue::raise_team_proposal(&mut state, original.clone(), Responder::Local);
    for width in [60, 80, 130] {
        let item = state.decisions.items().next().unwrap();
        let rows = body_lines_for_test(item, &state, width);
        let text = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(
            text.iter().any(|row| row.contains("To: peer forged")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|row| row.contains("… more not shown — [r] Review first")),
            "{text:?}"
        );
        assert!(text.iter().all(|row| !row.contains('\u{1b}')), "{text:?}");
        assert_eq!(text.iter().filter(|row| row.starts_with("│ ")).count(), 4);
    }
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    let _ = route(&mut state, &key('c'), &facts("c1"));
    assert_eq!(*copies.lock().unwrap(), vec![original.body.clone()]);
    assert_eq!(
        state
            .decisions
            .items()
            .next()
            .and_then(|item| match &item.subject {
                DecisionSubject::TeamProposal(payload) => Some(payload.proposal.body.as_str()),
                _ => None,
            }),
        Some(original.body.as_str())
    );
}

#[test]
fn story21_6_complete_recipient_disclosure_survives_review_and_send() {
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;
    let (mut state, clock) = clocked(80, 24);
    decision_queue::prune_closed(&mut state, &facts("c1"));
    let mut recipients: Vec<String> = (0..18).map(|i| format!("roster-{i:02}")).collect();
    recipients.extend([
        format!("{}-first", "x".repeat(140)),
        format!("{}-second", "x".repeat(140)),
    ]);
    let original = Arc::new(TeamProposal {
        id: "large".into(),
        conversation_id: "c1".into(),
        recipients: recipients.clone(),
        message_type: SemanticMessageType::ArchitectureUpdate,
        body: "body".into(),
    });
    decision_queue::raise_team_proposal(&mut state, original, Responder::Local);
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    for reviewing in [false, true] {
        if reviewing {
            assert_eq!(
                route(&mut state, &key('r'), &facts("c1")),
                Some(RouteEffect::Consumed)
            );
        }
        for width in [58, 78] {
            let item = state.decisions.items().next().unwrap();
            let rows = body_lines_for_test(item, &state, width);
            let joined: String = rows
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.content.as_ref())
                .collect();
            assert!(
                joined.contains(&format!("To: {}", recipients.join(", "))),
                "{joined}"
            );
        }
    }
    route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert_eq!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::TeamProposalSend
        )))
    );
    let Some(DecisionSubject::TeamProposal(payload)) = state.decisions.take_resolved() else {
        panic!("the exact proposal must be delivered");
    };
    assert_eq!(payload.proposal.recipients, recipients);
    assert!(state.decisions.take_resolved().is_none());
}

#[test]
fn story21_6_wide_send_rail_discloses_without_clipping() {
    for width in [60, 80] {
        let (mut state, _) = clocked(width, 24);
        decision_queue::prune_closed(&mut state, &facts("c1"));
        let base = proposal();
        let p = TeamProposal {
            id: base.id.clone(),
            conversation_id: base.conversation_id.clone(),
            recipients: vec!["a-very-long-recipient-alias".repeat(4), "other".into()],
            message_type: base.message_type,
            body: base.body.clone(),
        };
        decision_queue::raise_team_proposal(&mut state, Arc::new(p), Responder::Local);
        let text = screen(&paint(&mut state, &facts("c1"), width, 24));
        assert!(text.contains("[y] Send to … (see To)"), "{text}");
        assert!(text.contains("[r] Review first"), "{text}");
        assert!(text.contains("[n] Dismiss"), "{text}");
    }
}
