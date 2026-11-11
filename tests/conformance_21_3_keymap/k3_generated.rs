//! K3 — generated discovery surfaces (AC3, AC4, KG-1): the which-key menu
//! painted at 60/80/130 columns for both hosts, `(n/a)` exactly where the
//! registry says a chord cannot run, and the menu order rule. Help, palette and
//! status-row legs live in the in-file `story21_3_` tests next to their renderers
//! (`keymap/render_help.rs`, `palette_registry.rs`, `status_bar.rs`, `hints.rs`).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rustain::adapters::tui::keymap::table::{
    ATTACHED_PROFILE, ATTACHED_SIGN_IN, attached_mission_remedy, attached_panel_remedy,
};
use rustain::adapters::tui::keymap::{AvailCtx, Key};
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::which_key_bar;
use rustain::domain::models::FocusState;

use rustain::adapters::tui::app::status_tail;
use rustain::adapters::tui::keymap::render::StatusTail;
use rustain::adapters::tui::keymap::render_tips::tip;
use rustain::adapters::tui::keymap::{Context, Host};
use unicode_width::UnicodeWidthStr;

use super::k4_overlays::cross_search_state;
use super::support::{fresh, press};

fn paint(width: u16, height: u16, avail: &AvailCtx) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            let _ = which_key_bar::render(frame, frame.area(), &Theme::dark(), avail, None);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn position(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` missing from\n{text}"))
}

#[test]
fn width_and_host_unavailable_chords_are_qualified_and_answer_with_remedy() {
    // Local, 80 columns: only the monitor chord is (n/a) (Room, A2A log, Artifacts, Adapters,
    // Agents, Tasks and History all open Mission Control tabs or frames); the overlays are not.
    // The Room chord (21-7, 7-A) opens the Mission Control Room tab at every width: available.
    let narrow = paint(80, 12, &AvailCtx::local(80));
    assert!(
        narrow.contains("[R] Room") && !narrow.contains("[R] Room (n/a)"),
        "80 cols: the Room chord is available:\n{narrow}"
    );
    // The A2A log (Messages) and Artifacts chords (21-7, 7-B) open Mission Control tabs too:
    // available at 80 columns, never `(n/a)` there.
    for painted in ["[L] A2A log", "[E] Artifacts"] {
        assert!(
            narrow.contains(painted) && !narrow.contains(&format!("{painted} (n/a)")),
            "80 cols: `{painted}` is available:\n{narrow}"
        );
    }
    for painted in ["[A] Adapters", "[S] Agents"] {
        assert!(
            narrow.contains(painted) && !narrow.contains(&format!("{painted} (n/a)")),
            "80 cols: `{painted}` is available (21-7 7-C opens a frame at every width):\n{narrow}"
        );
    }
    // The Tasks chord (21-7, 7-D) opens the Mission Control Tasks tab at every width.
    assert!(
        narrow.contains("[T] Tasks") && !narrow.contains("[T] Tasks (n/a)"),
        "80 cols: the Tasks chord is available:\n{narrow}"
    );
    assert!(
        narrow.contains("[H] History") && !narrow.contains("[H] History (n/a)"),
        "80 cols: the History chord is available (a frame at every width):\n{narrow}"
    );
    for painted in ["[P] Profile", "[M] Model", "[U] Usage", "[?] Help"] {
        assert!(narrow.contains(painted), "{painted}:\n{narrow}");
        assert!(
            !narrow.contains(&format!("{painted} (n/a)")),
            "{painted} is available:\n{narrow}"
        );
    }
    // The monitor chord needs a terminal of 120 columns (21-7 7-F, A16): `(n/a)` at 80 and 119,
    // available at 120 and 130 (the control below).
    assert!(
        narrow.contains("[W] Monitor (n/a)"),
        "80 cols: the monitor chord names its width floor:\n{narrow}"
    );
    let floor = paint(119, 12, &AvailCtx::local(119));
    assert!(floor.contains("[W] Monitor (n/a)"), "119 cols:\n{floor}");
    let at_floor = paint(120, 12, &AvailCtx::local(120));
    assert!(
        at_floor.contains("[W] Monitor") && !at_floor.contains("[W] Monitor (n/a)"),
        "120 cols:\n{at_floor}"
    );
    // Local, 130 columns: panels are available.
    let wide = paint(130, 12, &AvailCtx::local(130));
    assert!(
        wide.contains("[H] History") && !wide.contains("(n/a)"),
        "{wide}"
    );
    // Attached: the local-only chords are (n/a); detach, log, help and the log keep working.
    let attached = paint(200, 12, &AvailCtx::attached(130, false));
    for painted in [
        "[P] Profile (n/a)",
        "[M] Model (n/a)",
        "[I] Sign in (n/a)",
        "[A] Adapters (n/a)",
        "[W] Monitor (n/a)",
        "[U] Usage (n/a)",
        "[S] Agents (n/a)",
        "[T] Tasks (n/a)",
        "[H] History (n/a)",
        "[R] Room (n/a)",
        "[E] Artifacts (n/a)",
    ] {
        assert!(
            attached.contains(painted),
            "attached: `{painted}`:\n{attached}"
        );
    }
    for painted in [
        "[D] Detach",
        "[L] A2A log",
        "[?] Help",
        "[N] Notifications",
        "[X] Clear toasts",
        "[Q] Decisions",
    ] {
        assert!(attached.contains(painted), "{painted}:\n{attached}");
        assert!(
            !attached.contains(&format!("{painted} (n/a)")),
            "{painted}:\n{attached}"
        );
    }
    assert!(!attached.contains("[F] Focus") && !narrow.contains("[D] Dashboard"));

    // `Ctrl+X h` is no longer width-qualified (21-7 7-E: the History window is a frame at every
    // local width): it asks for the window at 80 columns and posts no refusal.
    let mut state = TuiState::new(80, 24);
    press(&mut state, Key::Ctrl('x'));
    assert_eq!(
        press(&mut state, Key::Char('h')),
        rustain::adapters::tui::app::InputAction::OpenHistory
    );
    assert_eq!(state.notifications.records().count(), 0);
    // The attached remedies are the registry's (what the attached host posts).
    assert!(
        ATTACHED_PROFILE.contains("rustain profile")
            && ATTACHED_SIGN_IN.contains("rustain auth login")
    );
    assert!(attached_panel_remedy().contains("/team status"));
    assert!(attached_panel_remedy().contains("Detach (Ctrl+X, D)"));
    // The attached refusal of every Mission Control chord and local panel (A20) is one registry
    // text: Messages only, with the detach chord named.
    assert!(attached_mission_remedy().contains("Messages only"));
    assert!(attached_mission_remedy().contains("Detach (Ctrl+X, D)"));
}

#[test]
fn decisions_help_names_the_queue_for_both_hosts() {
    use rustain::adapters::tui::keymap::table::LeaderCmd;
    use rustain::adapters::tui::keymap::{Availability, CommandId, Resolved, resolve};
    let help = rustain::adapters::tui::keymap::render_help::help_rows();
    let row = help.iter().find(|row| row.key == "Ctrl+X, Q").unwrap();
    assert_eq!(
        row.description,
        "Decisions: open the Needs-you queue (status cue: Needs you)"
    );
    for (host, avail) in [
        (Host::Local, AvailCtx::local(130)),
        (Host::Attached, AvailCtx::attached(130, false)),
    ] {
        let Resolved::Hit(row) = resolve(Context::Leader, host, &[Key::Char('q')]) else {
            panic!("{host:?} has no Decisions row");
        };
        assert_eq!(row.command, CommandId::Leader(LeaderCmd::Decisions));
        assert_eq!((row.avail)(&avail), Availability::Available);
    }
}

#[test]
fn menu_keeps_the_relative_order_of_21_1_and_21_2() {
    for avail in [AvailCtx::attached(130, false), AvailCtx::local(130)] {
        let text = paint(200, 6, &avail);
        let help = position(&text, "[?] Help");
        let notifications = position(&text, "[N] Notifications");
        let clear = position(&text, "[X] Clear toasts");
        let send = position(&text, "[Enter] Send");
        let cancel = position(&text, "[Esc] Cancel");
        assert!(
            help < notifications && notifications < clear && clear < send && send < cancel,
            "{text}"
        );
        if avail.host == rustain::adapters::tui::keymap::Host::Attached {
            assert!(position(&text, "[D] Detach") < help, "{text}");
        } else {
            assert!(
                !text.contains("[D] Detach"),
                "local `d` is retired:\n{text}"
            );
        }
    }
}

/// AC4/KG-2: the tail is offered iff Ctrl+X opens the leader. Painting may
/// omit it whole when status facts need the width (status_bar witnesses).
#[test]
fn status_tail_menu_offered_iff_ctrl_x_opens_leader() {
    let foci = [
        ("Input", FocusState::Input),
        ("Chat", FocusState::Chat),
        ("Help", FocusState::Chat),
        ("CommandPalette", FocusState::Input),
        ("ModelSelector", FocusState::Input),
        ("ProfileSwitcher", FocusState::Input),
        ("ReverseSearch", FocusState::Input),
        ("Search", FocusState::Input),
        ("CrossSearch", FocusState::Input),
        ("BookmarkList", FocusState::Chat),
        ("UsagePanel", FocusState::Input),
        ("WaveOverlay", FocusState::Chat),
        ("Confirmation", FocusState::Input),
        ("Login", FocusState::Input),
    ];
    let (mut opened, mut blocked) = (0, 0);
    for (name, focus) in foci {
        let mut state = fresh();
        state.focus = focus.clone();
        if name == "CrossSearch" {
            state = cross_search_state();
        } else if name == "Confirmation" {
            rustain::adapters::tui::decision_queue::raise_fork(&mut state, "c1", 0, "preview", 0);
        } else if name == "Help" {
            press(&mut state, Key::Char('?'));
        } else if name == "CommandPalette" {
            press(&mut state, Key::Ctrl('p'));
        } else if name == "ReverseSearch" {
            press(&mut state, Key::Ctrl('r'));
            assert!(state.reverse_search.is_open());
            assert_eq!(state.focus, FocusState::Input);
            assert!(state.modal_stack.is_empty());
        } else if let Some(surface) = match name {
            "ModelSelector" => Some(Surface::ModelSelector),
            "ProfileSwitcher" => Some(Surface::ProfileSwitcher),
            "UsagePanel" => Some(Surface::UsagePanel),
            "Login" => Some(Surface::Login),
            "Search" => Some(Surface::Search),
            "BookmarkList" => Some(Surface::BookmarkList),
            "WaveOverlay" => Some(Surface::WaveOverlay),
            _ => None,
        } {
            reducer::open(&mut state, surface);
        }
        let cross_frame = (name == "CrossSearch").then(|| state.modal_stack.top().unwrap().clone());
        let offered = status_tail(&state) == StatusTail::Menu;
        if name == "Confirmation" {
            crate::conformance_21_6_decision_queue::raisers::press(&mut state, Key::Ctrl('x'));
            assert!(
                state
                    .decisions
                    .holds(rustain::adapters::tui::decision_queue::DecisionKind::Fork)
            );
        } else {
            press(&mut state, Key::Ctrl('x'));
        }
        if let Some(frame) = cross_frame {
            assert_eq!(state.focus, focus);
            assert_eq!(state.modal_stack.top(), Some(&frame));
        }
        assert_eq!(
            offered,
            state.pending_leader.is_some(),
            "{name}: ^X menu offered iff Ctrl+X opens"
        );
        if offered { opened += 1 } else { blocked += 1 }
    }
    assert_eq!(
        (opened, blocked),
        (14, 0),
        "Input, Chat, ReverseSearch popup and decision frames open the leader"
    );
}

/// R5: the generated Chat tip stays short enough to show beside the status row.
#[test]
fn chat_tip_is_at_most_60_cells() {
    for host in [Host::Local, Host::Attached] {
        let tip = tip(Context::Chat, host, false, false).expect("a Chat tip");
        assert!(
            tip.width() <= 60,
            "{host:?}: `{tip}` is {} cells",
            tip.width()
        );
        assert!(tip.contains("i type"), "{host:?}: the tip names `i`");
    }
}

/// KG-1: `key_text` answers `?` for a command with no row, so every command a generated
/// footer, title or notice names must have one.
#[test]
fn every_command_a_generated_hint_names_has_a_row() {
    use rustain::adapters::tui::keymap::CommandId;
    use rustain::adapters::tui::keymap::render::{key_text, key_text_at};
    use rustain::adapters::tui::keymap::table_mission::MissionCmd as M;
    use rustain::adapters::tui::keymap::table_nav::BookmarkCmd;
    use rustain::adapters::tui::keymap::table_overlay::OverlayCmd as O;
    use rustain::adapters::tui::keymap::table_panels::PanelCmd as P;
    use rustain::adapters::tui::presentation::viewer::ViewerCmd;
    let commands: Vec<CommandId> = vec![
        O::PaneYes.into(),
        O::PaneNo.into(),
        O::CrossOpen.into(),
        O::CrossClose.into(),
        O::ModelBack.into(),
        O::ModelSelect.into(),
        O::ModelWarnYes.into(),
        O::ModelWarnNo.into(),
        O::ProfileYes.into(),
        O::ProfileBack.into(),
        M::HistoryOpen.into(),
        M::HistoryDelete.into(),
        M::HistorySearch.into(),
        CommandId::Viewer(ViewerCmd::LineDown),
        CommandId::Viewer(ViewerCmd::LineUp),
        BookmarkCmd::Jump.into(),
        BookmarkCmd::Undo.into(),
        BookmarkCmd::Close.into(),
        P::WaveNext.into(),
        P::WavePrev.into(),
        P::WaveRerun.into(),
        P::WaveDiverge.into(),
        P::WaveClose.into(),
        M::Pause.into(),
        M::Skip.into(),
        M::Kill.into(),
        M::Retry.into(),
        M::Edit.into(),
        M::CopyResult.into(),
        M::ReorderDown.into(),
        M::ReorderUp.into(),
        M::ReorderCommit.into(),
        M::ReorderCancel.into(),
    ];
    for command in commands {
        assert_ne!(key_text(command), "?", "{command:?} has no row");
    }
    for index in 0..2 {
        assert_ne!(
            key_text_at(BookmarkCmd::Delete, index),
            "?",
            "Delete #{index}"
        );
    }
}
