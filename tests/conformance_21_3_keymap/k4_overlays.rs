//! Story 21.3 conformance leg K4 (overlay contexts): every own row of Help,
//! CommandPalette, ModelSelector, ProfileSwitcher, UsagePanel, CrossSearch,
//! ReverseSearch, Autocomplete, NotificationLog, QuitConfirm, DecisionPane,
//! RecipientSurface and Login BY VALUE through the real local front door
//! (`app::handle_input`; S-LOG and the decision pane through the host-neutral
//! handler the event loop calls first), plus the harness's negative sweep.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_pane::{
    self, Arrival, DecisionDraft, DecisionKind, INERT_AFTER_PAINT,
};
use rustain::adapters::tui::keymap::resolve::rows_of;
use rustain::adapters::tui::keymap::{Context, Host, Key};
use rustain::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use rustain::adapters::tui::notifications::{
    self, NoticeAction, NoticeDraft, NoticeSeverity, NoticeSource, NotificationStore,
};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface, is_open};
use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost};
use rustain::adapters::tui::state::{
    ContextWarning, LoginOrigin, LoginSection, OverlayMode, ProviderColumn, RecipientSurface,
    RecipientSurfaceAccess, RecipientSurfaceTarget, RunningLogin, TuiState,
};
use rustain::domain::clock::MockClock;
use rustain::domain::events::LoginTargetRow;
use rustain::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use rustain::domain::models::provider::ModelDescriptor;
use rustain::domain::models::visual::OverlayType;
use rustain::domain::models::{
    AdapterRef, FocusState, NotificationsConfig, PortDimension, ProfileDescriptor,
    ProfileIdentityColor, ProfileSelection, ProfileSource, SemanticMessageType, VerdictOffer,
};
use rustain::domain::services::cross_search::CrossSearchResult;
use tokio_util::sync::CancellationToken;

use super::support::{decode, fresh, is_inert, press};
use super::witness::{Case, W, local, local_door, printable, run, w, wp};

fn focused(overlay: OverlayType) -> TuiState {
    let mut s = fresh();
    s.focus = FocusState::Overlay(overlay);
    s
}

/// Ba rows retain their caller frame and base focus, including menu/palette entry.
fn ba_door(s: &mut TuiState, key: Key) -> (String, bool) {
    let caller = s.modal_stack.top().expect("Ba fixture frame").clone();
    let frames = s.modal_stack.iter().cloned().collect::<Vec<_>>();
    let focus = s.focus.clone();
    let outcome = local_door(s, key);
    assert_eq!(s.focus, focus, "Ba dispatch retains base focus");
    if key == Key::Ctrl('p') {
        assert_eq!(s.modal_stack.len(), frames.len() + 1);
        assert_eq!(
            s.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        assert_eq!(
            s.modal_stack
                .iter()
                .take(frames.len())
                .cloned()
                .collect::<Vec<_>>(),
            frames
        );
    } else if is_open(s, caller.surface) {
        assert_eq!(s.modal_stack.iter().cloned().collect::<Vec<_>>(), frames);
    } else {
        assert!(
            s.modal_stack.is_empty(),
            "Back removes only the fixture frame"
        );
    }
    assert_eq!(
        s.pending_leader,
        (key == Key::Ctrl('x')).then_some(caller.surface.context())
    );
    outcome
}

#[rustfmt::skip]
const HELP: &[W] = &[
    w(&[Key::Char('j')], "Consumed | scroll=4 active=true focus=Chat"),
    w(&[Key::Down], "Consumed | scroll=4 active=true focus=Chat"),
    w(&[Key::Char('k')], "Consumed | scroll=2 active=true focus=Chat"),
    w(&[Key::Up], "Consumed | scroll=2 active=true focus=Chat"),
    w(&[Key::Char('G')], "Consumed | scroll=99 active=true focus=Chat"),
    w(&[Key::Char('g')], "Consumed | scroll=0 active=true focus=Chat"),
    w(&[Key::Char('?')], "Consumed | scroll=3 active=false focus=Chat"),
    w(&[Key::Esc], "Consumed | scroll=3 active=false focus=Chat"),
    w(&[Key::Ctrl('c')], "Consumed | scroll=3 active=false focus=Chat"),
    w(&[Key::Ctrl('q')], "Quit | scroll=3 active=true focus=Chat"),
    w(&[Key::Ctrl('x')], "Consumed | scroll=3 active=true focus=Chat"),
    w(&[Key::Ctrl('p')], "Consumed | scroll=3 active=true focus=Chat"),
];

/// Catches a Help row that scrolls the wrong way, closes into the wrong focus, or leaks a key.
#[test]
fn help_rows_by_value_and_every_other_key_is_inert() {
    let setup = || {
        let mut s = fresh();
        s.focus = FocusState::Chat;
        press(&mut s, Key::Char('?'));
        s.help_overlay.scroll_offset = 3;
        s
    };
    // The scroll is clamped to 99 so `G` (a huge sentinel) reads the same everywhere.
    let salient = |s: &TuiState| {
        let h = &s.help_overlay;
        format!(
            "scroll={} active={} focus={:?}",
            h.scroll_offset.min(99),
            is_open(s, Surface::Help),
            s.focus
        )
    };
    run(&local(Context::Help, setup, salient, HELP));
}

#[rustfmt::skip]
const PALETTE: &[W] = &[
    w(&[Key::Up], "Consumed | active=true filter=ab sel=2 focus=Input"),
    w(&[Key::Down], "Consumed | active=true filter=ab sel=1 focus=Input"),
    w(&[Key::Enter], "NewTab | active=false filter= sel=0 focus=Input"),
    w(&[Key::Esc], "Consumed | active=false filter= sel=0 focus=Input"),
    w(&[Key::Ctrl('c')], "Consumed | active=false filter= sel=0 focus=Input"),
    w(&[Key::Backspace], "Consumed | active=true filter=a sel=0 focus=Input"),
    w(&[Key::Ctrl('q')], "Quit | active=true filter=ab sel=0 focus=Input"),
    w(&[Key::Ctrl('x')], "Consumed | active=true filter=ab sel=0 focus=Input"),
    w(&[Key::Ctrl('p')], "Consumed | active=true filter=ab sel=0 focus=Input"),
];

/// Catches a palette row that runs the wrong entry, keeps the palette open, or moves the wrong way.
#[test]
fn command_palette_rows_by_value_and_every_other_non_text_key_is_inert() {
    let setup = || {
        let entry = |action| PaletteEntry {
            name: "e".into(),
            description: String::new(),
            shortcut: None,
            scope: PaletteScope::All,
            action,
        };
        let mut s = fresh();
        press(&mut s, Key::Ctrl('p'));
        s.command_palette.filter_text = "ab".into();
        s.command_palette.filtered_entries = [
            PaletteAction::NewTab,
            PaletteAction::CloseTab,
            PaletteAction::Noop,
        ]
        .map(entry)
        .into();
        s
    };
    let salient = |s: &TuiState| {
        let p = &s.command_palette;
        format!(
            "active={} filter={} sel={} focus={:?}",
            is_open(s, Surface::CommandPalette),
            p.filter_text,
            p.selected_index,
            s.focus
        )
    };
    run(&Case {
        acts_without_row: printable,
        ..local(Context::CommandPalette, setup, salient, PALETTE)
    });
}

#[rustfmt::skip]
fn column(id: &str) -> ProviderColumn {
    let model = |n: u32| ModelDescriptor {
        model_id: format!("{id}-{n}"), display_name: format!("{id} {n}"), provider_id: id.into(),
        context_window: 1000, capabilities: HashSet::new(), pricing_tier: None, stale: false,
        wire_model_id: None,
    };
    ProviderColumn {
        provider_id: id.into(), display_name: id.into(), healthy: true, locked_hint: None,
        discovery: None, sign_in_hint: None, models: (1..=3).map(model).collect(),
    }
}

fn searching(s: &mut TuiState) {
    s.model_selector.search_active = true;
    s.model_selector.search_query = "b-".into();
}

fn warning(s: &mut TuiState) {
    s.model_selector.pending_context_warning = Some(ContextWarning {
        provider_id: "b".into(),
        model_id: "b-3".into(),
        model_display_name: "b 3".into(),
        context_window: 1000,
        current_tokens: 2000,
    });
}

#[rustfmt::skip]
const SELECTOR: &[W] = &[
    w(&[Key::Left], "Consumed | p=0 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('h')], "Consumed | p=0 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Right], "Consumed | p=2 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('l')], "Consumed | p=2 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Up], "Consumed | p=1 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('k')], "Consumed | p=1 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Down], "Consumed | p=1 m=2 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('j')], "Consumed | p=1 m=2 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Enter], "SwitchModelProvider { provider_id: Some(\"b\"), model_id: \"b-2\" } | p=1 m=1 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Esc], "Consumed | p=1 m=1 active=false search=false q= warn=false focus=Input"),
    w(&[Key::Ctrl('c')], "Consumed | p=1 m=1 active=false search=false q= warn=false focus=Input"),
    wp(&[Key::Backspace], "Consumed | p=1 m=0 active=true search=true q=b warn=false focus=Input", searching),
    w(&[Key::Char('/')], "Consumed | p=1 m=1 active=true search=true q= warn=false focus=Input"),
    wp(&[Key::Char('y')], "CompactThenSwitchModel { provider_id: \"b\", model_id: \"b-3\" } | p=1 m=1 active=true search=false q= warn=false focus=Input", warning),
    wp(&[Key::Char('n')], "Consumed | p=1 m=1 active=true search=false q= warn=false focus=Input", warning),
    // The arms lower-case the key: the capitals are the same rows (`folds_case`).
    w(&[Key::Char('H')], "Consumed | p=0 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('L')], "Consumed | p=2 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('K')], "Consumed | p=1 m=0 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('J')], "Consumed | p=1 m=2 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Ctrl('q')], "Quit | p=1 m=1 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Ctrl('x')], "Consumed | p=1 m=1 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Ctrl('p')], "Consumed | p=1 m=1 active=true search=false q= warn=false focus=Input"),
];

/// Off-state (R3): while searching every letter types (`y n j k H` are not commands), and with no
/// warning shown `y` / `n` and, with one, `Enter` / `j` do nothing.
#[rustfmt::skip]
const SELECTOR_OFF: &[W] = &[
    w(&[Key::Char('y')], "Consumed | p=1 m=1 active=true search=false q= warn=false focus=Input"),
    w(&[Key::Char('n')], "Consumed | p=1 m=1 active=true search=false q= warn=false focus=Input"),
    wp(&[Key::Char('y'), Key::Char('n'), Key::Char('j'), Key::Char('k'), Key::Char('H')], "Consumed | p=1 m=0 active=true search=true q=b-ynjkH warn=false focus=Input", searching),
    wp(&[Key::Enter], "Consumed | p=1 m=1 active=true search=false q= warn=true focus=Input", warning),
    wp(&[Key::Char('j')], "Consumed | p=1 m=1 active=true search=false q= warn=true focus=Input", warning),
    wp(&[Key::Char('Y')], "CompactThenSwitchModel { provider_id: \"b\", model_id: \"b-3\" } | p=1 m=1 active=true search=false q= warn=false focus=Input", warning),
    // FieldEdit parent (DF-21-3-TEXTFIELD-CTRL-H-GAPS): `Ctrl+H` edits the search like `Backspace`, and is inert without one.
    wp(&[Key::Ctrl('h')], "Consumed | p=1 m=0 active=true search=true q=b warn=false focus=Input", searching),
    w(&[Key::Ctrl('h')], "Consumed | p=1 m=1 active=true search=false q= warn=false focus=Input"),
];

/// Three columns of three models; the middle model of the middle column is selected.
fn selector() -> TuiState {
    let mut s = fresh();
    presentation::reducer::open(&mut s, Surface::ModelSelector);
    let columns = ["a", "b", "c"].map(column).into();
    s.model_selector.initialize(columns, "b", "b-2");
    s
}

/// Review R6: inside the search every character it accepts types itself and the rest are
/// ignored; no letter is ever a command there.
#[test]
fn model_selector_search_types_what_it_accepts_and_ignores_the_rest() {
    for c in (0x20u8..0x7f).map(char::from) {
        let mut s = selector();
        press(&mut s, Key::Char('/'));
        let action = press(&mut s, Key::Char(c));
        let accepted = c.is_alphanumeric() || "/-_. :+".contains(c);
        let typed = if accepted {
            c.to_string()
        } else {
            String::new()
        };
        assert!(is_inert(&action), "{c:?} -> {action:?}");
        assert_eq!(s.model_selector.search_query, typed, "{c:?}");
        assert!(
            s.model_selector.search_active,
            "{c:?} keeps the search open"
        );
    }
}

/// Catches swapped provider/model directions, a lost selection, and a warning key that fires without a warning.
#[test]
fn model_selector_rows_by_value_and_every_other_key_is_inert() {
    let salient = |s: &TuiState| {
        let m = &s.model_selector;
        format!(
            "p={} m={} active={} search={} q={} warn={} focus={:?}",
            m.selected_provider,
            m.selected_model,
            is_open(s, Surface::ModelSelector),
            m.search_active,
            m.search_query,
            m.pending_context_warning.is_some(),
            s.focus
        )
    };
    run(&Case {
        off: SELECTOR_OFF,
        door: ba_door,
        ..local(Context::ModelSelector, selector, salient, SELECTOR)
    });
}

/// The preview, entered through the real `Enter` row.
fn into_preview(s: &mut TuiState) {
    press(s, Key::Enter);
}

#[rustfmt::skip]
const PROFILES: &[W] = &[
    w(&[Key::Up], "Consumed | sel=0 preview=false active=true focus=Input"),
    w(&[Key::Down], "Consumed | sel=2 preview=false active=true focus=Input"),
    w(&[Key::Enter], "Consumed | sel=1 preview=true active=true focus=Input"),
    w(&[Key::Esc], "Consumed | sel=1 preview=false active=false focus=Input"),
    w(&[Key::Ctrl('c')], "Consumed | sel=1 preview=false active=false focus=Input"),
    wp(&[Key::Esc], "Consumed | sel=1 preview=false active=true focus=Input", into_preview),
    wp(&[Key::Char('y')], "ConfirmProfileSwitch(\"p2\") | sel=1 preview=true active=true focus=Input", into_preview),
    wp(&[Key::Char('n')], "Consumed | sel=1 preview=false active=true focus=Input", into_preview),
    w(&[Key::Ctrl('q')], "Quit | sel=1 preview=false active=true focus=Input"),
    w(&[Key::Ctrl('x')], "Consumed | sel=1 preview=false active=true focus=Input"),
    w(&[Key::Ctrl('p')], "Consumed | sel=1 preview=false active=true focus=Input"),
];

/// Off-state (R3): with no preview `y` / `n` do nothing; inside it Up / Down do nothing and `Y` confirms.
#[rustfmt::skip]
const PROFILES_OFF: &[W] = &[
    w(&[Key::Char('y')], "Consumed | sel=1 preview=false active=true focus=Input"),
    w(&[Key::Char('n')], "Consumed | sel=1 preview=false active=true focus=Input"),
    wp(&[Key::Up], "Consumed | sel=1 preview=true active=true focus=Input", into_preview),
    wp(&[Key::Down], "Consumed | sel=1 preview=true active=true focus=Input", into_preview),
    wp(&[Key::Char('Y')], "ConfirmProfileSwitch(\"p2\") | sel=1 preview=true active=true focus=Input", into_preview),
];
/// Catches Enter that skips the preview, Esc that closes from inside it, and y/n that act without one.
#[test]
fn profile_switcher_rows_by_value_and_every_other_key_is_inert() {
    // Three profiles, the middle one active; current adapters exist so a preview can be built.
    let setup = || {
        let profile = |name: &str| ProfileDescriptor {
            name: name.into(),
            description: None,
            preview: false,
            identity_color: ProfileIdentityColor(1),
            source: ProfileSource::Builtin,
            source_origin: None,
            selection: ProfileSelection::default(),
        };
        let adapter = AdapterRef {
            adapter: "x".into(),
            _config: None,
        };
        let dims = BTreeMap::from([(PortDimension::Persona, adapter)]);
        let mut s = fresh();
        let profiles = ["p1", "p2", "p3"].map(profile).to_vec();
        rustain::adapters::tui::handlers::profile_switch::open(&mut s, |selector| {
            selector.initialize(profiles, "p2", dims);
        });
        s
    };
    let salient = |s: &TuiState| {
        let p = &s.profile_switcher;
        format!(
            "sel={} preview={} active={} focus={:?}",
            p.selected,
            p.preview.is_some(),
            is_open(s, Surface::ProfileSwitcher),
            s.focus
        )
    };
    run(&Case {
        off: PROFILES_OFF,
        door: ba_door,
        ..local(Context::ProfileSwitcher, setup, salient, PROFILES)
    });
}

#[rustfmt::skip]
const USAGE: &[W] = &[
    w(&[Key::Esc], "Consumed | section=0 active=false focus=Chat"),
    w(&[Key::Ctrl('c')], "Consumed | section=0 active=false focus=Chat"),
    w(&[Key::Up], "Consumed | section=3 active=true focus=Chat"),
    w(&[Key::Down], "Consumed | section=1 active=true focus=Chat"),
    w(&[Key::Ctrl('q')], "Quit | section=0 active=true focus=Chat"),
    w(&[Key::Ctrl('x')], "Consumed | section=0 active=true focus=Chat"),
    w(&[Key::Ctrl('p')], "Consumed | section=0 active=true focus=Chat"),
];

/// Catches an Up that fails to wrap to the last section or a close that loses the previous focus.
#[test]
fn usage_panel_rows_by_value_and_every_other_key_is_inert() {
    let setup = || {
        let mut s = fresh();
        s.focus = FocusState::Chat;
        presentation::reducer::open(&mut s, Surface::UsagePanel);
        s.usage_panel.initialize();
        s
    };
    let salient = |s: &TuiState| {
        format!(
            "section={} active={} focus={:?}",
            s.usage_panel.selected_section,
            is_open(s, Surface::UsagePanel),
            s.focus
        )
    };
    run(&Case {
        door: ba_door,
        ..local(Context::UsagePanel, setup, salient, USAGE)
    });
}

#[rustfmt::skip]
const CROSS: &[W] = &[
    w(&[Key::Esc], "CloseCrossSearch | q=ab sel=1 focus=Overlay(CrossSearch)"),
    w(&[Key::Ctrl('c')], "CloseCrossSearch | q=ab sel=1 focus=Overlay(CrossSearch)"),
    w(&[Key::Enter], "OpenCrossSearchResult | q=ab sel=1 focus=Overlay(CrossSearch)"),
    w(&[Key::Backspace], "CrossSearchQueryChanged | q=a sel=1 focus=Overlay(CrossSearch)"),
    w(&[Key::Up], "Consumed | q=ab sel=0 focus=Overlay(CrossSearch)"),
    w(&[Key::Char('k')], "Consumed | q=ab sel=0 focus=Overlay(CrossSearch)"),
    w(&[Key::Down], "Consumed | q=ab sel=2 focus=Overlay(CrossSearch)"),
    w(&[Key::Char('j')], "Consumed | q=ab sel=2 focus=Overlay(CrossSearch)"),
    w(&[Key::Ctrl('q')], "Quit | q=ab sel=1 focus=Overlay(CrossSearch)"),
];

/// Catches j/k typed into the query, a swapped selection direction, or a close that is not requested.
#[test]
fn cross_search_rows_by_value_and_every_other_non_text_key_is_inert() {
    let setup = || {
        let hit = |n: u32| CrossSearchResult {
            conversation_id: format!("c{n}"),
            title: String::new(),
            excerpt: String::new(),
            timestamp: 0,
            first_match_message_index: 0,
        };
        let mut s = focused(OverlayType::CrossSearch);
        s.cross_search.active = true;
        s.cross_search.query = "ab".into();
        s.cross_search.results = (0..3).map(hit).collect();
        s.cross_search.selected = 1;
        s
    };
    let salient = |s: &TuiState| {
        format!(
            "q={} sel={} focus={:?}",
            s.cross_search.query, s.cross_search.selected, s.focus
        )
    };
    run(&Case {
        acts_without_row: printable,
        ..local(Context::CrossSearch, setup, salient, CROSS)
    });
}

#[rustfmt::skip]
const REVERSE: &[W] = &[
    w(&[Key::Enter], "Consumed | rev=false q=al sel=None m=3 buf=alfa pal=false leader=false focus=Input"),
    w(&[Key::Esc], "Consumed | rev=false q=al sel=None m=3 buf= pal=false leader=false focus=Input"),
    w(&[Key::Ctrl('c')], "Consumed | rev=false q=al sel=None m=3 buf= pal=false leader=false focus=Input"),
    w(&[Key::Up], "Consumed | rev=true q=al sel=Some(1) m=3 buf= pal=false leader=false focus=Input"),
    w(&[Key::Ctrl('r')], "Consumed | rev=true q=al sel=Some(1) m=3 buf= pal=false leader=false focus=Input"),
    w(&[Key::Down], "Consumed | rev=true q=al sel=Some(2) m=3 buf= pal=false leader=false focus=Input"),
    w(&[Key::Backspace], "Consumed | rev=true q=a sel=Some(0) m=4 buf= pal=false leader=false focus=Input"),
    w(&[Key::Ctrl('p')], "Consumed | rev=false q=al sel=None m=3 buf= pal=true leader=false focus=Input"),
    w(&[Key::Ctrl('x')], "Consumed | rev=false q=al sel=None m=3 buf= pal=false leader=true focus=Input"),
    w(&[Key::Ctrl('q')], "Quit | rev=true q=al sel=Some(0) m=3 buf= pal=false leader=false focus=Input"),
];

/// Catches an older/newer swap, an accept that picks the wrong entry, and Ctrl+P/Ctrl+X that stay in the search.
#[test]
fn reverse_search_rows_by_value_and_every_other_non_text_key_is_inert() {
    // History `alpha one, beta, alpha two, alfa`; the query `al` has three matches, newest first.
    let setup = || {
        let mut s = fresh();
        for entry in ["alpha one", "beta", "alpha two", "alfa"] {
            s.input_history.push(entry.into());
        }
        s.reverse_search.reset();
        s.reverse_search.query = "al".into();
        let found = s.input_history.search("al");
        s.reverse_search.matches = found.into_iter().map(|(i, e)| (i, e.to_owned())).collect();
        s
    };
    let salient = |s: &TuiState| {
        assert_eq!(s.focus, FocusState::Input);
        assert_eq!(s.cursor_position, s.input_buffer.len());
        assert_eq!(
            s.modal_stack.len(),
            usize::from(is_open(s, Surface::CommandPalette)),
            "ReverseSearch never consumes a frame; only Ctrl+P opens one"
        );
        let r = &s.reverse_search;
        format!(
            "rev={} q={} sel={:?} m={} buf={} pal={} leader={} focus={:?}",
            r.is_open(),
            r.query,
            r.selected_match,
            r.matches.len(),
            s.input_buffer,
            is_open(s, Surface::CommandPalette),
            s.pending_leader.is_some(),
            s.focus
        )
    };
    run(&Case {
        acts_without_row: printable,
        ..local(Context::ReverseSearch, setup, salient, REVERSE)
    });
}

// Autocomplete: every other key dismisses the popup and then acts as a normal
// key, so the harness's "rowless keys are inert" sweep cannot apply. The rows
// run by value here; the documented fall-through has its own test below.

/// The slash popup for `/h` (`help`, `hist`, `home`; `hist` selected) owning its provisional edit.
#[rustfmt::skip]
fn autocomplete() -> TuiState {
    let slash = |name: &str| AutocompleteSuggestion::SlashCommand { name: name.into(), description: String::new() };
    let mut s = fresh();
    press(&mut s, Key::Char('/'));
    press(&mut s, Key::Char('h'));
    assert_eq!(s.autocomplete.kind, Some(AutocompleteKind::SlashCommand));
    assert_eq!(s.focus, FocusState::Input);
    assert!(s.modal_stack.is_empty());
    s.autocomplete.suggestions = ["help", "hist", "home"].map(slash).to_vec();
    s.autocomplete.selected_index = 1;
    s
}

#[rustfmt::skip]
const AUTOCOMPLETE: &[W] = &[
    w(&[Key::Up], "Consumed | buf=/h cur=2 sel=0 active=true filter=h"),
    w(&[Key::Down], "Consumed | buf=/h cur=2 sel=2 active=true filter=h"),
    w(&[Key::Tab], "Consumed | buf=/hist cur=5 sel=0 active=false filter="),
    w(&[Key::Enter], "Consumed | buf=/hist cur=5 sel=0 active=false filter="),
    w(&[Key::Esc], "Consumed | buf= cur=0 sel=0 active=false filter="),
    w(&[Key::Ctrl('c')], "Consumed | buf= cur=0 sel=0 active=false filter="),
    w(&[Key::Backspace], "Consumed | buf=/ cur=1 sel=1 active=true filter="),
    w(&[Key::Ctrl('h')], "Consumed | buf=/ cur=1 sel=1 active=true filter="),
    w(&[Key::Ctrl('q')], "Quit | buf=/h cur=2 sel=1 active=true filter=h"),
];

/// Catches an Autocomplete row that accepts the wrong suggestion, fails to restore the composer on Esc, or has no witness.
#[test]
fn autocomplete_rows_by_value() {
    for row in rows_of(Context::Autocomplete, Host::Local) {
        assert!(
            AUTOCOMPLETE.iter().any(|w| w.keys == row.keys),
            "{:?} has no witness",
            row.keys
        );
    }
    for w in AUTOCOMPLETE {
        let mut s = autocomplete();
        let last = w
            .keys
            .iter()
            .map(|key| local_door(&mut s, *key).0)
            .last()
            .unwrap();
        let a = &s.autocomplete;
        let salient = format!(
            "buf={} cur={} sel={} active={} filter={}",
            s.input_buffer,
            s.cursor_position,
            a.selected_index,
            a.is_open(),
            a.filter_text
        );
        assert_eq!(format!("{last} | {salient}"), w.expect, "keys {:?}", w.keys);
        assert!(
            s.modal_stack.is_empty(),
            "the popup is routed, never a frame"
        );
    }
}

/// Catches a stray non-row key leaving the popup open over a composer the key then edits.
#[test]
fn autocomplete_rowless_key_dismisses_the_popup_and_leaves_the_composer() {
    for key in [Key::Left, Key::Ctrl('z'), Key::F(1)] {
        let mut s = autocomplete();
        press(&mut s, key);
        assert!(!s.autocomplete.is_open(), "{key:?} left the popup open");
        assert!(s.modal_stack.is_empty());
        assert_eq!(s.input_buffer, "/h", "{key:?} edited the composer");
    }
}

#[rustfmt::skip]
const QUIT: &[W] = &[
    w(&[Key::Char('y')], "QuitConfirmed | open=false focus=Chat"),
    w(&[Key::Char('Y')], "QuitConfirmed | open=false focus=Chat"),
    w(&[Key::Char('n')], "Consumed | open=false focus=Chat"),
    w(&[Key::Char('N')], "Consumed | open=false focus=Chat"),
    w(&[Key::Esc], "Consumed | open=false focus=Chat"),
    w(&[Key::Ctrl('c')], "Consumed | open=false focus=Chat"),
    w(&[Key::Enter], "Consumed | open=false focus=Chat"),
];

/// Catches a stray key confirming the quit (or leaking underneath) and Enter that quits instead of staying.
#[test]
fn quit_confirm_rows_by_value_and_every_other_key_is_swallowed() {
    let setup = || {
        let mut s = fresh();
        s.focus = FocusState::Chat;
        let facts = QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Local)
        };
        quit_confirm::request(&mut s, facts);
        s
    };
    let salient = |s: &TuiState| format!("open={} focus={:?}", s.quit_confirm.is_some(), s.focus);
    run(&local(Context::QuitConfirm, setup, salient, QUIT));
}

#[rustfmt::skip]
const RECIPIENT: &[W] = &[
    w(&[Key::Char('p')], "RecipientThreadPage { earlier: true } | surface=true focus=Chat scroll=20"),
    w(&[Key::Char('l')], "RecipientThreadPage { earlier: false } | surface=true focus=Chat scroll=20"),
    w(&[Key::Esc], "Consumed | surface=false focus=Input scroll=0"),
    w(&[Key::Ctrl('c')], "Consumed | surface=false focus=Input scroll=0"),
    w(&[Key::PageUp], "Consumed | surface=true focus=Chat scroll=10"),
    w(&[Key::PageDown], "Consumed | surface=true focus=Chat scroll=30"),
    w(&[Key::Ctrl('q')], "Quit | surface=true focus=Chat scroll=20"),
];

/// A node view with an earlier and a later part of the thread, at line 20 of an 11-row viewport.
#[rustfmt::skip]
pub(super) fn recipient() -> TuiState {
    let mut s = fresh();
    s.focus = FocusState::Chat;
    s.recipient_surface = Some(RecipientSurface {
        target: RecipientSurfaceTarget::Node("n".into()), access: RecipientSurfaceAccess::LocalOnly,
        can_toggle_target: false, root_id: None, first_non_pinned_ordinal: None,
        last_non_pinned_ordinal: None, omitted_before: 2, omitted_after: 3, node_id: None,
        item_state: None, context_available: false, context_selected: false,
        prior_focus: FocusState::Input, scroll_offset: 20, viewport_rows: 11, content: String::new(),
        message_type: SemanticMessageType::Unknown, verdict_offer: VerdictOffer::None, cached_width: 0,
        cached_lines: Vec::new(), cached_header: Vec::new(), cached_footer: Vec::new(),
        cached_action_mask: 0, results: Vec::new(),
    });
    s
}

/// Catches a close that loses the prior focus, a page key that scrolls the wrong way, and a key leaking to the chat below.
#[test]
#[rustfmt::skip]
fn recipient_surface_rows_by_value_and_every_other_key_is_swallowed() {
    let salient = |s: &TuiState| {
        let scroll = s.recipient_surface.as_ref().map_or(0, |r| r.scroll_offset);
        format!("surface={} focus={:?} scroll={scroll}", s.recipient_surface.is_some(), s.focus)
    };
    run(&local(Context::RecipientSurface, recipient, salient, RECIPIENT));
}

fn typed(s: &mut TuiState) {
    s.login.query = "p5".into();
    s.login.refilter();
}

#[rustfmt::skip]
fn running(s: &mut TuiState, sections: Vec<LoginSection>) {
    let cancel = CancellationToken::new();
    let (login_id, provider_id, display) = (1, "example".into(), "Example".into());
    let (origin, progress, field) = (LoginOrigin::Slash, Vec::new(), None);
    s.login.start_running(RunningLogin { login_id, generation: s.login.generation, provider_id, display, origin, cancel, sections, progress, field });
}

#[rustfmt::skip]
fn browser(s: &mut TuiState) {
    let (url, launch_url) = ("https://a.test/x".to_owned(), "http://localhost:1455/go".to_owned());
    running(s, vec![LoginSection::AuthUrl { url, launch_url, instructions: String::new() }]);
}

#[rustfmt::skip]
fn device(s: &mut TuiState) {
    let (verification_url, user_code) = ("https://a.test/dev".to_owned(), "WDJB".to_owned());
    running(s, vec![LoginSection::Device { verification_url, user_code, instructions: String::new() }]);
}

#[rustfmt::skip]
const LOGIN_ROWS: &[W] = &[
    w(&[Key::Esc], "Login(Closed { origin: Slash }) | view=ProviderPicker q= sel=5 active=false focus=Input"),
    wp(&[Key::Esc], "Consumed | view=ProviderPicker q= sel=0 active=true focus=Input", typed),
    w(&[Key::Ctrl('c')], "Login(Closed { origin: Slash }) | view=ProviderPicker q= sel=5 active=false focus=Input"),
    w(&[Key::Enter], "Login(Choose { provider: \"p5\" }) | view=ProviderPicker q= sel=5 active=true focus=Input"),
    w(&[Key::Up], "Consumed | view=ProviderPicker q= sel=4 active=true focus=Input"),
    w(&[Key::Down], "Consumed | view=ProviderPicker q= sel=6 active=true focus=Input"),
    w(&[Key::PageUp], "Consumed | view=ProviderPicker q= sel=0 active=true focus=Input"),
    w(&[Key::PageDown], "Consumed | view=ProviderPicker q= sel=14 active=true focus=Input"),
    wp(&[Key::Backspace], "Consumed | view=ProviderPicker q=p sel=0 active=true focus=Input", typed),
    w(&[Key::Ctrl('q')], "Quit | view=ProviderPicker q= sel=5 active=true focus=Input"),
    wp(&[Key::Alt('c')], "Login(Copy(\"https://a.test/x\")) | view=Running q= sel=5 active=true focus=Input", browser),
    wp(&[Key::Alt('l')], "Login(Copy(\"http://localhost:1455/go\")) | view=Running q= sel=5 active=true focus=Input", browser),
    wp(&[Key::Alt('u')], "Login(Copy(\"https://a.test/dev\")) | view=Running q= sel=5 active=true focus=Input", device),
    wp(&[Key::Char('c')], "Login(Copy(\"https://a.test/x\")) | view=Running q= sel=5 active=true focus=Input", browser),
    wp(&[Key::Char('l')], "Login(Copy(\"http://localhost:1455/go\")) | view=Running q= sel=5 active=true focus=Input", browser),
    wp(&[Key::Char('u')], "Login(Copy(\"https://a.test/dev\")) | view=Running q= sel=5 active=true focus=Input", device),
    w(&[Key::Ctrl('x')], "Consumed | view=ProviderPicker q= sel=5 active=true focus=Input"),
    w(&[Key::Ctrl('p')], "Consumed | view=ProviderPicker q= sel=5 active=true focus=Input"),
];

/// Catches a Login row that picks the wrong provider, pages by one, closes with a search typed, or copies the wrong link.
#[test]
fn login_rows_by_value_and_every_other_non_text_key_is_inert() {
    // The provider picker over fifteen providers `p0..p14`, `p5` selected, nothing typed.
    let setup = || {
        let row = |n: u32| LoginTargetRow {
            id: format!("p{n}"),
            display: format!("Provider {n}"),
            source: None,
            status: String::new(),
            search: format!("p{n}"),
        };
        let mut s = fresh();
        presentation::reducer::open(&mut s, Surface::Login);
        let generation = s
            .login
            .open_picker(&s.modal_stack, LoginOrigin::Slash, OverlayMode::Login)
            .unwrap();
        presentation::reducer::settle_login(&mut s);
        s.login.set_rows(generation, (0..15).map(row).collect());
        s.login.selected = 5;
        s
    };
    let salient = |s: &TuiState| {
        let l = &s.login;
        format!(
            "view={:?} q={} sel={} active={} focus={:?}",
            l.view,
            l.query,
            l.selected,
            is_open(s, Surface::Login),
            s.focus
        )
    };
    run(&Case {
        acts_without_row: printable,
        door: ba_door,
        ..local(Context::Login, setup, salient, LOGIN_ROWS)
    });
}

fn facts() -> LogFacts {
    LogFacts {
        active_conversation_id: Some("conv-a".into()),
        active_messages: 1,
        retry_supported: true,
        decisions_supported: true,
        context_warning: true,
        ..LogFacts::default()
    }
}

fn shown<T: Debug>(out: &Option<T>) -> String {
    out.as_ref()
        .map_or_else(|| "None".to_owned(), |o| format!("{o:?}"))
}

/// The production presentation boundary precedes S-LOG's fact-aware handler.
fn log_door(state: &mut TuiState, key: Key) -> (String, bool) {
    let effect = decode(key).and_then(|e| presentation::route(state, &e, Host::Local, &facts()));
    let out = match effect {
        Some(RouteEffect::Consumed) if !is_open(state, Surface::NotificationLog) => {
            Some(LogOutcome::Closed)
        }
        Some(RouteEffect::Consumed) => Some(LogOutcome::Consumed),
        Some(RouteEffect::QuitRequest) => Some(LogOutcome::Quit),
        Some(RouteEffect::Log(out)) => Some(out),
        Some(RouteEffect::Action(action)) => {
            assert!(is_open(state, Surface::CommandPalette));
            return (format!("{action:?}"), true);
        }
        None => None,
        other => panic!("unexpected local Log effect: {other:?}"),
    };
    (
        shown(&out),
        matches!(out, None | Some(LogOutcome::Consumed)),
    )
}

/// Select `warn`, the middle record.
fn select_warn(s: &mut TuiState) {
    log_door(s, Key::Down);
}

#[rustfmt::skip]
const LOG: &[W] = &[
    w(&[Key::Esc], "Closed | sel=None detail=None vis=3 dec=[] focus=Chat"),
    w(&[Key::Ctrl('c')], "Closed | sel=None detail=None vis=3 dec=[] focus=Chat"),
    w(&[Key::Ctrl('q')], "Quit | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat"),
    w(&[Key::Char('j')], "Consumed | sel=Some((1, Some(2))) detail=Some(false) vis=3 dec=[] focus=Chat"),
    w(&[Key::Down], "Consumed | sel=Some((1, Some(2))) detail=Some(false) vis=3 dec=[] focus=Chat"),
    wp(&[Key::Char('k')], "Consumed | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat", select_warn),
    wp(&[Key::Up], "Consumed | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat", select_warn),
    w(&[Key::Enter], "Consumed | sel=Some((0, Some(3))) detail=Some(true) vis=3 dec=[] focus=Chat"),
    w(&[Key::Char('c')], "Copy(\"boom\") | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat"),
    w(&[Key::Char('x')], "Consumed | sel=Some((0, Some(3))) detail=Some(false) vis=2 dec=[] focus=Chat"),
    w(&[Key::Char('r')], "Retry { action: RetryTurn { messages: 1 }, conversation_id: \"conv-a\" } | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat"),
    wp(&[Key::Char('K')], "Closed | sel=None detail=None vis=3 dec=[ContextWarning] focus=Chat", select_warn),
    wp(&[Key::Char('n')], "Closed | sel=None detail=None vis=3 dec=[Carryover] focus=Chat", select_warn),
    w(&[Key::Ctrl('x')], "Consumed | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat"),
    w(&[Key::Ctrl('p')], "Consumed | sel=Some((0, Some(3))) detail=Some(false) vis=3 dec=[] focus=Chat"),
];

/// Catches a log row that mis-selects, copies/retries the wrong record, or raises a decision from an ineligible one.
#[test]
fn notification_log_rows_by_value_and_every_other_key_is_swallowed() {
    // `alpha` (info), `warn` (context warning of `conv-a`) and `boom` (retryable turn failure of
    // `conv-a`); `boom`, the newest, is selected.
    let setup = || {
        let mut s = fresh();
        s.focus = FocusState::Chat;
        let drafts = [
            NoticeDraft::new(NoticeSource::System, NoticeSeverity::Info, "alpha"),
            NoticeDraft::new(NoticeSource::Context, NoticeSeverity::Warning, "warn")
                .conversation("conv-a")
                .action(NoticeAction::ContextDecision),
            NoticeDraft::new(NoticeSource::System, NoticeSeverity::Error, "boom")
                .conversation("conv-a")
                .action(NoticeAction::RetryTurn { messages: 1 }),
        ];
        for draft in drafts {
            notifications::notify_draft(&mut s, draft);
        }
        notification_log::open(&mut s);
        s
    };
    let salient = |s: &TuiState| {
        let l = s.notification_log.as_ref();
        format!(
            "sel={:?} detail={:?} vis={} dec={:?} focus={:?}",
            l.map(|l| (l.selected, l.selected_id)),
            l.map(|l| l.detail),
            s.notifications.visible().count(),
            s.decisions.items().map(|i| i.kind).collect::<Vec<_>>(),
            s.focus
        )
    };
    run(&Case {
        door: log_door,
        ..local(Context::NotificationLog, setup, salient, LOG)
    });
}

/// A focused decision of `kind`, painted, with the inert window passed on a mock clock (no waiting).
fn pane(kind: DecisionKind) -> TuiState {
    let clock = Arc::new(MockClock::new(Instant::now()));
    let config = NotificationsConfig::default();
    let mut s = fresh();
    s.notifications = NotificationStore::from_config(&config, clock.clone());
    decision_pane::raise(
        &mut s,
        DecisionDraft::new(kind, "the question"),
        Arrival::Requested,
    );
    decision_pane::note_painted(&mut s);
    clock.advance(INERT_AFTER_PAINT);
    s
}

#[rustfmt::skip]
const PANE: &[W] = &[
    w(&[Key::Ctrl('c')], "Consumed | focused=false items=[(Budget, true)]"),
    w(&[Key::Ctrl('q')], "Quit | focused=true items=[(Budget, false)]"),
    wp(&[Key::Enter], "Decision(RecoveryContinue) | focused=false items=[]", |s| *s = pane(DecisionKind::Recovery)),
    w(&[Key::Esc], "Consumed | focused=false items=[(Budget, true)]"),
    w(&[Key::Char('y')], "Decision(BudgetContinue) | focused=false items=[]"),
    wp(&[Key::Char('n')], "Decision(CarryoverDecline) | focused=false items=[]", |s| *s = pane(DecisionKind::Carryover)),
    w(&[Key::Char('s')], "Decision(BudgetSwitchCheaper) | focused=false items=[]"),
    w(&[Key::Char('p')], "Decision(BudgetPause) | focused=false items=[]"),
    wp(&[Key::Char('k')], "Decision(ContextCompact) | focused=false items=[]", |s| *s = pane(DecisionKind::ContextWarning)),
    wp(&[Key::Char('y')], "ConfirmDelete | focused=false items=[]", |s| *s = pane(DecisionKind::DeleteConversations)),
    wp(&[Key::Char('y')], "ImageConfirmAttach | focused=false items=[]", |s| *s = pane(DecisionKind::LargeImage)),
    wp(&[Key::Char('n')], "ImageConfirmCancel | focused=false items=[]", |s| *s = pane(DecisionKind::LargeImage)),
    wp(&[Key::Char('n')], "Consumed | focused=true items=[(Carryover, false)]", |s| *s = pane(DecisionKind::ContextWarning)),
    w(&[Key::Ctrl('x')], "Consumed | focused=true items=[(Budget, false)]"),
    w(&[Key::Ctrl('p')], "Consumed | focused=true items=[(Budget, false)]"),
];

/// A key of another kind's decision does nothing: the pane stays, the decision stays (R7).
#[rustfmt::skip]
const PANE_OFF: &[W] = &[
    w(&[Key::Char('n')], "Consumed | focused=true items=[(Budget, false)]"),
    w(&[Key::Char('k')], "Consumed | focused=true items=[(Budget, false)]"),
    wp(&[Key::Char('y')], "Consumed | focused=true items=[(Recovery, false)]", |s| *s = pane(DecisionKind::Recovery)),
    wp(&[Key::Char('s')], "Consumed | focused=true items=[(Carryover, false)]", |s| *s = pane(DecisionKind::Carryover)),
    wp(&[Key::Char('y')], "Consumed | focused=true items=[(ContextWarning, false)]", |s| *s = pane(DecisionKind::ContextWarning)),
    wp(&[Key::Char('s')], "Consumed | focused=true items=[(LargeImage, false)]", |s| *s = pane(DecisionKind::LargeImage)),
    wp(&[Key::Char('p')], "Consumed | focused=true items=[(DeleteConversations, false)]", |s| *s = pane(DecisionKind::DeleteConversations)),
];

/// Catches a pane key that runs the wrong effect for its decision kind, resolves twice, or defers destructively.
#[test]
fn decision_pane_rows_by_value_and_every_other_key_is_swallowed() {
    assert!(matches!(
        rustain::adapters::tui::keymap::resolve(
            Context::DecisionPane,
            Host::Local,
            &[Key::Ctrl('c')]
        ),
        rustain::adapters::tui::keymap::Resolved::Hit(row)
            if row.command == rustain::adapters::tui::keymap::CommandId::Overlay(
                rustain::adapters::tui::keymap::table_overlay::OverlayCmd::PaneBack
            )
    ));
    let door = |state: &mut TuiState, key| {
        let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
        let draft = (state.input_buffer.clone(), state.cursor_position);
        let focus = state.focus.clone();
        let out = match decode(key) {
            Some(event) => presentation::route(state, &event, Host::Local, &facts())
                .expect("the pane boundary owns every decoded key")
                .into_action(),
            None => InputAction::Ignored,
        };
        assert!(presentation::consistent(state));
        assert_eq!(state.focus, focus, "pane keys preserve base focus");
        assert_eq!(
            (state.input_buffer.as_str(), state.cursor_position),
            (draft.0.as_str(), draft.1),
            "pane keys never edit the composer"
        );
        if matches!(
            rustain::adapters::tui::keymap::resolve(Context::DecisionPane, Host::Local, &[key]),
            rustain::adapters::tui::keymap::Resolved::Miss
        ) {
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                frames
            );
        }
        if key == Key::Ctrl('x') {
            assert_eq!(state.pending_leader, Some(Context::DecisionPane));
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                frames
            );
        }
        if key == Key::Ctrl('p') {
            assert_eq!(
                state.modal_stack.top().unwrap().surface,
                Surface::CommandPalette
            );
            assert_eq!(
                state
                    .modal_stack
                    .iter()
                    .take(frames.len())
                    .cloned()
                    .collect::<Vec<_>>(),
                frames,
                "palette preserves its pane caller"
            );
        }
        (format!("{out:?}"), is_inert(&out))
    };
    let salient = |s: &TuiState| {
        let items: Vec<_> = s.decisions.items().map(|i| (i.kind, i.deferred)).collect();
        assert!(presentation::consistent(s));
        assert_eq!(
            s.modal_stack.len(),
            usize::from(is_open(s, Surface::DecisionPane))
                + usize::from(is_open(s, Surface::CommandPalette))
        );
        format!(
            "focused={} items={items:?}",
            is_open(s, Surface::DecisionPane)
        )
    };
    let setup = || pane(DecisionKind::Budget);
    run(&Case {
        door,
        off: PANE_OFF,
        ..local(Context::DecisionPane, setup, salient, PANE)
    });
}
