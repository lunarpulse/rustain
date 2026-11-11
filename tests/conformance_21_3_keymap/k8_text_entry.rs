//! K8: composer, decision feedback and the question answer use grapheme-safe caret
//! editing (6-C moved the answer into the frame's own `TextField`). Attached twins live
//! in attach_keys.rs.

use crate::conformance_21_6_decision_queue::raisers::open_ready;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{DecisionKind, DecisionSubject};
use rustain::adapters::tui::keymap::resolve::resolve_in;
use rustain::adapters::tui::keymap::table::GlobalCmd;
use rustain::adapters::tui::keymap::{
    Binding, CommandId, Context, Host, Key, Resolved, bind, resolve,
};
use rustain::adapters::tui::presentation::text_field::TextField;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::FocusState;

use super::k4_overlays::cross_search_state;
use super::support::{feedback, fresh, is_inert, permission, press};

const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

/// Press `key` in a composer holding `text` with the cursor at scalar `cursor`.
fn composer(text: &str, cursor: usize, key: Key) -> (String, usize) {
    let mut state = fresh();
    state.input_buffer = text.to_string();
    state.cursor_position = cursor;
    assert_eq!(press(&mut state, key), InputAction::Consumed, "{key:?}");
    (state.input_buffer, state.cursor_position)
}

fn is(text: &str, cursor: usize) -> (String, usize) {
    (text.to_string(), cursor)
}

/// Regression: Ctrl+A/E/W/U/K/H typed their letter or did nothing (3-A: inert).
#[test]
fn composer_moves_to_logical_line_start_and_end() {
    let text = "one\ntwo three\nfour";
    assert_eq!(composer(text, 8, Key::Ctrl('a')), is(text, 4));
    assert_eq!(composer(text, 6, Key::Ctrl('e')), is(text, 13));
    assert_eq!(composer(text, 0, Key::Ctrl('e')), is(text, 3));
    // Caret scroll: the caret returns to the first row and the window follows it.
    let mut state = fresh();
    state.input_buffer = "a\nb\nc\nd\ne\nf\ng\nh".into();
    state.cursor_position = 0;
    state.input_scroll_offset = 5;
    press(&mut state, Key::Ctrl('a'));
    assert_eq!(state.input_scroll_offset, 0);
}

/// M7 killer: kill-line must delete to the end of the LOGICAL line, whole graphemes
/// included (`cursor..cursor + 1` would leave the tail of the ZWJ sequence).
#[test]
fn ctrl_k_kills_to_end_of_logical_line_grapheme_safe() {
    assert_eq!(composer("ab cd\nxy", 2, Key::Ctrl('k')), is("ab\nxy", 2));
    assert_eq!(composer("ab cd\nxy", 0, Key::Ctrl('k')), is("\nxy", 0));
    assert_eq!(composer("ab cd\nxy", 5, Key::Ctrl('k')), is("ab cd\nxy", 5));
    let family = format!("a{FAMILY}b c");
    assert_eq!(composer(&family, 1, Key::Ctrl('k')), is("a", 1));
    assert_eq!(
        composer("你好 世界\n次", 3, Key::Ctrl('k')),
        is("你好 \n次", 3)
    );
}

/// Ctrl+U deletes from the line start to the cursor and leaves the other lines.
#[test]
fn ctrl_u_kills_to_start_of_logical_line() {
    assert_eq!(
        composer("one\ntwo three", 13, Key::Ctrl('u')),
        is("one\n", 4)
    );
    assert_eq!(
        composer("one\ntwo three", 7, Key::Ctrl('u')),
        is("one\n three", 4)
    );
    assert_eq!(composer("solo", 4, Key::Ctrl('u')), is("", 0));
    assert_eq!(composer("one\ntwo", 4, Key::Ctrl('u')), is("one\ntwo", 4));
}

/// Ctrl+W deletes the word before the cursor; a ZWJ family, a CJK run and a
/// combining mark are one unit each.
#[test]
fn ctrl_w_kills_the_previous_word_by_grapheme() {
    assert_eq!(
        composer("hello wörld  ", 13, Key::Ctrl('w')),
        is("hello ", 6)
    );
    assert_eq!(
        composer("hello world", 8, Key::Ctrl('w')),
        is("hello rld", 6)
    );
    let family = format!("go {FAMILY}");
    let end = family.chars().count();
    assert_eq!(composer(&family, end, Key::Ctrl('w')), is("go ", 3));
    assert_eq!(composer("你好 世界", 5, Key::Ctrl('w')), is("你好 ", 3));
    assert_eq!(composer("e\u{301}x", 3, Key::Ctrl('w')), is("", 0));
    assert_eq!(composer("", 0, Key::Ctrl('w')), is("", 0));
}

/// Regression (review R2): Ctrl+W scanned from the start of the text once per
/// step, so a pasted 200k-scalar blob with no space froze the UI. One reverse
/// scan finishes far inside the (generous, sleep-free) bound.
#[test]
fn ctrl_w_on_a_huge_single_word_is_linear() {
    let blob = "a".repeat(200_000);
    let started = std::time::Instant::now();
    assert_eq!(composer(&blob, 200_000, Key::Ctrl('w')), is("", 0));
    assert_eq!(
        composer(&format!("{blob} x"), 200_002, Key::Ctrl('w')),
        is(&format!("{blob} "), 200_001)
    );
    let took = started.elapsed();
    assert!(
        took.as_secs() < 5,
        "Ctrl+W on a 200k-scalar word took {took:?}"
    );
}

/// Ctrl+H is Backspace (legacy terminals send 0x08 for it): one grapheme.
#[test]
fn ctrl_h_deletes_one_grapheme_like_backspace() {
    let family = format!("a{FAMILY}");
    let end = family.chars().count();
    assert_eq!(composer(&family, end, Key::Ctrl('h')), is("a", 1));
    assert_eq!(composer("ae\u{301}", 3, Key::Ctrl('h')), is("a", 1));
    assert_eq!(
        composer("abc", 2, Key::Ctrl('h')),
        composer("abc", 2, Key::Backspace)
    );
    assert_eq!(composer("abc", 0, Key::Ctrl('h')), is("abc", 0));
}

/// Typing in the composer never becomes a command; the two documented popup
/// triggers insert their character and open their popup.
#[test]
fn printables_in_the_composer_type_and_only_the_documented_triggers_open_popups() {
    for c in (0x21u8..0x7f)
        .map(char::from)
        .filter(|c| !matches!(c, '/' | '@'))
    {
        let mut state = fresh();
        assert_eq!(
            press(&mut state, Key::Char(c)),
            InputAction::Consumed,
            "{c}"
        );
        assert_eq!(state.input_buffer, c.to_string(), "{c} types itself");
        assert!(!state.autocomplete.is_open(), "{c} opens no popup");
    }
    for trigger in ['/', '@'] {
        let mut state = fresh();
        press(&mut state, Key::Char(trigger));
        assert_eq!(state.input_buffer, trigger.to_string());
        assert!(state.autocomplete.is_open(), "{trigger} opens its popup");
    }
}

/// M6 killer: a context below `TextEntry` inherits ONLY the rows marked
/// `text_entry_ok`. A synthetic table with a non-inheritable Global row on `q`
/// proves the filter itself (the shipped Global rows are all inheritable).
#[test]
fn text_entry_children_inherit_only_the_marked_rows() {
    static FIXTURE: &[Binding] = &[
        bind(
            Context::Global,
            &[Key::Char('q')],
            CommandId::Global(GlobalCmd::QuitRequest),
            "q",
        ),
        bind(
            Context::Global,
            &[Key::Ctrl('x')],
            CommandId::Global(GlobalCmd::OpenLeader),
            "x",
        )
        .te(),
    ];
    assert!(matches!(
        resolve_in(FIXTURE, Context::Input, Host::Local, &[Key::Char('q')]),
        Resolved::Miss
    ));
    assert!(matches!(
        resolve_in(FIXTURE, Context::Input, Host::Local, &[Key::Ctrl('x')]),
        Resolved::Hit(_)
    ));
    // Positive control: a context that does not pass through TextEntry sees both.
    assert!(matches!(
        resolve_in(FIXTURE, Context::Chat, Host::Local, &[Key::Char('q')]),
        Resolved::Hit(_)
    ));
}

/// LINT (a table-level check is not behavioural proof; the effect sweep below is):
/// no printable resolves to a command in any text field context of the shipped
/// table, except the documented rows: the popup triggers `/` and `@` in the
/// composer, and `j` / `k` result navigation in cross-search (they cannot be typed
/// into that query today).
#[test]
fn lint_no_printable_resolves_to_a_row_in_any_text_entry() {
    let fields = [
        Context::Input,
        Context::SearchTyping,
        Context::CommandPalette,
        Context::ReverseSearch,
        Context::CrossSearch,
        Context::InspectorSearch,
        Context::FieldEdit,
        Context::DecisionCompose,
    ];
    for context in fields {
        for c in (0x20u8..0x7f).map(char::from) {
            let allowed = matches!((context, c), (Context::Input, '/' | '@'))
                || matches!((context, c), (Context::CrossSearch, 'j' | 'k'));
            let resolved = resolve(context, Host::Local, &[Key::Char(c)]);
            assert_eq!(
                matches!(resolved, Resolved::Hit(_)),
                allowed,
                "{}: {c:?} resolved to {resolved:?}",
                context.name()
            );
        }
    }
}

/// K8 effect sweep (review R6; M6 is re-sited here once 3-B2 dispatches through the resolver): every printable ASCII key types itself into
/// every text field and answers with a typing action, never a command.
/// Cross-search `j` / `k` navigate instead (they are rows). The model selector's
/// search has its own sweep in `k4_overlays`. The "pending plan / delegation /
/// consolidation card" leg of K8 moved to K11 (3-C): it cannot pass before AC17.
#[test]
fn every_printable_types_into_every_text_field() {
    for field in FIELDS {
        for c in (0x20u8..0x7f).map(char::from) {
            let mut state = fresh();
            (field.open)(&mut state);
            let action = press(&mut state, Key::Char(c));
            if field.name == "cross-search query" {
                assert_eq!(state.modal_stack.len(), 2, "CrossSearch above History");
                assert_eq!(
                    state.modal_stack.top().unwrap().surface,
                    rustain::adapters::tui::presentation::Surface::CrossSearch
                );
                assert_eq!(state.focus, FocusState::Input);
            }
            let text = (field.text)(&state);
            let name = field.name;
            if name.starts_with("cross") && matches!(c, 'j' | 'k') {
                assert!(
                    is_inert(&action) && text.is_empty(),
                    "{name}: {c:?} navigates"
                );
                continue;
            }
            assert_eq!(text, c.to_string(), "{name}: {c:?} types itself");
            assert!(
                matches!(
                    action,
                    InputAction::Consumed
                        | InputAction::SearchQueryChanged
                        | InputAction::CrossSearchQueryChanged
                ),
                "{name}: {c:?} -> {action:?}"
            );
        }
    }
}

// ── text fields (caret-capable queries and remaining tail editors) ──────────

struct Field {
    name: &'static str,
    open: fn(&mut TuiState),
    text: fn(&TuiState) -> &str,
    set: fn(&mut TuiState, &str),
}

/// The open question's answer field (the frame owns it; the composer never does).
fn question_field(state: &TuiState) -> &TextField {
    state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            DecisionSubject::Question(payload) => Some(&payload.field),
            _ => None,
        })
        .expect("a question is open")
}
const FIELDS: &[Field] = &[
    Field {
        name: "command palette filter",
        open: |s| {
            press(s, Key::Ctrl('p'));
        },
        text: |s| s.command_palette.filter_text.as_str(),
        set: |s, text| s.command_palette.filter_text.set(text),
    },
    Field {
        name: "search query",
        open: |s| {
            press(s, Key::Ctrl('f'));
        },
        text: |s| s.search_state.query.as_str(),
        set: |s, text| s.search_state.query.set(text),
    },
    Field {
        name: "cross-search query",
        open: |s| *s = cross_search_state(),
        text: |s| s.cross_search.query.as_str(),
        set: |s, text| s.cross_search.query = text.into(),
    },
    Field {
        name: "reverse-search query",
        open: |s| {
            press(s, Key::Ctrl('r'));
        },
        text: |s| s.reverse_search.query.as_str(),
        set: |s, text| s.reverse_search.query.set(text),
    },
    Field {
        name: "log inspector query",
        open: |s| {
            press(s, Key::Ctrl('x'));
            press(s, Key::Char('n'));
            press(s, Key::Char('/'));
        },
        text: |s| s.notification_log.as_ref().unwrap().search.query.as_str(),
        set: |s, text| s.notification_log.as_mut().unwrap().search.query.set(text),
    },
    Field {
        name: "help inspector query",
        open: |s| {
            press(s, Key::Ctrl('x'));
            press(s, Key::Char('?'));
            press(s, Key::Char('/'));
        },
        text: |s| s.help_overlay.search.query.as_str(),
        set: |s, text| s.help_overlay.search.query.set(text),
    },
    Field {
        name: "permission feedback",
        open: |s| {
            *s = permission();
            assert_eq!(press(s, Key::Char('f')), InputAction::Consumed);
        },
        text: |s| feedback(s).as_str(),
        set: |s, text| {
            press(s, Key::Ctrl('a'));
            press(s, Key::Ctrl('k'));
            rustain::adapters::tui::app::handle_input(
                s,
                &rustain::domain::events::DomainInputEvent::Paste(text.into()),
            );
        },
    },
    Field {
        name: "question answer",
        open: |s| {
            let (state, _clock, hold) = open_ready(DecisionKind::Question, "What?");
            // The turn keeps waiting: the receiver outlives the test state.
            std::mem::forget(hold);
            *s = state;
        },
        text: |s| question_field(s).as_str(),
        set: |s, text| {
            press(s, Key::Ctrl('a'));
            press(s, Key::Ctrl('k'));
            rustain::adapters::tui::app::handle_input(
                s,
                &rustain::domain::events::DomainInputEvent::Paste(text.into()),
            );
        },
    },
];

/// Regression: the fields only knew Backspace; Ctrl+W/U/H did nothing (or typed).
#[test]
fn append_only_fields_edit_their_tail_by_grapheme() {
    for field in FIELDS {
        let name = field.name;
        if name == "permission feedback" || name == "question answer" {
            continue; // Caret-aware behavior is witnessed separately below.
        }
        let mut state = fresh();
        (field.open)(&mut state);
        let set = |state: &mut TuiState, text: &str| (field.set)(state, text);
        set(&mut state, &format!("alpha beta {FAMILY}"));
        press(&mut state, Key::Ctrl('w'));
        assert_eq!(
            (field.text)(&mut state),
            "alpha beta ",
            "{name}: the family is one word"
        );
        press(&mut state, Key::Ctrl('w'));
        assert_eq!((field.text)(&mut state), "alpha ", "{name}");
        set(&mut state, "xe\u{301}");
        press(&mut state, Key::Ctrl('h'));
        assert_eq!(
            (field.text)(&mut state),
            "x",
            "{name}: the accent goes with its letter"
        );
        set(&mut state, "one two");
        press(&mut state, Key::Ctrl('u'));
        assert_eq!((field.text)(&mut state), "", "{name}: cleared");
        // Motion preserves text; the final kill is at EOL. K10 observes query carets.
        set(&mut state, "keep");
        for key in ['a', 'e', 'k'] {
            let action = press(&mut state, Key::Ctrl(key));
            assert!(is_inert(&action), "{name}: Ctrl+{key} -> {action:?}");
            assert_eq!((field.text)(&mut state), "keep", "{name}: Ctrl+{key}");
        }
    }
}

#[test]
fn permission_feedback_edits_at_a_grapheme_safe_caret() {
    use rustain::adapters::tui::app::handle_input;
    use rustain::domain::events::DomainInputEvent;

    let mut state = permission();
    state.input_buffer = "retained\ncomposer draft".into();
    state.cursor_position = 5;
    assert_eq!(press(&mut state, Key::Char('f')), InputAction::Consumed);
    let draft = (
        state.input_buffer.clone(),
        state.cursor_position,
        state.multiline_mode,
    );
    let height = |s: &TuiState| {
        rustain::adapters::tui::layout::compute_layout(
            ratatui::layout::Rect::new(0, 0, 80, 24),
            &s.theme,
            &s.input_buffer,
            1,
            false,
            rustain::domain::models::visual::DensityMode::Focus,
        )
        .unwrap()
        .input_area
        .height
    };
    let before_height = height(&state);
    let paste = |s: &mut TuiState, text: &str| {
        handle_input(s, &DomainInputEvent::Paste(text.into()));
    };
    paste(&mut state, "alpha beta");
    press(&mut state, Key::Ctrl('a'));
    assert_eq!(feedback(&state).cursor(), 0);
    press(&mut state, Key::Right);
    press(&mut state, Key::Char('X'));
    assert_eq!(feedback(&state).as_str(), "aXlpha beta");
    press(&mut state, Key::Left);
    press(&mut state, Key::Backspace);
    assert_eq!(feedback(&state).as_str(), "Xlpha beta");
    press(&mut state, Key::End);
    press(&mut state, Key::Ctrl('w'));
    assert_eq!(feedback(&state).as_str(), "Xlpha ");
    paste(&mut state, FAMILY);
    press(&mut state, Key::Ctrl('h'));
    assert_eq!(feedback(&state).as_str(), "Xlpha ");
    paste(&mut state, "e\u{301}tail");
    press(&mut state, Key::Home);
    press(&mut state, Key::Ctrl('k'));
    assert_eq!(feedback(&state).as_str(), "");
    paste(&mut state, "one two");
    press(&mut state, Key::Ctrl('a'));
    press(&mut state, Key::Right);
    press(&mut state, Key::Ctrl('u'));
    assert_eq!(feedback(&state).as_str(), "ne two");
    press(&mut state, Key::Ctrl('e'));
    assert_eq!(feedback(&state).cursor(), 6);
    assert_eq!(height(&state), before_height);
    assert_eq!(
        (
            state.input_buffer,
            state.cursor_position,
            state.multiline_mode
        ),
        draft
    );
}

#[test]
fn question_answer_edits_at_a_grapheme_safe_caret_and_never_moves_the_composer() {
    use rustain::adapters::tui::app::handle_input;
    use rustain::domain::events::DomainInputEvent;

    let (mut state, _clock, hold) = open_ready(DecisionKind::Question, "What?");
    std::mem::forget(hold);
    state.input_buffer = "retained\ncomposer draft".into();
    state.cursor_position = 5;
    let draft = (
        state.input_buffer.clone(),
        state.cursor_position,
        state.multiline_mode,
    );
    let paste = |s: &mut TuiState, text: &str| {
        handle_input(s, &DomainInputEvent::Paste(text.into()));
    };
    paste(&mut state, "alpha beta");
    press(&mut state, Key::Ctrl('a'));
    assert_eq!(question_field(&state).cursor(), 0);
    press(&mut state, Key::Right);
    press(&mut state, Key::Char('X'));
    assert_eq!(question_field(&state).as_str(), "aXlpha beta");
    press(&mut state, Key::Left);
    press(&mut state, Key::Backspace);
    assert_eq!(question_field(&state).as_str(), "Xlpha beta");
    press(&mut state, Key::End);
    press(&mut state, Key::Ctrl('w'));
    assert_eq!(question_field(&state).as_str(), "Xlpha ");
    paste(&mut state, FAMILY);
    press(&mut state, Key::Ctrl('h'));
    assert_eq!(question_field(&state).as_str(), "Xlpha ");
    press(&mut state, Key::Home);
    press(&mut state, Key::Ctrl('k'));
    assert_eq!(question_field(&state).as_str(), "");
    paste(&mut state, "one two");
    press(&mut state, Key::Ctrl('a'));
    press(&mut state, Key::Right);
    press(&mut state, Key::Ctrl('u'));
    assert_eq!(question_field(&state).as_str(), "ne two");
    press(&mut state, Key::Ctrl('e'));
    assert_eq!(question_field(&state).cursor(), 6);
    assert_eq!(
        (
            state.input_buffer,
            state.cursor_position,
            state.multiline_mode
        ),
        draft,
        "the composer is untouched by the answer field"
    );
}
