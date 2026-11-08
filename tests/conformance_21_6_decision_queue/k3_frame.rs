//! K3 — the decision is an L3 frame: geometry, inert window, the Esc table, stale
//! Close and the exact restore (AC2, AC4; A4, A6, A7, A9).
//!
//! Keys go through `presentation::route` (the router the event loop calls before
//! `handle_input`), paint through `presentation::compose`, items arrive through
//! their producers where one is public.

use std::time::Duration;

use ratatui::layout::Rect;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::decision_queue::{
    self, Arrival, DecisionDraft, DecisionEffect, DecisionKind, DecisionSubject, waiting,
};
use rustain::adapters::tui::handlers::context_warning::{
    handle_clear_context_warning, handle_upsert_context_warning,
};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface, geometry, is_open};
use rustain::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainKey;
use rustain::domain::models::ImageAttachment;
use rustain::domain::models::visual::DeleteConfirmTarget;
use rustain::infrastructure::runtime::event_loop::begin_delete_confirmation;

use super::support::{
    advance, clocked, facts, key, layout_of, paint, ready, route, row_of, row_text, screen,
    special, status_row,
};

fn image() -> ImageAttachment {
    ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    }
}

/// Raise `kind` through its producer (the two without a public producer, Recovery
/// and Budget, arrive as the event loop's own arm raises them).
fn raise_kind(state: &mut TuiState, kind: DecisionKind) {
    match kind {
        DecisionKind::Recovery => {
            decision_queue::raise(
                state,
                DecisionDraft::new(kind, "Recovered \"Plan the launch\" (1,200 tokens)."),
                Arrival::Automatic,
            );
        }
        DecisionKind::Budget => {
            decision_queue::raise(
                state,
                DecisionDraft::new(kind, "Daily budget ($10.00) reached."),
                Arrival::Automatic,
            );
        }
        DecisionKind::Carryover => decision_queue::request_carryover(state, None),
        DecisionKind::ContextWarning => {
            handle_upsert_context_warning(state, "c1", 85);
        }
        DecisionKind::LargeImage => {
            decision_queue::raise_large_image(state, image(), "Image is large (6 MB).");
        }
        DecisionKind::DeleteConversations => begin_delete_confirmation(
            state,
            DeleteConfirmTarget::Single {
                id: "c9".into(),
                title: "Old chat".into(),
            },
        ),
        DecisionKind::Permission | DecisionKind::Consent => {
            unreachable!("the permission family is raised by its own producers (k4)")
        }
    }
    assert!(presentation::consistent(state));
}

const ALL: [DecisionKind; 6] = [
    DecisionKind::Recovery,
    DecisionKind::Carryover,
    DecisionKind::Budget,
    DecisionKind::ContextWarning,
    DecisionKind::LargeImage,
    DecisionKind::DeleteConversations,
];

#[test]
fn k3_the_frame_is_an_l3_sheet_with_title_breadcrumb_body_and_rail_at_60_80_130() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let (mut state, _clock) = clocked(width, height);
        // A full composer: the worst case for the room the frame has.
        state.input_buffer = (0..12)
            .map(|n| format!("draft line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        // Chat focus: unsent composer text under Input focus would (rightly) protect it.
        state.focus = rustain::domain::models::FocusState::Chat;
        let f = facts("c1");
        handle_upsert_context_warning(&mut state, "c1", 85);
        let buffer = paint(&mut state, &f, width, height);
        let layout = layout_of(&state, width, height);
        let rect = geometry::frame_rect(geometry::modal_region(
            &layout,
            Rect::new(0, 0, width, height),
        ));
        let at = format!("{width}x{height}");
        let title = row_of(&buffer, "Decision: Context window")
            .unwrap_or_else(|| panic!("{at}: no frame title\n{}", screen(&buffer)));
        assert_eq!(title, rect.y, "{at}: the title is the frame's top border");
        let body = row_of(&buffer, "Running low on context (85%).")
            .unwrap_or_else(|| panic!("{at}: no body row\n{}", screen(&buffer)));
        let rail = row_of(&buffer, "[K] Compact")
            .unwrap_or_else(|| panic!("{at}: no rail row\n{}", screen(&buffer)));
        assert!(
            title < body && body <= rail && rail < rect.bottom() - 1,
            "{at}: title {title} < body {body} <= rail {rail} < bottom {}",
            rect.bottom() - 1
        );
        let rail_text = row_text(&buffer, rail);
        for pair in ["[K] Compact", "[n] Start fresh", "[Esc] Later"] {
            assert!(rail_text.contains(pair), "{at}: {pair:?} in {rail_text:?}");
        }
        // The frame never covers the composer or the status row.
        assert_eq!(
            row_text(&buffer, layout.status_bar.y).trim_end(),
            "status-row-sentinel",
            "{at}"
        );
        assert!(
            row_text(&buffer, layout.input_area.y).contains("composer-sentinel"),
            "{at}"
        );
        let screen_text = screen(&buffer);
        assert!(
            !screen_text.contains("System ·"),
            "{at}: the old title is gone"
        );
        assert!(!screen_text.contains("more waiting"), "{at}");
        // Full sheet below 100 columns, centered above.
        let corner = buffer.cell((rect.x, rect.y)).unwrap().symbol().to_owned();
        assert_eq!(corner, "┌", "{at}");
        if width < 100 {
            assert_eq!((rect.x, rect.width), (0, width), "{at}: a full sheet");
        } else {
            assert!(rect.x > 0 && rect.width < width, "{at}: centered");
            assert!(
                row_text(&buffer, 0).contains("stage row 00"),
                "{at}: the Stage shows around the box"
            );
        }
        if height >= 24 {
            assert!(
                screen_text.contains("Needs you › Context window | 1 pending"),
                "{at}: breadcrumb\n{screen_text}"
            );
        }
    }
}

#[test]
fn k3_every_kind_fits_sixty_by_sixteen_with_a_full_composer() {
    for kind in ALL {
        let (mut state, _clock) = clocked(60, 16);
        state.input_buffer = (0..12)
            .map(|n| format!("draft line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.focus = rustain::domain::models::FocusState::Chat;
        raise_kind(&mut state, kind);
        let buffer = paint(&mut state, &facts("c1"), 60, 16);
        let text = screen(&buffer);
        assert!(text.contains("Decision: "), "{kind:?}: title\n{text}");
        let first_rail_word = kind
            .rail()
            .first()
            .map(|(key, _)| key.clone())
            .expect("every kind has a rail");
        assert!(text.contains(&first_rail_word), "{kind:?}: rail\n{text}");
        assert!(
            row_text(&buffer, layout_of(&state, 60, 16).status_bar.y).trim_end()
                == "status-row-sentinel",
            "{kind:?}: the status row is never covered"
        );
    }
}

#[test]
fn rail_keys_are_inert_for_500ms_after_first_paint() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    raise_kind(&mut state, DecisionKind::Recovery);
    assert!(is_open(&state, Surface::Decision));
    // The first paint starts the 500 ms clock (compose reported the frame visible).
    paint(&mut state, &f, 80, 24);
    advance(&clock, Duration::from_millis(100));
    assert_eq!(
        route(&mut state, &key('n'), &f),
        Some(RouteEffect::Consumed),
        "100 ms: dropped"
    );
    advance(&clock, Duration::from_millis(399));
    assert_eq!(
        route(&mut state, &key('n'), &f),
        Some(RouteEffect::Consumed),
        "499 ms: dropped"
    );
    assert!(
        state.decisions.holds(DecisionKind::Recovery),
        "dropped keys are never replayed"
    );
    advance(&clock, Duration::from_millis(2));
    assert_eq!(
        route(&mut state, &key('n'), &f),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::RecoveryNewSession
        ))),
        "501 ms: the fresh key answers"
    );
    assert!(state.decisions.is_empty());
}

#[test]
fn k3_an_unpainted_frame_stays_inert_however_long_it_waits() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    raise_kind(&mut state, DecisionKind::Recovery);
    advance(&clock, Duration::from_secs(30));
    assert_eq!(
        route(&mut state, &key('n'), &f),
        Some(RouteEffect::Consumed)
    );
    assert!(state.decisions.holds(DecisionKind::Recovery));
}

#[test]
fn k3_esc_decides_the_safe_default_or_defers_and_never_decides_the_rest() {
    for kind in ALL {
        let (mut state, clock) = clocked(80, 24);
        let f = facts("c1");
        raise_kind(&mut state, kind);
        ready(&mut state, &clock);
        let effect = route(&mut state, &special(DomainKey::Esc), &f);
        match kind {
            DecisionKind::Recovery => {
                assert_eq!(
                    effect,
                    Some(RouteEffect::Action(InputAction::Decision(
                        DecisionEffect::RecoveryContinue
                    )))
                );
                assert!(state.decisions.is_empty());
            }
            DecisionKind::DeleteConversations => {
                assert_eq!(
                    effect,
                    Some(RouteEffect::Action(InputAction::CancelDelete)),
                    "Esc cancels: the legacy default"
                );
                assert!(state.decisions.is_empty());
            }
            _ => {
                assert_eq!(effect, Some(RouteEffect::Consumed), "{kind:?} defers");
                assert!(!is_open(&state, Surface::Decision), "{kind:?}");
                assert_eq!(waiting(&state), 1, "{kind:?}: still counted");
                assert!(state.decisions.items().next().unwrap().deferred);
                assert!(
                    status_row(&state, 80).contains("Needs you (1) ^X q"),
                    "{kind:?}: the chip carries it"
                );
            }
        }
    }
}

#[test]
fn k3_ctrl_c_backs_one_frame_without_deciding_and_ctrl_q_asks_to_quit() {
    for kind in ALL {
        let (mut state, clock) = clocked(80, 24);
        let f = facts("c1");
        raise_kind(&mut state, kind);
        ready(&mut state, &clock);
        assert_eq!(
            route(&mut state, &special(DomainKey::CtrlC), &f),
            Some(RouteEffect::Consumed),
            "{kind:?}: never an effect, never a turn cancel"
        );
        assert!(!is_open(&state, Surface::Decision), "{kind:?}");
        assert_eq!(waiting(&state), 1, "{kind:?}: nothing was decided");
    }
    // Ctrl+Q asks the guard and leaves the decision where it is.
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    raise_kind(&mut state, DecisionKind::Budget);
    ready(&mut state, &clock);
    assert_eq!(
        route(&mut state, &special(DomainKey::CtrlQ), &f),
        Some(RouteEffect::QuitRequest)
    );
    assert!(state.decisions.holds(DecisionKind::Budget));
    // The guard opens over the frame and a `n` goes back to it.
    let mut quit = QuitFacts::idle(QuitHost::Local);
    quit.pending_decisions = 1;
    presentation::reducer::request_quit(&mut state, quit);
    assert_eq!(
        state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::QuitConfirm)
    );
    route(&mut state, &key('n'), &f);
    assert_eq!(
        state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::Decision)
    );
    assert!(state.decisions.holds(DecisionKind::Budget));
}

#[test]
fn k3_a_stale_item_paints_its_copy_with_close_only_and_no_key_acts() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    raise_kind(&mut state, DecisionKind::ContextWarning);
    ready(&mut state, &clock);
    // Its condition clears while the user is looking at it.
    handle_clear_context_warning(&mut state, "c1");
    let buffer = paint(&mut state, &f, 80, 24);
    let text = screen(&buffer);
    assert!(
        text.contains("This decision is no longer pending."),
        "{text}"
    );
    assert!(text.contains("[Esc] Close"), "{text}");
    assert!(
        !text.contains("[K] Compact"),
        "no mutation key is offered\n{text}"
    );
    for event in [
        key('k'),
        key('K'),
        key('n'),
        key('y'),
        special(DomainKey::Enter),
    ] {
        assert_eq!(
            route(&mut state, &event, &f),
            Some(RouteEffect::Consumed),
            "{event:?} acts on nothing"
        );
        assert_eq!(waiting(&state), 1);
        assert!(is_open(&state, Surface::Decision));
    }
    // Close removes it; nothing reopens.
    assert_eq!(
        route(&mut state, &special(DomainKey::Esc), &f),
        Some(RouteEffect::Consumed)
    );
    assert!(state.decisions.is_empty());
    assert!(!is_open(&state, Surface::Decision));
    assert!(!status_row(&state, 80).contains("Needs you"));
}

#[test]
fn k3_resolve_and_defer_restore_exactly_the_frame_they_interrupted() {
    for (case, answer) in [
        ("resolve", key('n')),
        ("defer", special(DomainKey::Esc)),
        ("back", special(DomainKey::CtrlC)),
    ] {
        let (mut state, clock) = clocked(80, 24);
        let f = facts("c1");
        handle_input(&mut state, &special(DomainKey::CtrlX));
        handle_input(&mut state, &key('?'));
        state.help_overlay.scroll_offset = 7;
        let before: Vec<_> = state.modal_stack.iter().cloned().collect();
        assert_eq!(before.len(), 1, "{case}: Help is open");
        raise_kind(&mut state, DecisionKind::LargeImage);
        assert_eq!(
            state.modal_stack.len(),
            2,
            "{case}: the decision opened over Help"
        );
        let over = paint(&mut state, &f, 80, 24);
        assert!(
            screen(&over).contains("Interrupted: Help"),
            "{case}: names what it interrupted\n{}",
            screen(&over)
        );
        ready(&mut state, &clock);
        route(&mut state, &answer, &f);
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            before,
            "{case}: exactly the interrupted frame"
        );
        assert_eq!(state.help_overlay.scroll_offset, 7, "{case}");
    }
}

#[test]
fn k3_a_deferred_item_is_never_reopened_automatically() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    raise_kind(&mut state, DecisionKind::ContextWarning);
    ready(&mut state, &clock);
    route(&mut state, &special(DomainKey::Esc), &f);
    // Time, another arrival, ticks and repaints never bring it back.
    advance(&clock, Duration::from_secs(120));
    handle_upsert_context_warning(&mut state, "c2", 90);
    decision_queue::tick(&mut state);
    paint(&mut state, &f, 80, 24);
    assert!(!is_open(&state, Surface::Decision));
    assert_eq!(waiting(&state), 2);
    // The list selects the non-deferred c2; choose the deferred c1 explicitly.
    handle_input(&mut state, &special(DomainKey::CtrlX));
    assert_eq!(
        handle_input(&mut state, &key('q')),
        InputAction::FocusDecisions
    );
    decision_queue::open_waiting(&mut state, &f);
    assert!(is_open(&state, Surface::DecisionQueue));
    route(&mut state, &special(DomainKey::Up), &f);
    route(&mut state, &special(DomainKey::Enter), &f);
    assert!(is_open(&state, Surface::Decision));
    let shown = decision_queue::current(&state, &f).expect("the focused item");
    assert_eq!(shown.conversation_id.as_deref(), Some("c1"));
    assert!(matches!(shown.subject, DecisionSubject::Text));
}
