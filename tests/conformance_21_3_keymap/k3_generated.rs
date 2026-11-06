//! K3 — generated discovery surfaces (AC3, AC4, KG-1): the which-key menu
//! painted at 60/80/130 columns for both hosts, `(n/a)` exactly where the
//! registry says a chord cannot run, and the menu order rule. Help, palette and
//! status-row legs live in the in-file `story21_3_` tests next to their renderers
//! (`keymap/render_help.rs`, `palette_registry.rs`, `status_bar.rs`, `hints.rs`).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rustain::adapters::tui::keymap::table::{
    ATTACHED_PROFILE, ATTACHED_SIGN_IN, PANEL_NARROW, attached_panel_remedy,
};
use rustain::adapters::tui::keymap::{AvailCtx, Key};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::which_key_bar;
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::DensityMode;

use rustain::adapters::tui::app::status_tail;
use rustain::adapters::tui::keymap::render::StatusTail;
use rustain::adapters::tui::keymap::render_tips::tip;
use rustain::adapters::tui::keymap::{Context, Host};
use rustain::domain::models::visual::{ConfirmationType, OverlayType};
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
    // Local, 80 columns: every sidebar-panel chord is (n/a); the overlays are not.
    let narrow = paint(80, 12, &AvailCtx::local(80, DensityMode::Focus));
    for painted in [
        "[A] Adapters (n/a)",
        "[S] Agents (n/a)",
        "[L] A2A log (n/a)",
        "[R] Room (n/a)",
        "[E] Artifacts (n/a)",
        "[T] Tasks (n/a)",
        "[H] History (n/a)",
    ] {
        assert!(
            narrow.contains(painted),
            "80 cols: `{painted}` missing:\n{narrow}"
        );
    }
    for painted in [
        "[P] Profile",
        "[M] Model",
        "[U] Usage",
        "[W] Monitor",
        "[?] Help",
    ] {
        assert!(narrow.contains(painted), "{painted}:\n{narrow}");
        assert!(
            !narrow.contains(&format!("{painted} (n/a)")),
            "{painted} is available:\n{narrow}"
        );
    }
    // Local, 130 columns: panels are available.
    let wide = paint(130, 12, &AvailCtx::local(130, DensityMode::Monitor));
    assert!(
        wide.contains("[H] History") && !wide.contains("(n/a)"),
        "{wide}"
    );
    // Attached: the local-only chords are (n/a); detach, log, help and the log keep working.
    let attached = paint(130, 12, &AvailCtx::attached(130, false));
    for painted in [
        "[P] Profile (n/a)",
        "[M] Model (n/a)",
        "[I] Sign in (n/a)",
        "[A] Adapters (n/a)",
        "[W] Monitor (n/a)",
        "[U] Usage (n/a)",
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
    ] {
        assert!(attached.contains(painted), "{painted}:\n{attached}");
        assert!(
            !attached.contains(&format!("{painted} (n/a)")),
            "{painted}:\n{attached}"
        );
    }
    assert!(!attached.contains("[F] Focus") && !narrow.contains("[D] Dashboard"));

    // Pressing an (n/a) chord posts exactly its remedy (never a silent no-op).
    let mut state = TuiState::new(80, 24);
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('h'));
    let notices: Vec<(NoticeSeverity, String)> = state
        .notifications
        .records()
        .map(|r| (r.severity, r.message.clone()))
        .collect();
    assert_eq!(
        notices,
        vec![(NoticeSeverity::Info, PANEL_NARROW.to_string())]
    );
    // The attached remedies are the registry's (what the attached host posts).
    assert!(
        ATTACHED_PROFILE.contains("rustain profile")
            && ATTACHED_SIGN_IN.contains("rustain auth login")
    );
    assert!(attached_panel_remedy().contains("/team status"));
    assert!(attached_panel_remedy().contains("Detach (Ctrl+X, D)"));
}

#[test]
fn menu_keeps_the_relative_order_of_21_1_and_21_2() {
    for avail in [
        AvailCtx::attached(130, false),
        AvailCtx::local(130, DensityMode::Monitor),
    ] {
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
    let overlay = |o: OverlayType| FocusState::Overlay(o);
    let foci = [
        ("Input", FocusState::Input),
        ("Chat", FocusState::Chat),
        ("Help", FocusState::Chat),
        ("CommandPalette", FocusState::Input),
        ("ModelSelector", FocusState::Input),
        ("ProfileSwitcher", FocusState::Input),
        ("ReverseSearch", FocusState::Input),
        ("Search", FocusState::Input),
        (
            "CrossSearch",
            FocusState::Sidebar {
                panel: rustain::domain::models::visual::PanelType::History,
                selected: 0,
            },
        ),
        ("BookmarkList", FocusState::Chat),
        ("UsagePanel", FocusState::Input),
        ("WaveOverlay", FocusState::Chat),
        (
            "Confirmation",
            overlay(OverlayType::Confirmation(ConfirmationType::Fork)),
        ),
        ("Login", FocusState::Input),
    ];
    let (mut opened, mut blocked) = (0, 0);
    for (name, focus) in foci {
        let mut state = fresh();
        state.focus = focus.clone();
        if name == "CrossSearch" {
            state = cross_search_state();
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
        press(&mut state, Key::Ctrl('x'));
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
        (13, 1),
        "Input, Chat, ReverseSearch popup and migrated frames open; only Confirmation blocks"
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
    use rustain::adapters::tui::keymap::table_nav::{BookmarkCmd, SidebarCmd};
    use rustain::adapters::tui::keymap::table_overlay::OverlayCmd as O;
    use rustain::adapters::tui::keymap::table_panels::PanelCmd as P;
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
        SidebarCmd::FocusChat.into(),
        SidebarCmd::SelectNext.into(),
        SidebarCmd::SelectPrev.into(),
        BookmarkCmd::Jump.into(),
        BookmarkCmd::Undo.into(),
        BookmarkCmd::Close.into(),
        P::LogNext.into(),
        P::LogPrev.into(),
        P::LogDrill.into(),
        P::LogSearch.into(),
        P::LogExport.into(),
        P::LogTail.into(),
        P::WaveNext.into(),
        P::WavePrev.into(),
        P::WaveRerun.into(),
        P::WaveDiverge.into(),
        P::WaveClose.into(),
        P::DrillToggleExpand.into(),
        P::DrillScrollDown.into(),
        P::DrillScrollUp.into(),
        P::DrillCopy.into(),
        P::DrillClose.into(),
        P::DrillRetry.into(),
        P::DrillSkip.into(),
        P::DrillEdit.into(),
        P::DrillPause.into(),
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
