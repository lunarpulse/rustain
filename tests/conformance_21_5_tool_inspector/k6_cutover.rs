//! K6: production geometry, replacement, preemption and ownership settlement.
//! The existing shared K6 scanner remains the sole source gate (owned by parent).
use super::support::*;
use ratatui::style::Modifier;
use rustain::adapters::tui::{
    app::InputAction,
    decision_pane::{self, Arrival, DecisionDraft, DecisionKind, Raised},
    keymap::Key,
    presentation::{self, Surface},
    quit_confirm::{self, QuitFacts, QuitHost, QuitRequest},
    tool_inspector,
};
use rustain::domain::{
    models::{
        Conversation, FocusState, InvocationStatus as S, RequestId, ToolRisk, Turn,
        WriteDiffState as D, tool_call::ApprovalSource,
    },
    services::approval_runtime::ApprovalRuntimeEvent,
};
use rustain::infrastructure::runtime::{approval_bridge, event_loop};
use serde_json::json;
use std::time::Duration;

fn fixture(width: u16, height: u16) -> Fixture {
    Fixture::single(
        width,
        height,
        "Bash",
        json!({"command":"seq 1 99"}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    )
    .0
}

#[test]
fn sheets_fit_empty_and_full_composers_without_covering_the_status_row() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let mut f = fixture(width, height);
            if full {
                f.h.state.input_buffer = ["draft"; 8].join("\n");
                f.h.state.cursor_position = 5;
            }
            // Compare the uncovered status against its real no-context-hint
            // baseline: opening a frame intentionally removes Inspect discovery.
            let session_count = f.h.state.session_count;
            f.h.state.session_count = u32::MAX;
            f.paint();
            let before = f.layout();
            assert_eq!(before.input_area.height, if full { 10 } else { 3 });
            let status = pixels(f.buffer(), before.status_bar);
            assert!(f.h.state.current_hint.is_none());
            f.h.state.session_count = session_count;
            f.paint();
            assert!(
                f.h.state.current_hint.is_some(),
                "restore enabled Stage hints"
            );
            let mut composer = pixels(f.buffer(), before.input_area);
            for cell in &mut composer {
                cell.modifier.insert(Modifier::DIM);
            }
            f.open();
            let after = f.layout();
            assert_eq!(
                (before.chat_pane, before.input_area, before.status_bar),
                (after.chat_pane, after.input_area, after.status_bar)
            );
            let rect = f.frame_rect();
            let region = presentation::geometry::modal_region(&after, f.buffer().area);
            if width < 100 {
                assert_eq!(rect, region);
            } else {
                assert!(rect.width < region.width && rect.height < region.height);
                assert_eq!(rect.x, region.x + (region.width - rect.width) / 2);
                assert_eq!(rect.y, region.y + (region.height - rect.height) / 2);
            }
            assert_eq!(f.buffer()[(rect.x, rect.y)].symbol(), "┌");
            assert_eq!(
                f.buffer()[(rect.right() - 1, rect.bottom() - 1)].symbol(),
                "┘"
            );
            let p = tool_inspector::layout::parts(rect);
            assert_eq!(p.tabs.height, 1);
            assert!(row(f.buffer(), p.tabs, p.tabs.y).contains("[Output]"));
            assert!(p.body.height >= 1 && p.rail.height >= 1);
            assert!(row(f.buffer(), p.body, p.body.y).contains("row 000"));
            assert!(
                row(f.buffer(), p.rail, p.rail.y).contains("Back"),
                "the fitted first rail must remain actionable"
            );
            assert!(p.rail.bottom() < rect.bottom());
            assert!(rect.bottom() <= after.status_bar.y);
            if !full || width != 60 {
                assert!(p.identity.is_some() && p.status.is_some());
            }
            if width == 60 && full {
                assert_eq!(p.body.height, 1);
                assert!(p.identity.is_none() && p.status.is_none());
            }
            assert_eq!(
                pixels(f.buffer(), after.status_bar),
                status,
                "{width}x{height}, full={full}: status covered or displaced"
            );
            assert_eq!(pixels(f.buffer(), after.input_area), composer);
        }
    }
}

#[test]
fn p_is_inert_and_enter_never_expands_stage_rows() {
    let mut f = fixture(80, 24);
    let height = f.h.state.total_content_height;
    let stage = pixels(f.buffer(), f.layout().chat_pane);
    let cache = f.cache();
    assert!(matches!(
        f.press(Key::Char('p')),
        InputAction::Consumed | InputAction::Ignored
    ));
    f.paint();
    assert_eq!(f.h.state.total_content_height, height);
    assert_eq!(pixels(f.buffer(), f.layout().chat_pane), stage);
    f.open();
    assert_eq!(f.h.state.total_content_height, height);
    assert_eq!(f.cache(), cache);
    assert!(!f.h.screen_text().contains("┌─ Bash"));
    f.press(Key::Esc);
    f.paint();
    assert_eq!(pixels(f.buffer(), f.layout().chat_pane), stage);
}

#[test]
fn all_named_stage_replacements_drop_the_frame_and_payload_in_the_owning_manager() {
    for reason in ["tab switch", "/new", "/clear", "fork", "rewind"] {
        let mut f = fixture(80, 24);
        f.open();
        let surface = f.h.state.modal_stack.top().unwrap().surface;
        let old_id = f.conversation().id.clone();
        let old_view = f.view();
        if reason == "/new" || reason == "/clear" {
            // Parse the real slash command before applying the exact shared host seam.
            f.press(Key::Esc);
            f.h.state.focus = FocusState::Input;
            f.h.state.input_buffer = reason.into();
            f.h.state.cursor_position = reason.len();
            assert!(
                matches!(f.press(Key::Enter),InputAction::ExecuteCommand { name,.. } if name == reason[1..])
            );
            f.h.state.focus = FocusState::Chat;
            f.paint();
            f.open();
            event_loop::reset_stage_conversation(
                &mut f.tabs,
                &mut f.h.conversation,
                &mut f.h.state,
                Conversation {
                    id: format!("replacement-{reason}"),
                    ..Conversation::default()
                },
            );
            assert_eq!(f.tabs.active_tab().view_state.scroll_offset, 0);
            assert!(f.tabs.active_tab().view_state.focused_turn.is_none());
            assert!(f.h.conversation.turns.is_empty());
            assert_ne!(f.h.conversation.id, old_id);
        } else {
            match reason {
                "tab switch" | "fork" => {
                    f.tabs.create_tab_with_conversation(Conversation {
                        id: format!("destination-{reason}"),
                        ..Conversation::default()
                    });
                    f.tabs.active_tab_mut().view_state.scroll_offset = 7;
                    if reason == "tab switch" {
                        f.tabs.switch_to_index(1);
                        assert_eq!(
                            f.conversation().id,
                            old_id,
                            "switch control really returns to the owning source tab"
                        );
                        f.tabs.switch_to_index(2);
                    }
                }
                "rewind" => {
                    f.tabs.active_tab_mut().conversation = Conversation {
                        id: old_id.clone(),
                        ..Conversation::default()
                    };
                }
                _ => unreachable!(),
            }
            let destination_view = f.view();
            f.load();
            assert_eq!(
                f.view(),
                destination_view,
                "{reason}: discard cannot restore an old view into the destination"
            );
            assert_eq!(f.h.conversation.id == old_id, reason == "rewind");
            assert!(f.h.conversation.turns.is_empty());
            if reason != "rewind" {
                assert_ne!(
                    f.view(),
                    old_view,
                    "the independent owning destination is the control"
                );
            }
        }
        assert!(!presentation::is_open(&f.h.state, surface), "{reason}");
        assert!(
            f.h.state.tool_inspectors.is_empty(),
            "{reason}: no dangling cached payload"
        );
        assert!(presentation::consistent(&f.h.state));
    }
}

#[test]
fn automatic_decision_preempts_and_escape_restores_exact_tab_viewers_and_cells() {
    let mut f = fixture(80, 24);
    f.open();
    f.press(Key::Ctrl('d'));
    f.paint();
    f.press(Key::Right);
    f.paint();
    f.press(Key::Right);
    f.press(Key::Down);
    f.paint();
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let payload = format!("{:?}", f.tool());
    let body = pixels(f.buffer(), f.body_rect());
    assert!(f.h.state.decisions.is_empty());
    f.clock.advance(Duration::from_secs(2));
    assert_eq!(
        decision_pane::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "budget arrival control"),
            Arrival::Automatic
        ),
        Raised::Focused
    );
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::DecisionPane
    );
    f.paint();
    assert!(f.h.screen_text().contains("budget arrival control"));
    f.clock.advance(decision_pane::INERT_AFTER_PAINT);
    assert!(presentation::is_open(&f.h.state, frame.surface));
    assert_eq!(f.h.state.tool_inspectors.len(), 1);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(format!("{:?}", f.tool()), payload);
    assert_eq!(pixels(f.buffer(), f.body_rect()), body);
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn exact_target_reopen_retains_cache_different_target_pushes_and_close_releases() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"first"}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    prose(&mut turn, &rows(24));
    let second = call(
        &mut turn,
        "Edit",
        json!({"old_string":"old","new_string":"new"}),
        S::Success,
        Some(("second target body", D::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    f.focus_call(&first);
    f.open();
    f.press(Key::Ctrl('d'));
    f.paint();
    let original = f.h.state.modal_stack.top().unwrap().clone();
    let payload = format!("{:?}", f.tool());
    f.press(Key::Ctrl('x'));
    f.press(Key::Char('?'));
    assert_eq!(f.h.state.modal_stack.top().unwrap().surface, Surface::Help);
    assert!(
        f.inspect(),
        "the host effect reopens the exact existing target under a cover"
    );
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&original));
    assert_eq!(format!("{:?}", f.tool()), payload);
    assert_eq!(f.h.state.tool_inspectors.len(), 1);
    f.focus_call(&second);
    assert!(f.inspect());
    f.paint();
    let new_frame = f.h.state.modal_stack.top().unwrap().clone();
    assert_ne!(new_frame.id, original.id);
    assert_eq!(f.h.state.modal_stack.len(), 2);
    assert_eq!(f.h.state.tool_inspectors.len(), 2);
    assert!(f.body_row(0).contains("second target body"));
    assert!(presentation::consistent(&f.h.state));
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&original));
    assert_eq!(format!("{:?}", f.tool()), payload);
    assert_eq!(f.h.state.tool_inspectors.len(), 1);
    assert!(!f.h.state.tool_inspectors.contains_key(&new_frame.id));
    f.press(Key::Esc);
    assert!(f.h.state.tool_inspectors.is_empty());
    assert!(presentation::consistent(&f.h.state));
    f.focus_call(&first);
    f.open();
    assert!(
        f.h.state.modal_stack.top().unwrap().id > new_frame.id,
        "a discarded opening cannot be reused as a late generation"
    );
    assert_eq!(
        f.tool().viewers[0].top,
        0,
        "the newly opened target cannot resurrect its discarded scroll/cache payload"
    );
    assert_eq!(f.h.state.tool_inspectors.len(), 1);
    f.press(Key::Esc);
    f.paint();
    assert!(
        f.h.state.tool_inspectors.is_empty(),
        "refresh after close never resurrects a discarded owner"
    );
}

#[test]
fn depth_cap_continuation_keeps_exact_inspector_payload_until_restoration_or_discard() {
    for discard in [false, true] {
        let mut f = fixture(80, 24);
        f.press(Key::Ctrl('p'));
        f.press(Key::Ctrl('x'));
        f.press(Key::Char('?'));
        f.press(Key::Ctrl('x'));
        f.press(Key::Char('n'));
        assert_eq!(f.h.state.modal_stack.len(), 3);
        assert!(f.inspect());
        f.paint();
        assert_eq!(f.h.state.modal_stack.len(), 4);
        f.press(Key::Down);
        f.paint();
        let tool_frame = f.h.state.modal_stack.top().unwrap().clone();
        let payload = format!("{:?}", f.tool());
        f.clock.advance(Duration::from_secs(2));
        assert_eq!(
            decision_pane::raise(
                &mut f.h.state,
                DecisionDraft::new(DecisionKind::Budget, "continuation arrival"),
                Arrival::Automatic
            ),
            Raised::Focused
        );
        f.paint();
        f.clock.advance(decision_pane::INERT_AFTER_PAINT);
        assert_eq!(f.h.state.modal_stack.len(), 4);
        assert!(
            !f.h.state
                .modal_stack
                .iter()
                .any(|frame| frame.surface == tool_frame.surface),
            "tool is really suspended, not merely covered"
        );
        assert!(presentation::is_open(&f.h.state, tool_frame.surface));
        assert_eq!(
            format!("{:?}", f.h.state.tool_inspectors[&tool_frame.id]),
            payload
        );
        assert!(presentation::consistent(&f.h.state));
        if discard {
            event_loop::reset_stage_conversation(
                &mut f.tabs,
                &mut f.h.conversation,
                &mut f.h.state,
                Conversation::default(),
            );
            assert!(!presentation::is_open(&f.h.state, tool_frame.surface));
            assert!(f.h.state.tool_inspectors.is_empty());
        } else {
            f.press(Key::Esc);
            f.paint();
            assert_eq!(f.h.state.modal_stack.top(), Some(&tool_frame));
            assert_eq!(format!("{:?}", f.tool()), payload);
            f.press(Key::Esc);
            assert!(f.h.state.tool_inspectors.is_empty());
        }
        assert!(presentation::consistent(&f.h.state));
    }
}

#[test]
fn each_back_key_releases_a_real_queued_permission_before_the_next_paint() {
    for close in [Key::Esc, Key::Ctrl('c')] {
        let mut f = fixture(80, 24);
        f.open();
        let id = RequestId::new();
        let conversation_id = f.conversation().id.clone();
        approval_bridge::apply(
            &mut f.h.state,
            ApprovalRuntimeEvent::Requested {
                id: id.clone(),
                source: ApprovalSource::ForegroundTurn { conversation_id },
                tool: "Bash".into(),
                risk: ToolRisk::Elevated,
                consent_card: Some("queued inspector permission control".into()),
            },
            None,
        );
        assert!(f.h.state.pending_permission.is_none());
        assert_eq!(f.h.state.permission_queue.len(), 1);
        assert_eq!(f.press(close), InputAction::Consumed);
        assert!(f.h.state.tool_inspectors.is_empty());
        assert_eq!(
            f.h.state.pending_permission.as_ref().map(|p| &p.id),
            Some(&id),
            "{close:?}: key settlement, not a later paint, promotes"
        );
        assert!(f.h.state.permission_queue.is_empty());
        f.paint();
        assert!(
            f.h.screen_text()
                .contains("queued inspector permission control")
        );
        assert_eq!(
            f.press(Key::Char('y')),
            InputAction::PermissionAllow,
            "the promoted real request owns ingress"
        );
    }
}

#[test]
fn ctrl_q_precedes_the_inspector_and_preserves_its_payload_under_quit() {
    let mut f = fixture(80, 24);
    f.open();
    f.press(Key::Down);
    f.paint();
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let payload = format!("{:?}", f.tool());
    assert_eq!(f.press(Key::Ctrl('q')), InputAction::Quit);
    assert_eq!(
        quit_confirm::request(
            &mut f.h.state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            }
        ),
        QuitRequest::Confirming
    );
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::QuitConfirm
    );
    assert!(presentation::is_open(&f.h.state, frame.surface));
    assert_eq!(f.h.state.tool_inspectors.len(), 1);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(format!("{:?}", f.tool()), payload);
}

#[test]
fn a_missing_target_paints_the_defensive_notice_and_only_back_acts() {
    let mut f = fixture(80, 24);
    f.open();
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert!(
        f.h.screen_text()
            .contains("This tool call is no longer in the conversation.")
    );
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    for key in [Key::Right, Key::Left, Key::Down, Key::Char('G'), Key::Enter] {
        assert!(matches!(
            f.press(key),
            InputAction::Consumed | InputAction::Ignored
        ));
        f.paint();
        assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
        assert!(
            f.h.screen_text()
                .contains("This tool call is no longer in the conversation.")
        );
    }
    f.press(Key::Esc);
    assert!(f.h.state.tool_inspectors.is_empty());
}
