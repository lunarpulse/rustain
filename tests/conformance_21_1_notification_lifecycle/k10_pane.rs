//! K10 — the interim decision pane and arrival protection (AC9–AC11; RU-5/6/7).
//!
//! Every assertion enters through the public front doors the event loop calls:
//! the producers (`handle_upsert_context_warning`, `begin_delete_confirmation`,
//! S-LOG `K`/`n`), `decision_pane::{raise, handle_domain_event, open_waiting,
//! note_painted, render}`, `handle_input` (typing, `Ctrl+X q`), `status_bar::render`.
//! Time is a `MockClock` (no sleeps). The event loop's own effect arms
//! (`InputAction::Decision`) need a daemon-less runtime and are covered by the
//! native smoke (K12b).

use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::Paragraph;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::decision_pane::{
    self, Arrival, DecisionDraft, DecisionEffect, DecisionKind, INERT_AFTER_PAINT, PaneOutcome,
    Raised, waiting_toast,
};
use rustain::adapters::tui::handlers::context_warning::handle_upsert_context_warning;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use rustain::adapters::tui::notifications::{
    self, NoticeAction, NoticeDraft, NoticeSeverity, NoticeSource,
};
use rustain::adapters::tui::presentation::{self, Shell, Surface, is_open};
use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{FocusState, ImageAttachment};
use rustain::infrastructure::runtime::event_loop::begin_delete_confirmation;

use super::k4_retention::status_row;
use super::support::{advance, clocked, layout_of, row_text};

fn key(c: char) -> DomainInputEvent {
    DomainInputEvent::KeyPress(c)
}

fn special(k: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(k)
}

fn facts(active: &str) -> LogFacts {
    LogFacts {
        active_conversation_id: Some(active.to_owned()),
        decisions_supported: true,
        retry_supported: true,
        context_warning: true,
        ..LogFacts::default()
    }
}

fn press(state: &mut TuiState, event: DomainInputEvent, f: &LogFacts) -> Option<PaneOutcome> {
    let outcome = decision_pane::handle_domain_event(state, &event, f);
    assert!(presentation::consistent(state));
    outcome
}

/// First paint, then the inert window passes.
fn ready(state: &mut TuiState, clock: &MockClock) {
    decision_pane::note_painted(state);
    advance(clock, INERT_AFTER_PAINT);
}

fn automatic(state: &mut TuiState, kind: DecisionKind) -> Raised {
    let raised = decision_pane::raise(
        state,
        DecisionDraft::new(kind, "the question"),
        Arrival::Automatic,
    );
    assert!(presentation::consistent(state));
    raised
}

fn waiting_toasts(state: &TuiState) -> usize {
    state
        .notifications
        .visible()
        .filter(|r| r.message == waiting_toast())
        .count()
}

/// Paint the chat pane with distinct rows, then the pane the way the local host
/// does (chat pane rect, after the stage). Returns the buffer.
fn draw_pane(state: &TuiState, f: &LogFacts, width: u16, height: u16, with_pane: bool) -> Buffer {
    let layout = layout_of(state, width, height);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let pane = layout.chat_pane;
            let rows: Vec<String> = (0..pane.height)
                .map(|row| {
                    let tag = format!("stage row {row:02} ");
                    tag.repeat(usize::from(pane.width) / tag.len() + 1)
                })
                .collect();
            frame.render_widget(Paragraph::new(rows.join("\n")), pane);
            frame.render_widget(Paragraph::new("composer-sentinel"), layout.input_area);
            if with_pane {
                let shell = Shell::new(frame.area(), &layout);
                assert!(presentation::compose(
                    frame,
                    state,
                    &shell,
                    Host::Local,
                    f,
                    |_| {},
                    |frame| {
                        frame.render_widget(
                            Paragraph::new("status-row-sentinel"),
                            layout.status_bar,
                        );
                    },
                ));
            } else {
                frame.render_widget(Paragraph::new("status-row-sentinel"), layout.status_bar);
            }
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn screen(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── RU-6 arrival matrix ──────────────────────────────────────────────────────

#[test]
fn k10_an_idle_empty_input_chat_or_sidebar_takes_the_first_arrival() {
    for focus in [
        FocusState::Input,
        FocusState::Chat,
        FocusState::Sidebar {
            panel: rustain::domain::models::visual::PanelType::History,
            selected: 0,
        },
    ] {
        let (mut state, _c) = clocked(80, 24);
        state.focus = focus.clone();
        assert_eq!(
            automatic(&mut state, DecisionKind::Recovery),
            Raised::Focused,
            "{focus:?}"
        );
        assert!(is_open(&state, Surface::DecisionPane));
        assert_eq!(state.modal_stack.len(), 1);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::DecisionPane
        );
        assert_eq!(
            waiting_toasts(&state),
            0,
            "{focus:?}: no toast when it takes focus"
        );
    }
}

#[test]
fn k10_a_typing_composer_is_never_interrupted() {
    // Non-empty composer.
    let (mut state, _c) = clocked(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer = "half a sentence".into();
    assert_eq!(automatic(&mut state, DecisionKind::Budget), Raised::Waiting);
    assert!(!is_open(&state, Surface::DecisionPane));
    assert!(state.modal_stack.is_empty());
    assert_eq!(state.input_buffer, "half a sentence");
    assert_eq!(
        waiting_toasts(&state),
        1,
        "exactly one `Decision waiting — Ctrl+X q` toast"
    );
    assert_eq!(
        state.notifications.visible().last().unwrap().severity,
        NoticeSeverity::Info
    );

    // A printable key inside the last second, composer emptied again (real front door).
    let (mut state, clock) = clocked(80, 24);
    state.focus = FocusState::Input;
    handle_input(&mut state, &key('a'));
    state.input_buffer.clear();
    state.cursor_position = 0;
    advance(&clock, Duration::from_millis(999));
    assert_eq!(
        automatic(&mut state, DecisionKind::Budget),
        Raised::Waiting,
        "999 ms"
    );
    // Positive control: 1 s after the last printable key the same arrival takes focus.
    let (mut state, clock) = clocked(80, 24);
    state.focus = FocusState::Input;
    handle_input(&mut state, &key('a'));
    state.input_buffer.clear();
    state.cursor_position = 0;
    advance(&clock, Duration::from_secs(1));
    assert_eq!(
        automatic(&mut state, DecisionKind::Budget),
        Raised::Focused,
        "1 s"
    );
}

#[test]
fn k10_open_surfaces_are_never_taken_from() {
    // Keep every legacy protection branch. Non-text Help/S-LOG now have
    // separate eligible-preemption controls below (21.4 AC7).
    type Setup = fn(&mut TuiState);
    use rustain::adapters::tui::keymap::Context;
    use rustain::adapters::tui::presentation::reducer;
    use rustain::domain::models::autocomplete::AutocompleteKind;
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};
    let surfaces: [(&str, Setup); 14] = [
        ("S-QUIT", |s| {
            let mut f = QuitFacts::idle(QuitHost::Local);
            f.composer_draft = true;
            quit_confirm::request(s, f);
        }),
        ("model selector", |s| s.model_selector.active = true),
        ("sign-in", |s| s.login.active = true),
        ("profile switcher", |s| s.profile_switcher.active = true),
        ("usage panel", |s| s.usage_panel.active = true),
        ("palette", |s| {
            reducer::open(s, Surface::CommandPalette);
        }),
        ("which-key", |s| s.pending_leader = Some(Context::Chat)),
        ("reverse search", |s| s.reverse_search.active = true),
        ("in-conversation search", |s| s.search_state.active = true),
        ("ask_user_question", |s| {
            s.ask_user_question = Some(
                rustain::adapters::tui::widgets::ask_user_question::AskUserQuestionState {
                    tool_use_id: "tu-1".to_string(),
                    question: "What?".to_string(),
                    input_buffer: String::new(),
                    cursor_position: 0,
                    submitted_answer: None,
                },
            );
        }),
        ("a proposal card", |s| {
            s.pending_large_image = Some(ImageAttachment {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            });
        }),
        ("a plan-cancel confirmation", |s| {
            s.task_panel_state.cancel_plan_confirm = Some("plan-1".into());
        }),
        ("autocomplete popup", |s| {
            handle_input(s, &key('/'));
            assert_eq!(s.autocomplete.kind, Some(AutocompleteKind::SlashCommand));
            assert_eq!(s.focus, FocusState::Input);
            assert!(s.modal_stack.is_empty());
        }),
        ("permission confirmation overlay", |s| {
            s.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));
        }),
    ];
    for (name, setup) in surfaces {
        let (mut state, _c) = clocked(80, 24);
        setup(&mut state);
        let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
        assert_eq!(
            automatic(&mut state, DecisionKind::Budget),
            Raised::Waiting,
            "{name}"
        );
        assert!(!is_open(&state, Surface::DecisionPane), "{name}");
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames,
            "{name}: protected frames are untouched"
        );
        assert!(
            state.decisions.holds(DecisionKind::Budget),
            "{name}: it waits"
        );
    }
}

#[test]
fn k10_non_text_help_and_log_allow_arrival_and_restore_the_interrupted_frame() {
    for surface in [Surface::Help, Surface::NotificationLog] {
        let (mut state, clock) = clocked(80, 24);
        state.focus = FocusState::Chat;
        match surface {
            Surface::Help => {
                handle_input(&mut state, &key('?'));
                state.help_overlay.scroll_offset = 3;
            }
            Surface::NotificationLog => {
                notifications::info(&mut state, NoticeSource::System, "older record");
                notifications::info(&mut state, NoticeSource::System, "newer record");
                notification_log::open(&mut state);
                notification_log::handle_domain_event(
                    &mut state,
                    &special(DomainKey::Down),
                    &LogFacts::default(),
                );
            }
            _ => unreachable!(),
        }
        let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
        let log = state.notification_log.clone();
        advance(&clock, Duration::from_secs(1));
        assert_eq!(automatic(&mut state, DecisionKind::Budget), Raised::Focused);
        assert_eq!(state.modal_stack.len(), frames.len() + 1);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::DecisionPane
        );
        assert!(is_open(&state, surface));
        assert_eq!(waiting_toasts(&state), 0);
        ready(&mut state, &clock);
        assert_eq!(
            press(&mut state, special(DomainKey::Esc), &LogFacts::default()),
            Some(PaneOutcome::Consumed)
        );
        assert!(
            state.decisions.holds(DecisionKind::Budget),
            "Later keeps the item"
        );
        assert!(!is_open(&state, Surface::DecisionPane));
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames
        );
        assert_eq!(state.focus, FocusState::Chat);
        assert_eq!(
            state.notification_log, log,
            "Log payload and selected id restored"
        );
        if surface == Surface::Help {
            assert_eq!(state.help_overlay.scroll_offset, 3);
        }
    }
}

#[test]
fn k10_only_the_first_arrival_can_take_focus_and_a_deferral_blocks_for_a_minute() {
    let (mut state, _c) = clocked(80, 24);
    assert_eq!(
        automatic(&mut state, DecisionKind::Recovery),
        Raised::Focused
    );
    assert_eq!(
        automatic(&mut state, DecisionKind::Budget),
        Raised::Waiting,
        "0→1 only: a second item waits behind the first"
    );

    // Defer cooldown: an item is deferred, then its condition clears; a new
    // arrival inside 60 s waits, at 60 s it takes focus.
    let (mut state, clock) = clocked(80, 24);
    assert_eq!(
        automatic(&mut state, DecisionKind::LargeImage),
        Raised::Focused
    );
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, special(DomainKey::Esc), &LogFacts::default()),
        Some(PaneOutcome::Consumed),
        "Esc is Later"
    );
    decision_pane::withdraw(&mut state, DecisionKind::LargeImage);
    advance(&clock, Duration::from_secs(59));
    assert_eq!(
        automatic(&mut state, DecisionKind::Budget),
        Raised::Waiting,
        "59 s"
    );
    decision_pane::withdraw(&mut state, DecisionKind::Budget);
    advance(&clock, Duration::from_secs(1));
    assert_eq!(
        automatic(&mut state, DecisionKind::Budget),
        Raised::Focused,
        "60 s"
    );
}

// ── inert window, no replay, exactly once ────────────────────────────────────

#[test]
fn k10_rail_keys_are_inert_for_500ms_after_first_paint_and_never_replayed() {
    let (mut state, clock) = clocked(80, 24);
    automatic(&mut state, DecisionKind::Recovery);
    let f = LogFacts::default();
    assert_eq!(
        press(&mut state, key('n'), &f),
        Some(PaneOutcome::Consumed),
        "before paint"
    );
    decision_pane::note_painted(&mut state);
    advance(&clock, INERT_AFTER_PAINT - Duration::from_millis(1));
    assert_eq!(
        press(&mut state, key('n'), &f),
        Some(PaneOutcome::Consumed),
        "499 ms"
    );
    assert_eq!(
        press(&mut state, special(DomainKey::Esc), &f),
        Some(PaneOutcome::Consumed),
        "Esc is inert too"
    );
    assert!(
        state.decisions.holds(DecisionKind::Recovery),
        "dropped, not replayed"
    );
    advance(&clock, Duration::from_millis(1));
    assert_eq!(
        press(&mut state, key('n'), &f),
        Some(PaneOutcome::Effect(DecisionEffect::RecoveryNewSession)),
        "positive control at 500 ms"
    );
    assert_eq!(
        press(&mut state, key('n'), &f),
        None,
        "exactly once: nothing left to answer"
    );
}

#[test]
fn k10_esc_follows_the_table_for_every_kind() {
    let cases = [
        (
            DecisionKind::Recovery,
            Some(DecisionEffect::RecoveryContinue),
        ),
        (
            DecisionKind::DeleteConversations,
            Some(DecisionEffect::CancelDelete),
        ),
        (DecisionKind::Carryover, None),
        (DecisionKind::Budget, None),
        (DecisionKind::ContextWarning, None),
        (DecisionKind::LargeImage, None),
    ];
    for (kind, effect) in cases {
        let (mut state, clock) = clocked(80, 24);
        decision_pane::raise(
            &mut state,
            DecisionDraft::new(kind, "q"),
            Arrival::Requested,
        );
        ready(&mut state, &clock);
        let outcome = press(&mut state, special(DomainKey::Esc), &LogFacts::default());
        match effect {
            Some(effect) => {
                assert_eq!(outcome, Some(PaneOutcome::Effect(effect)), "{kind:?}");
                assert!(state.decisions.is_empty(), "{kind:?}");
            }
            None => {
                // Later: never an effect (carryover Esc no longer opens a tab), the
                // item stays pending and the pane lets go of the keyboard.
                assert_eq!(outcome, Some(PaneOutcome::Consumed), "{kind:?}");
                assert!(state.decisions.holds(kind), "{kind:?}: still pending");
                assert!(
                    !is_open(&state, Surface::DecisionPane),
                    "{kind:?}: pane released"
                );
            }
        }
        assert!(state.modal_stack.is_empty(), "{kind:?}: pane frame closed");
    }
}

#[test]
fn k10_a_waiting_or_deferred_item_cannot_be_answered_by_a_stray_key() {
    // EV-3 zombie: with the item hidden, typed `y`/`n` are text, never answers.
    let (mut state, clock) = clocked(80, 24);
    state.focus = FocusState::Input;
    state.pending_large_image = Some(ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    });
    decision_pane::raise(
        &mut state,
        DecisionDraft::new(DecisionKind::LargeImage, "big"),
        Arrival::Requested,
    );
    ready(&mut state, &clock);
    press(&mut state, special(DomainKey::Esc), &LogFacts::default());
    assert!(!is_open(&state, Surface::DecisionPane));
    assert!(state.modal_stack.is_empty());
    for c in ['y', 'n'] {
        let action = handle_input(&mut state, &key(c));
        assert!(
            !matches!(
                action,
                InputAction::ImageConfirmAttach
                    | InputAction::ImageConfirmCancel
                    | InputAction::Decision(_)
            ),
            "{c}: {action:?}"
        );
    }
    assert_eq!(state.input_buffer, "yn", "both keys typed as text");
    assert!(
        state.pending_large_image.is_some(),
        "the attachment is untouched"
    );
    assert!(state.decisions.holds(DecisionKind::LargeImage));
}

// ── Ctrl+X q ──────────────────────────────────────────────────────────────────

#[test]
fn k10_ctrl_x_q_focuses_the_oldest_decidable_item_and_says_so_when_none() {
    let (mut state, _c) = clocked(80, 24);
    state.focus = FocusState::Chat;
    handle_input(&mut state, &special(DomainKey::CtrlX));
    assert_eq!(
        handle_input(&mut state, &key('q')),
        InputAction::FocusDecisions
    );
    decision_pane::open_waiting(&mut state, &facts("c1"));
    assert_eq!(
        state.notifications.visible().last().unwrap().message,
        "No decisions waiting"
    );

    // Two waiting items: the oldest takes focus.
    let (mut state, _c) = clocked(80, 24);
    state.input_buffer = "typing".into();
    state.focus = FocusState::Input;
    automatic(&mut state, DecisionKind::Carryover);
    automatic(&mut state, DecisionKind::Budget);
    assert!(!is_open(&state, Surface::DecisionPane));
    assert!(state.modal_stack.is_empty());
    decision_pane::open_waiting(&mut state, &facts("c1"));
    assert_eq!(
        decision_pane::current(&state, &facts("c1")).map(|i| i.kind),
        Some(DecisionKind::Carryover)
    );
    assert!(presentation::consistent(&state));
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::DecisionPane
    );
    assert_eq!(state.input_buffer, "typing", "the draft is untouched");
}

#[test]
fn k10_the_status_row_carries_the_cue_while_items_wait() {
    let (mut state, _c) = clocked(80, 24);
    assert!(!status_row(&state, 80, None).contains("Needs you"));
    state.input_buffer = "typing".into();
    automatic(&mut state, DecisionKind::Carryover);
    automatic(&mut state, DecisionKind::Budget);
    let row = status_row(&state, 80, None);
    assert!(row.contains("Needs you (2) ^X q"), "{row}");
    decision_pane::withdraw(&mut state, DecisionKind::Budget);
    let row = status_row(&state, 80, None);
    assert!(row.contains("Needs you (1) ^X q"), "{row}");
}

/// The real status row with a long model name, token usage, a budget segment and
/// the `Log !N` badge input, drawn the way both hosts call it.
fn long_status_row(state: &TuiState, width: u16, long: bool) -> String {
    use rustain::adapters::tui::state::{DailyBudgetState, LogAwareness};
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::status_bar;
    use rustain::domain::models::visual::DensityMode;
    use rustain::domain::models::{PermissionMode, StatusState, UsageInfo};
    let usage = UsageInfo {
        input_tokens: 123_456,
        output_tokens: 98_765,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: None,
    };
    let budget = DailyBudgetState {
        spent_today_usd: 123_456.78,
        limit_usd: 200_000.0,
        computed_at_ms: 0,
        dismissed_until_unix: 0,
    };
    let model = if long {
        "claude-opus-4-1-20250805-extended-thinking"
    } else {
        "model"
    };
    let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
    let theme = Theme::dark();
    terminal
        .draw(|frame| {
            status_bar::render(
                frame,
                frame.area(),
                model,
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                10,
                PermissionMode::Normal,
                long.then_some(&usage),
                200_000,
                false,
                None,
                false,
                true,
                0,
                None,
                0,
                None,
                None,
                None,
                false,
                long.then_some(&budget),
                None,
                DensityMode::Focus,
                false,
                None,
                LogAwareness::Hidden,
                status_bar::NoticeBadges {
                    unseen: state.notifications.unseen(),
                    decisions: state.decisions.len(),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    row_text(terminal.backend().buffer(), 0)
}

#[test]
fn k10_the_cue_survives_beside_the_log_badge_at_sixty_and_eighty_columns() {
    use unicode_width::UnicodeWidthStr;
    let cue = "Needs you (2) ^X q";
    for width in [60u16, 80] {
        for long in [false, true] {
            let (mut state, _c) = clocked(width, 24);
            state.input_buffer = "typing".into();
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Warning,
                "something to look at",
            );
            automatic(&mut state, DecisionKind::Carryover);
            automatic(&mut state, DecisionKind::Budget);
            let row = long_status_row(&state, width, long);
            let case = format!("{width} cols, long={long}: {row:?}");
            assert_eq!(row.matches(cue).count(), 1, "the cue is whole: {case}");
            // The cue survives; the lower-priority menu tail yields to facts.
            let menu_fits = !long && width == 80;
            let suffix = if !menu_fits {
                cue.to_owned()
            } else {
                format!("{cue} ^X menu")
            };
            assert!(row.trim_end().ends_with(&suffix), "{case}");
            assert_eq!(row.contains("^X menu"), menu_fits, "{case}");
            assert!(row.width() <= usize::from(width), "{case}");
            let badge_at = row.find("Log !1");
            if let Some(at) = badge_at {
                assert!(
                    at < row.find(cue).unwrap(),
                    "the badge never overlaps the cue: {case}"
                );
                assert_eq!(row.matches("Log !").count(), 1, "{case}");
            }
            if !long && width == 80 {
                assert!(badge_at.is_some(), "short row: both fit at 80: {case}");
                assert!(
                    row.contains("model"),
                    "the uncongested model still fits: {case}"
                );
            }
            if long {
                // A8: model/context/budget yield before the state and counters.
                assert!(row.contains("Ready"), "{case}");
                assert!(row.contains("↑123.5k ↓98.8k"), "{case}");
            }
        }
    }
}

#[test]
fn k10_enter_never_resolves_a_delete_through_the_front_door() {
    let (mut state, clock) = clocked(80, 24);
    begin_delete_confirmation(
        &mut state,
        rustain::domain::models::visual::DeleteConfirmTarget::Bulk { count: 3 },
    );
    ready(&mut state, &clock);
    let f = LogFacts::default();
    assert_eq!(
        press(&mut state, special(DomainKey::Enter), &f),
        Some(PaneOutcome::Consumed)
    );
    assert!(state.decisions.holds(DecisionKind::DeleteConversations));
    assert!(is_open(&state, Surface::DecisionPane));
    assert_eq!(state.modal_stack.len(), 1);
    assert!(state.pending_delete.is_some(), "the target is untouched");
    // Positive control: the painted `y` resolves it.
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete))
    );
}

#[test]
fn k10_a_promoted_item_gets_its_own_inert_window_so_a_double_press_answers_once() {
    let (mut state, clock) = clocked(80, 24);
    decision_pane::raise(
        &mut state,
        DecisionDraft::new(DecisionKind::Budget, "b"),
        Arrival::Requested,
    );
    automatic(&mut state, DecisionKind::LargeImage);
    let f = LogFacts::default();
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Effect(DecisionEffect::BudgetContinue))
    );
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Consumed),
        "the promoted image has not painted yet: the second `y` is dropped"
    );
    assert!(state.decisions.holds(DecisionKind::LargeImage));
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Effect(DecisionEffect::AttachLargeImage))
    );
}

#[test]
fn k10_an_item_brought_to_focus_over_a_painted_one_gets_its_own_inert_window() {
    let f = LogFacts::default();
    // The user asks to delete while a budget decision is focused and past its
    // window: the `y` meant for the budget must not confirm the delete.
    let (mut state, clock) = clocked(80, 24);
    decision_pane::raise(
        &mut state,
        DecisionDraft::new(DecisionKind::Budget, "b"),
        Arrival::Requested,
    );
    ready(&mut state, &clock);
    begin_delete_confirmation(
        &mut state,
        rustain::domain::models::visual::DeleteConfirmTarget::Bulk { count: 3 },
    );
    assert_eq!(
        decision_pane::current(&state, &f).map(|i| i.kind),
        Some(DecisionKind::DeleteConversations)
    );
    for k in [key('y'), special(DomainKey::Enter)] {
        assert_eq!(
            press(&mut state, k.clone(), &f),
            Some(PaneOutcome::Consumed),
            "the delete has not painted yet: {k:?} is dropped"
        );
    }
    assert!(state.decisions.holds(DecisionKind::DeleteConversations));
    assert!(state.pending_delete.is_some());
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete)),
        "positive control after its own paint and 500 ms"
    );

    // `Ctrl+X q` refocuses the oldest (deferred) item over a painted one.
    let (mut state, clock) = clocked(80, 24);
    automatic(&mut state, DecisionKind::LargeImage);
    ready(&mut state, &clock);
    press(&mut state, special(DomainKey::Esc), &f);
    assert!(
        !is_open(&state, Surface::DecisionPane),
        "Later released the pane"
    );
    assert!(state.modal_stack.is_empty());
    decision_pane::raise(
        &mut state,
        DecisionDraft::new(DecisionKind::Budget, "b"),
        Arrival::Requested,
    );
    ready(&mut state, &clock);
    decision_pane::open_waiting(&mut state, &f);
    assert_eq!(
        decision_pane::current(&state, &f).map(|i| i.kind),
        Some(DecisionKind::LargeImage)
    );
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Consumed),
        "the refocused image has not painted yet"
    );
    assert!(state.decisions.holds(DecisionKind::LargeImage));
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, key('y'), &f),
        Some(PaneOutcome::Effect(DecisionEffect::AttachLargeImage))
    );
}

#[test]
fn k10_closing_one_tab_or_clearing_its_warning_never_touches_another_tabs_decision() {
    let (mut state, _c) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 92);
    handle_upsert_context_warning(&mut state, "c2", 93);
    assert_eq!(state.decisions.len(), 2);
    // Compaction finished (or the level dropped) in c1: only c1's item clears.
    rustain::adapters::tui::handlers::context_warning::handle_clear_context_warning(
        &mut state, "c1",
    );
    assert_eq!(state.decisions.len(), 1);
    assert!(
        state
            .decisions
            .items()
            .all(|i| i.conversation_id.as_deref() == Some("c2")),
        "c2's warning is still pending"
    );
    // c2's tab is then closed: the host's draw prunes the orphan.
    let remaining = LogFacts {
        titles: vec![("c3".into(), "three".into())],
        ..facts("c3")
    };
    decision_pane::prune_closed(&mut state, &remaining);
    assert!(state.decisions.is_empty());
    assert_eq!(decision_pane::waiting(&state), 0);
    let row = status_row(&state, 80, None);
    assert!(!row.contains("Needs you"), "{row}");
}

// ── geometry: bottom-anchored over the chat pane, nothing else moves ─────────

#[test]
fn k10_the_pane_paints_inside_the_chat_pane_and_changes_no_other_cell() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let (mut state, _c) = clocked(width, height);
        decision_pane::raise(
            &mut state,
            DecisionDraft::new(
                DecisionKind::Budget,
                "Daily budget ($10.00) reached. A longer explanation follows so the frame has to wrap.",
            ),
            Arrival::Requested,
        );
        let layout = layout_of(&state, width, height);
        let f = LogFacts::default();
        let without = draw_pane(&state, &f, width, height, false);
        let with = draw_pane(&state, &f, width, height, true);
        let pane = layout.chat_pane;
        for y in 0..height {
            for x in 0..width {
                let inside =
                    Rect::new(pane.x, pane.y, pane.width, pane.height).contains((x, y).into());
                if !inside {
                    assert_eq!(
                        without.cell((x, y)).map(|c| c.symbol().to_owned()),
                        with.cell((x, y)).map(|c| c.symbol().to_owned()),
                        "{width}x{height}: ({x},{y}) outside the chat pane changed"
                    );
                    if layout.input_area.contains((x, y).into()) {
                        assert!(
                            with[(x, y)].modifier.contains(Modifier::DIM),
                            "{width}x{height}: composer ({x},{y}) must dim without changing text"
                        );
                    }
                    if layout.status_bar.contains((x, y).into()) {
                        assert_eq!(without[(x, y)], with[(x, y)], "status is unchanged");
                    }
                }
            }
        }
        let text = screen(&with);
        assert!(text.contains("Daily budget"), "{width}x{height}:\n{text}");
        assert!(text.contains("[y] Continue"), "{width}x{height}:\n{text}");
        assert!(text.contains("[Esc] Later"), "{width}x{height}:\n{text}");
        // Bottom-anchored: the frame's last row is the chat pane's last row.
        let last = row_text(&with, pane.bottom() - 1);
        assert!(
            last.contains('└') || last.contains('┘'),
            "{width}x{height}: {last}"
        );
    }
}

// ── producers and S-LOG K / n (AC9, AC11) ───────────────────────────────────

#[test]
fn k10_the_context_warning_producer_raises_a_decision_and_a_log_record() {
    let (mut state, _c) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 92);
    assert_eq!(
        decision_pane::current(&state, &facts("c1")).map(|i| i.kind),
        Some(DecisionKind::ContextWarning)
    );
    let record = state.notifications.records().last().expect("a log record");
    assert_eq!(record.action, Some(NoticeAction::ContextDecision));
    assert_eq!(record.conversation_id.as_deref(), Some("c1"));
    assert_eq!(record.severity, NoticeSeverity::Warning);
    handle_upsert_context_warning(&mut state, "c1", 96);
    assert_eq!(
        state.notifications.records().last().unwrap().severity,
        NoticeSeverity::Error,
        "95 % and over is an Error"
    );
}

#[test]
fn k10_slog_k_and_n_raise_decisions_and_never_act() {
    let (mut state, clock) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 92);
    ready(&mut state, &clock);
    // Later: the item waits, unfocused.
    press(&mut state, special(DomainKey::Esc), &facts("c1"));
    assert!(!is_open(&state, Surface::DecisionPane));
    assert!(state.modal_stack.is_empty());

    // `K` on the ctxwarn record raises/focuses the Compact decision.
    notification_log::open(&mut state);
    let outcome = notification_log::handle_domain_event(&mut state, &key('K'), &facts("c1"));
    assert_eq!(
        outcome,
        Some(LogOutcome::Closed),
        "K acts on nothing by itself"
    );
    assert!(state.notification_log.is_none(), "S-LOG closed");
    assert_eq!(
        decision_pane::current(&state, &facts("c1")).map(|i| i.kind),
        Some(DecisionKind::ContextWarning)
    );

    // `n` on the same record raises the carryover question.
    notification_log::open(&mut state);
    let outcome = notification_log::handle_domain_event(&mut state, &key('n'), &facts("c1"));
    assert_eq!(outcome, Some(LogOutcome::Closed));
    assert!(state.decisions.holds(DecisionKind::Carryover));
    assert_eq!(
        decision_pane::current(&state, &facts("c1")).map(|i| i.kind),
        Some(DecisionKind::Carryover)
    );
}

#[test]
fn k10_slog_k_and_n_are_offered_only_where_eligible_and_c_is_never_compact() {
    let (mut state, _c) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 92);
    let pending = state.decisions.len();
    let cases: [(&str, LogFacts); 4] = [
        ("another conversation is active", facts("c2")),
        (
            "the warning level cleared",
            LogFacts {
                context_warning: false,
                ..facts("c1")
            },
        ),
        (
            "the attached client has no pane",
            LogFacts {
                decisions_supported: false,
                ..facts("c1")
            },
        ),
        ("no active conversation", LogFacts::default()),
    ];
    for (why, f) in cases {
        notification_log::open(&mut state);
        for c in ['K', 'n'] {
            let outcome = notification_log::handle_domain_event(&mut state, &key(c), &f);
            assert_eq!(outcome, Some(LogOutcome::Consumed), "{why}: {c}");
        }
        assert!(state.notification_log.is_some(), "{why}: S-LOG stays open");
        assert_eq!(state.decisions.len(), pending, "{why}: nothing was raised");
        notification_log::close(&mut state);
    }

    // `c` copies; it is never Compact (RU-21).
    notification_log::open(&mut state);
    match notification_log::handle_domain_event(&mut state, &key('c'), &facts("c1")) {
        Some(LogOutcome::Copy(text)) => assert!(text.contains("Running low on context")),
        other => panic!("c must copy, got {other:?}"),
    }

    // A compaction failure of the active conversation: `n` raises the carryover.
    notification_log::close(&mut state);
    notifications::notify_draft(
        &mut state,
        NoticeDraft::new(
            NoticeSource::Compaction,
            NoticeSeverity::Error,
            "Compaction failed: x",
        )
        .conversation("c1")
        .action(NoticeAction::RetryCompaction),
    );
    notification_log::open(&mut state);
    let outcome = notification_log::handle_domain_event(&mut state, &key('n'), &facts("c1"));
    assert_eq!(outcome, Some(LogOutcome::Closed));
    assert!(state.decisions.holds(DecisionKind::Carryover));
}

#[test]
fn k10_slog_paints_k_and_n_only_on_an_eligible_record() {
    let (mut state, _c) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 92);
    notification_log::open(&mut state);
    let theme = state.theme.clone();
    let paint = |state: &TuiState, f: &LogFacts| -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                let log = state.notification_log.as_ref().unwrap();
                notification_log::render(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    log,
                    &state.notifications,
                    &theme,
                    f,
                );
            })
            .unwrap();
        screen(terminal.backend().buffer())
    };
    let eligible = paint(&state, &facts("c1"));
    assert!(eligible.contains("[K] Compact…"), "{eligible}");
    assert!(eligible.contains("[n] Start fresh…"), "{eligible}");
    let other = paint(&state, &facts("c2"));
    assert!(!other.contains("[K]") && !other.contains("[n]"), "{other}");
}

// ── the delete producer ───────────────────────────────────────────────────────

#[test]
fn k10_the_delete_producer_takes_the_pane_even_over_a_typing_composer() {
    // The user's own `d`/`y` request is not an arrival to protect against.
    let (mut state, clock) = clocked(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer = "draft".into();
    begin_delete_confirmation(
        &mut state,
        rustain::domain::models::visual::DeleteConfirmTarget::Bulk { count: 3 },
    );
    assert!(is_open(&state, Surface::DecisionPane));
    assert!(presentation::consistent(&state));
    assert_eq!(state.modal_stack.len(), 1);
    assert_eq!(state.input_buffer, "draft");
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, key('n'), &LogFacts::default()),
        Some(PaneOutcome::Effect(DecisionEffect::CancelDelete))
    );
}

// ── S-QUIT stays above the pane ───────────────────────────────────────────────

#[test]
fn k10_ctrl_c_backs_out_and_ctrl_q_inside_the_pane_requests_the_guard() {
    let (mut state, clock) = clocked(80, 24);
    automatic(&mut state, DecisionKind::Carryover);
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, special(DomainKey::CtrlC), &LogFacts::default()),
        Some(PaneOutcome::Consumed),
        "RU-20: Ctrl+C backs out rather than requesting quit"
    );
    assert!(!is_open(&state, Surface::DecisionPane));
    assert!(state.modal_stack.is_empty());
    assert!(state.decisions.holds(DecisionKind::Carryover));
    assert!(state.decisions.items().next().unwrap().deferred);

    decision_pane::open_waiting(&mut state, &LogFacts::default());
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, special(DomainKey::CtrlQ), &LogFacts::default()),
        Some(PaneOutcome::Quit)
    );
    assert!(is_open(&state, Surface::DecisionPane));
    assert!(
        state.decisions.holds(DecisionKind::Carryover),
        "the guard decides"
    );
    let pane = state.modal_stack.top().unwrap().clone();
    let mut f = QuitFacts::idle(QuitHost::Local);
    f.composer_draft = true;
    quit_confirm::request(&mut state, f);
    assert!(presentation::consistent(&state));
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::QuitConfirm
    );
    assert!(is_open(&state, Surface::DecisionPane));
    assert_eq!(automatic(&mut state, DecisionKind::Budget), Raised::Waiting);
    assert_eq!(handle_input(&mut state, &key('n')), InputAction::Consumed);
    assert_eq!(
        state.modal_stack.top(),
        Some(&pane),
        "Cancel returns to the pane"
    );
    assert!(presentation::consistent(&state));
}
