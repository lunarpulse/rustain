//! K19 / story `agents-thread` AC7 and AC10 (local host): the sent frame's `f Follow up`
//! field. Frames open through the production read-completion calls (`thread_open::open_sent`,
//! `fill_sent`, `offer_follow_up`: the `k15_sent.rs` precedent); keys travel as crossterm
//! events through `handle_operator_input`; assertions read the painted rail and field rows, the
//! stack, the composer and the outcome store. Design controls: M14 (`f` on a read-only tab),
//! M15 (the field writes the composer), M16 (retained text keyed by peer only).
use super::support::*;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
use rustain::adapters::tui::state::{ThreadAccess, sent_outcome_key};
use rustain::infrastructure::runtime::thread_open;

/// The turn the read offers. Peer-minted, so the prompt paints it through the escape path.
const TURN: &str = "t_\u{202e}7";
const PROMPT: &str = "/team followup ada t_\\u{202e}7 ";
const SIZES: [(u16, u16); 2] = [(80, 24), (130, 30)];

/// Open (or retarget) the frame on ada's sent `item` as the read's completion does: loading,
/// then (when `loaded`) the page of this host's own item and the structured target.
fn sent(rig: &mut Rig, access: ThreadAccess, item: &str, loaded: bool, target: Option<&str>) {
    let state = &mut rig.f.h.state;
    let generation = thread_open::open_sent(state, access, "ada", item, "origin");
    if loaded {
        let page = format!("thread · ada · {item}\n  you       {item}\n      body of {item}");
        let filled = thread_open::fill_sent(state, "origin", generation, &page);
        let offered = thread_open::offer_follow_up(state, "origin", generation, target);
        assert!(filled && offered);
    }
    rig.paint();
}

/// The painted rail: the frame's bottom `footer` inner rows. The widget anchors the rail to the
/// frame's bottom edge with an open field's row above it, so the rail is read from the bottom
/// (`footer_rows` starts right below the body, which is the field row while a field is open).
fn rail(rig: &Rig) -> String {
    let footer = usize::from(geometry(rig).footer);
    let rows = inner_rows(&rig.f);
    rows[rows.len().saturating_sub(footer)..].join("\n")
}

/// The painted field row (prompt and text) of the last paint, if one is painted.
fn field_row(rig: &Rig) -> Option<String> {
    inner_rows(&rig.f)
        .into_iter()
        .find_map(|row| row.find("/team followup ").map(|at| row[at..].to_owned()))
}

fn field(rig: &Rig) -> Option<String> {
    rig.inspector()
        .reply
        .as_ref()
        .map(|reply| reply.field.as_str().to_owned())
}

fn typed(rig: &mut Rig, text: &str) {
    for c in text.chars() {
        rig.key(Key::Char(c));
    }
}

fn retained<'a>(rig: &'a Rig, item: &str) -> Option<&'a str> {
    rig.f
        .h
        .state
        .thread_outcomes
        .retained(&sent_outcome_key("ada", item))
}

/// The operator's composer and history (AC10: byte-identical through every case), including
/// the exact draft the history saved when navigation began.
fn composer(rig: &Rig) -> (String, usize, Vec<String>, bool, String) {
    let s = &rig.f.h.state;
    let history = s.input_history.search("draft");
    (
        s.input_buffer.clone(),
        s.cursor_position,
        history.iter().map(|entry| entry.1.to_owned()).collect(),
        s.input_history.is_navigating(),
        s.input_history.draft().to_owned(),
    )
}

/// Notices, Stage rows (blocks and turns) and Needs-you entries: none is created (AC10).
fn quiet(rig: &Rig) -> [usize; 4] {
    let s = &rig.f.h.state;
    [
        s.notifications.len(),
        s.feedback_blocks.len(),
        s.decisions.len(),
        rig.f.conversation().turns.len(),
    ]
}

/// A rig with a Main draft (caret mid-line) the operator is editing while navigating history
/// (so the history holds a saved draft), a history entry and another thread's retained text.
async fn fresh(width: u16, height: u16) -> Rig {
    let mut rig = Rig::plain(Workspace::new().await, width, height).await;
    let s = &mut rig.f.h.state;
    s.input_history.push("earlier draft".into());
    s.input_history
        .navigate_up("unsent main draft\nsaved by history");
    assert_eq!(
        s.input_history.draft(),
        "unsent main draft\nsaved by history"
    );
    s.input_buffer = "main draft\nline 2".into();
    s.cursor_position = 4;
    s.thread_outcomes
        .retain_reply("sent:bea:other", "kept for bea".into());
    rig
}

#[tokio::test]
async fn k19_follow_up_is_painted_and_live_only_for_a_loaded_target_on_a_writable_tab() {
    for (width, height) in SIZES {
        let mut rig = fresh(width, height).await;
        let (before, calm) = (composer(&rig), quiet(&rig));
        for (access, loaded, target, read_only, case) in [
            (ThreadAccess::Writer, true, None, false, "no target"),
            (
                ThreadAccess::Writer,
                false,
                Some(TURN),
                false,
                "still loading",
            ),
            (
                ThreadAccess::Writer,
                true,
                Some(TURN),
                true,
                "read-only tab (M14)",
            ),
            (
                ThreadAccess::ReadOnly,
                true,
                Some(TURN),
                false,
                "read-only access",
            ),
            (
                ThreadAccess::LocalOnly,
                true,
                Some(TURN),
                false,
                "local-only access",
            ),
        ] {
            rig.f.tabs.active_tab_mut().read_only = read_only;
            sent(&mut rig, access, "opened", loaded, target);
            let case = format!("{case} at {width}x{height}");
            assert!(!rail(&rig).contains("Follow up"), "{case}");
            assert_eq!(rig.key(Key::Char('f')), InputAction::Consumed, "{case}");
            rig.paint();
            assert!(rig.is_open() && rig.inspector().reply.is_none(), "{case}");
            assert_eq!(field_row(&rig), None, "{case}");
            assert_eq!(composer(&rig), before, "{case}");
        }
        rig.f.tabs.active_tab_mut().read_only = false;
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        let painted = rail(&rig);
        let follow = painted.find("[f] Follow up").expect("f is painted");
        assert!(
            painted.find("[x] Retract").is_some_and(|x| follow < x),
            "{painted}"
        );
        assert_eq!(rig.key(Key::Char('f')), InputAction::Consumed);
        rig.paint();
        assert_eq!(field(&rig).as_deref(), Some(""));
        assert_eq!(field_row(&rig).as_deref(), Some(PROMPT.trim_end()));
        let painted = rail(&rig);
        assert!(
            painted.contains("[Enter] Send  [Esc] Clear") && !painted.contains("Follow up"),
            "{painted}"
        );
        assert_eq!((composer(&rig), quiet(&rig)), (before, calm));
    }
}

#[tokio::test]
async fn k19_field_keys_edit_only_the_field_and_enter_yields_the_exact_follow_up() {
    for (width, height) in SIZES {
        let mut rig = fresh(width, height).await;
        let (before, calm) = (composer(&rig), quiet(&rig));
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        rig.key(Key::Char('f'));
        assert_eq!(
            rig.key(Key::Enter),
            InputAction::Consumed,
            "empty text sends nothing"
        );
        // Writer letters and readline chords are text here, never frame keys or the composer.
        typed(&mut rig, "fix the plan");
        rig.key(Key::Ctrl('w'));
        assert_eq!(field(&rig).as_deref(), Some("fix the "));
        rig.key(Key::Backspace);
        typed(&mut rig, " draft x");
        let text = "fix the draft x";
        assert_eq!(field(&rig).as_deref(), Some(text));
        rig.paint();
        assert_eq!(field_row(&rig), Some(format!("{PROMPT}{text}")));
        assert_eq!(
            composer(&rig),
            before,
            "M15: the field never writes the composer"
        );
        let follow_up = InputAction::ThreadFollowUp {
            peer: "ada".into(),
            item: "opened".into(),
            turn: TURN.into(),
            text: text.into(),
        };
        assert_eq!(rig.key(Key::Enter), follow_up);
        assert!(rig.is_open());
        assert_eq!(
            field(&rig).as_deref(),
            Some(text),
            "the dispatch decides, not the key"
        );
        assert_eq!((composer(&rig), quiet(&rig)), (before.clone(), calm));

        // The menu, the palette and quit open above the field; backing out finds it intact.
        rig.keys(&[Key::Ctrl('x'), Key::Char('?')]);
        assert_eq!((rig.depth(), rig.top()), (2, Surface::Help));
        rig.key(Key::Esc);
        rig.key(Key::Ctrl('p'));
        assert_eq!((rig.depth(), rig.top()), (2, Surface::CommandPalette));
        rig.key(Key::Esc);
        assert!(rig.is_open());
        assert_eq!(field(&rig).as_deref(), Some(text));
        assert_eq!(rig.key(Key::Ctrl('q')), InputAction::Quit);
        // The fact the host's guard reads (`thread_inspector::reply_field_has_text`, crate
        // private): the open follow-up field holds text, so the request asks instead of exiting.
        let unsent = rig
            .inspector()
            .reply
            .as_ref()
            .is_some_and(|reply| reply.follow_up.is_some() && !reply.field.is_empty());
        let facts = QuitFacts {
            peer_compose: unsent,
            ..QuitFacts::idle(QuitHost::Local)
        };
        assert_eq!(
            quit_confirm::request(&mut rig.f.h.state, facts),
            QuitRequest::Confirming
        );
        assert_eq!((rig.depth(), rig.top()), (2, Surface::QuitConfirm));
        rig.key(Key::Esc);
        assert!(rig.is_open());
        assert_eq!(field(&rig).as_deref(), Some(text));

        // Esc clears and leaves the field (the text is discarded); the next Esc backs the frame.
        assert_eq!(rig.key(Key::Esc), InputAction::Consumed);
        assert!(rig.is_open() && rig.inspector().reply.is_none());
        assert_eq!(retained(&rig, "opened"), None);
        rig.key(Key::Esc);
        assert!(rig.f.h.state.modal_stack.is_empty());
        // Ctrl+C clears and leaves the field the same way.
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        rig.key(Key::Char('f'));
        typed(&mut rig, "gone");
        assert_eq!(rig.key(Key::Ctrl('c')), InputAction::Consumed);
        assert!(rig.is_open() && rig.inspector().reply.is_none());
        rig.paint();
        assert!(rail(&rig).contains("[f] Follow up"));
        assert_eq!(field_row(&rig), None);
        assert_eq!(retained(&rig, "opened"), None);
        assert_eq!(composer(&rig), before);
        assert_eq!(
            rig.f.h.state.thread_outcomes.retained("sent:bea:other"),
            Some("kept for bea")
        );
    }
}

#[tokio::test]
async fn k19_follow_up_text_is_retained_per_peer_and_item_and_returns_once() {
    for (width, height) in SIZES {
        let mut rig = fresh(width, height).await;
        let (before, calm) = (composer(&rig), quiet(&rig));
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        rig.key(Key::Char('f'));
        typed(&mut rig, "first draft");
        // Retarget to another sent item of the same peer (M16: keyed by peer AND item).
        sent(&mut rig, ThreadAccess::Writer, "second", true, Some("t_2"));
        assert_eq!(retained(&rig, "opened"), Some("first draft"));
        assert_eq!(retained(&rig, "second"), None);
        assert!(rig.inspector().reply.is_none());
        rig.key(Key::Char('f'));
        assert_eq!(
            field(&rig).as_deref(),
            Some(""),
            "two items of one peer share no text"
        );
        typed(&mut rig, "second draft");
        // Back to the first thread: its own text returns once; the second keeps its own.
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        assert_eq!(retained(&rig, "second"), Some("second draft"));
        rig.key(Key::Char('f'));
        assert_eq!(field(&rig).as_deref(), Some("first draft"));
        assert_eq!(retained(&rig, "opened"), None, "pre-filled once");
        rig.paint();
        assert_eq!(field_row(&rig), Some(format!("{PROMPT}first draft")));
        // A same-target reopen keeps it on the same key.
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        assert_eq!(retained(&rig, "opened"), Some("first draft"));
        rig.key(Key::Char('f'));
        assert_eq!(field(&rig).as_deref(), Some("first draft"));
        // Releasing the frame's payload keeps it too; the reopened field takes it.
        reducer::close(&mut rig.f.h.state, Surface::ThreadInspector);
        assert!(rig.f.h.state.modal_stack.is_empty() && rig.f.h.state.thread_inspector.is_none());
        assert_eq!(retained(&rig, "opened"), Some("first draft"));
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        rig.key(Key::Char('f'));
        assert_eq!(field(&rig).as_deref(), Some("first draft"));
        // Esc discards it: the next field of the thread starts empty.
        rig.keys(&[Key::Esc, Key::Esc]);
        assert!(rig.f.h.state.modal_stack.is_empty());
        assert_eq!(retained(&rig, "opened"), None);
        sent(&mut rig, ThreadAccess::Writer, "opened", true, Some(TURN));
        rig.key(Key::Char('f'));
        assert_eq!(field(&rig).as_deref(), Some(""));
        assert_eq!(retained(&rig, "second"), Some("second draft"));
        assert_eq!(
            rig.f.h.state.thread_outcomes.retained("sent:bea:other"),
            Some("kept for bea")
        );
        assert_eq!((composer(&rig), quiet(&rig)), (before, calm));
    }
}
