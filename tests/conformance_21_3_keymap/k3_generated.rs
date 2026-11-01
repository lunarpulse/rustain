//! K3 — generated discovery surfaces (AC3, AC4, KG-1): the which-key menu
//! painted at 60/80/130 columns for both hosts, `(n/a)` exactly where the
//! registry says a chord cannot run, and the menu order rule. Help, palette and
//! status-row legs live in the in-file `story21_3_` tests next to their renderers
//! (`keymap/render_help.rs`, `palette_registry.rs`, `status_bar.rs`, `hints.rs`).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rustain::adapters::tui::keymap::table::{
    ATTACHED_PANEL, ATTACHED_PROFILE, ATTACHED_SIGN_IN, PANEL_NARROW,
};
use rustain::adapters::tui::keymap::{AvailCtx, Key};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::state::{TuiState, WhichKeyState};
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

use super::support::{fresh, press};

fn paint(width: u16, height: u16, avail: &AvailCtx) -> String {
    let mut wk = WhichKeyState::new();
    wk.open(FocusState::Input);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| which_key_bar::render(frame, frame.area(), &wk, &Theme::dark(), avail))
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
    assert!(ATTACHED_PANEL.contains("/team status"));
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

/// AC4/KG-2: `^X menu` is painted exactly where `Ctrl+X` opens the leader, in
/// every overlay that can take focus (one predicate drives both).
#[test]
fn status_tail_menu_iff_ctrl_x_opens_leader() {
    let overlay = |o: OverlayType| FocusState::Overlay(o);
    let foci = [
        ("Input", FocusState::Input),
        ("Chat", FocusState::Chat),
        ("Help", overlay(OverlayType::Help)),
        ("CommandPalette", overlay(OverlayType::CommandPalette)),
        ("ModelSelector", overlay(OverlayType::ModelSelector)),
        ("ProfileSwitcher", overlay(OverlayType::ProfileSwitcher)),
        ("Search", overlay(OverlayType::Search)),
        ("CrossSearch", overlay(OverlayType::CrossSearch)),
        ("BookmarkList", overlay(OverlayType::BookmarkList)),
        ("UsagePanel", overlay(OverlayType::UsagePanel)),
        ("WaveOverlay", overlay(OverlayType::WaveOverlay)),
        (
            "Confirmation",
            overlay(OverlayType::Confirmation(ConfirmationType::Fork)),
        ),
    ];
    let (mut opened, mut blocked) = (0, 0);
    for (name, focus) in foci {
        let mut state = fresh();
        state.focus = focus;
        let painted = status_tail(&state) == StatusTail::Menu;
        press(&mut state, Key::Ctrl('x'));
        assert_eq!(
            painted, state.which_key.active,
            "{name}: ^X menu iff Ctrl+X opens"
        );
        if painted { opened += 1 } else { blocked += 1 }
    }
    assert_eq!(
        (opened, blocked),
        (2, 10),
        "Input and Chat open; every overlay blocks"
    );
}

/// R5: the generated Chat tip stays short enough to show beside the status row.
#[test]
fn chat_tip_is_at_most_60_cells() {
    for host in [Host::Local, Host::Attached] {
        let tip = tip(Context::Chat, host, false).expect("a Chat tip");
        assert!(
            tip.width() <= 60,
            "{host:?}: `{tip}` is {} cells",
            tip.width()
        );
        assert!(tip.contains("i type"), "{host:?}: the tip names `i`");
    }
}
