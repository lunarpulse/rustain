//! AC7: the shared body is witnessed through its real S-TOOL child, not Viewer::apply.
use rustain::adapters::tui::{
    app::InputAction,
    clipboard::ClipboardResult,
    keymap::{Context, Key},
    notifications::NoticeSource,
    presentation::{self, Surface, markdown_body::BodyMode},
    quit_confirm::{self, QuitFacts, QuitHost},
    state::TuiState,
    tool_inspector::{self, ToolTab},
};
use rustain::domain::models::{FocusState, InvocationStatus, WriteDiffState};
use serde_json::json;
use std::sync::LazyLock;

use super::support::{is_inert, press};
use super::witness::{self, Case, Runtime, W, never, w, wp};
use crate::conformance_21_5_tool_inspector::support::{Fixture, call, pixels, row, rows};

pub(super) struct ToolRuntime(Fixture);

impl Runtime for ToolRuntime {
    fn state_mut(&mut self) -> &mut TuiState {
        &mut self.0.h.state
    }
}

fn setup() -> ToolRuntime {
    let (mut f, _) = Fixture::single(
        80,
        22,
        "Bash",
        json!({"command":"seq 1 99"}),
        InvocationStatus::Success,
        Some((&rows(99), WriteDiffState::NotAWrite)),
    );
    f.h.state.input_buffer = "retained draft".into();
    f.h.state.cursor_position = 4;
    f.paint();
    let before = stage(&f);
    f.open();
    assert_eq!(
        stage(&f),
        before,
        "opening must not edit the owning Stage or draft"
    );
    assert_eq!(f.body_rect().height, 12);
    assert_eq!(f.tool().rows().len(), 100);
    for _ in 0..20 {
        assert_eq!(f.press(Key::Down), InputAction::Consumed);
    }
    f.compose();
    f.top_is(20);
    ToolRuntime(f)
}

// Both cache halves and ViewState belong to the real TabManager, not a detached copy.
fn stage(f: &Fixture) -> String {
    let s = &f.h.state;
    format!(
        "{} {:?} {:?} {:?} {:?} {:?}",
        f.view(),
        f.cache(),
        serde_json::to_vec(f.conversation()).unwrap(),
        (&s.input_buffer, s.cursor_position, s.input_scroll_offset),
        (&s.focus, &s.focused_item_id),
        (f.layout().chat_pane, f.layout().input_area)
    )
}

fn salient(runtime: &ToolRuntime) -> String {
    let f = &runtime.0;
    let s = &f.h.state;
    let Some(frame) = s.modal_stack.top() else {
        return format!("closed payloads={}", s.tool_inspectors.len());
    };
    if !matches!(frame.surface, Surface::ToolInspector(_)) {
        let tool = s
            .tool_inspectors
            .values()
            .next()
            .expect("covered payload retained");
        return format!(
            "{} frames={} payloads={} retained={}",
            frame.surface.context().name(),
            s.modal_stack.len(),
            s.tool_inspectors.len(),
            tool.viewers[ToolTab::Output.index()].top
        );
    }
    let tool = f.tool();
    if presentation::frame_context(s, frame.surface) == Context::InspectorSearch {
        return format!(
            "search-editing frames={} payloads={}",
            s.modal_stack.len(),
            s.tool_inspectors.len()
        );
    }
    if let Some(notice) = s
        .notifications
        .visible()
        .find(|notice| notice.source == NoticeSource::Clipboard)
    {
        return format!("clipboard={:?} {:?}", notice.severity, notice.message);
    }
    let tabs = tool_inspector::layout::parts(f.frame_rect()).tabs;
    format!(
        "{}{}{}@{} {:?} tabs={} frames={} payloads={} leader={}",
        if tool.wrap { "" } else { "wrap-off " },
        if tool.mode == BodyMode::Rendered {
            "rendered "
        } else {
            ""
        },
        tool.tab.label(),
        tool.viewers[tool.tab.index()].top,
        f.body_row(0).trim(),
        row(f.buffer(), tabs, tabs.y).trim(),
        s.modal_stack.len(),
        s.tool_inspectors.len(),
        s.pending_leader.is_some()
    )
}

fn door(runtime: &mut ToolRuntime, key: Key) -> (String, bool) {
    let f = &mut runtime.0;
    let before = stage(f);
    let original = f.h.state.modal_stack.top().unwrap().clone();
    let action = f.press(key);
    if action == InputAction::Quit {
        // The host handles the emitted effect through the existing quit boundary.
        quit_confirm::request(
            &mut f.h.state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
    } else if let InputAction::StepTool { forward } = action {
        let tab = f.tabs.active_tab();
        tool_inspector::step(
            &mut f.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
            forward,
        );
    } else if action == InputAction::CopyToolSource {
        let tab = f.tabs.active_tab();
        let expected = rows(99);
        assert!(tool_inspector::copy::copy_source(
            &mut f.h.state,
            &tab.conversation,
            Some(&tab.reducer),
            &|text| {
                assert_eq!(
                    text, expected,
                    "copy is original full source, not visible row020"
                );
                ClipboardResult::Osc52Success
            },
        ));
    }
    f.compose();
    assert_eq!(stage(f), before, "{key:?}: owning Stage/draft changed");
    assert!(presentation::consistent(&f.h.state));
    if matches!(key, Key::Ctrl('x') | Key::Ctrl('p') | Key::Ctrl('q')) {
        assert!(
            f.h.state.modal_stack.iter().any(|frame| frame == &original),
            "window commands retain the actual tool frame and its identity"
        );
        assert!(f.h.state.tool_inspectors.contains_key(&original.id));
    }
    (format!("{action:?}"), is_inert(&action))
}

fn input(runtime: &mut ToolRuntime) {
    assert_eq!(door(runtime, Key::Right).0, "Consumed");
}

fn top(runtime: &mut ToolRuntime) {
    assert_eq!(door(runtime, Key::Char('g')).0, "Consumed");
}

fn tail(runtime: &mut ToolRuntime) {
    assert_eq!(door(runtime, Key::Char('G')).0, "Consumed");
}

fn with_neighbour(runtime: &mut ToolRuntime) {
    let f = &mut runtime.0;
    call(
        &mut f.tabs.active_tab_mut().conversation.turns[0],
        "Bash",
        json!({"command":"second"}),
        InvocationStatus::Success,
        Some(("next-call", WriteDiffState::NotAWrite)),
    );
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
}

fn at_neighbour(runtime: &mut ToolRuntime) {
    with_neighbour(runtime);
    assert_eq!(
        door(runtime, Key::Char(']')).0,
        "StepTool { forward: true }"
    );
    assert!(
        runtime.0.body_row(0).contains("next-call"),
        "observable other-target control"
    );
}

fn searchable(runtime: &mut ToolRuntime) {
    let f = &mut runtime.0;
    f.press(Key::Char('/'));
    assert_eq!(
        presentation::frame_context(&f.h.state, f.h.state.modal_stack.top().unwrap().surface),
        Context::InspectorSearch
    );
    for ch in "row".chars() {
        f.press(Key::Char(ch));
    }
    f.press(Key::Enter);
    f.compose();
    assert!(
        f.h.screen_text().contains("Search 21/99"),
        "actual committed-match control"
    );
}

#[rustfmt::skip]
const BODY: &[W<ToolRuntime>] = &[
    w(&[Key::Char('j')], "Consumed | Output@21 \"row 021\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Down], "Consumed | Output@21 \"row 021\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('k')], "Consumed | Output@19 \"row 019\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Up], "Consumed | Output@19 \"row 019\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Ctrl('d')], "Consumed | Output@26 \"row 026\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Ctrl('u')], "Consumed | Output@14 \"row 014\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::PageDown], "Consumed | Output@31 \"row 031\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::PageUp], "Consumed | Output@9 \"row 009\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('g')], "Consumed | Output@0 \"row 000\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('G')], "Consumed | Output@88 \"row 088\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
];

#[rustfmt::skip]
const TOOL: &[W<ToolRuntime>] = &[
    wp(&[Key::Left], "Consumed | Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false", input),
    w(&[Key::Right], "Consumed | Input@0 \"{\" tabs=Output  [Input]  Raw frames=1 payloads=1 leader=false"),
    wp(&[Key::Char('[')], "StepTool { forward: false } | Output@0 \"row 000\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false", at_neighbour),
    wp(&[Key::Char(']')], "StepTool { forward: true } | Output@0 \"next-call\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false", with_neighbour),
    w(&[Key::Char('/')], "Consumed | search-editing frames=1 payloads=1"),
    wp(&[Key::Char('n')], "Consumed | Output@21 \"row 021\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false", searchable),
    wp(&[Key::Char('N')], "Consumed | Output@19 \"row 019\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false", searchable),
    w(&[Key::Char('w')], "Consumed | wrap-off Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('c')], "CopyToolSource | clipboard=Success \"Copied to clipboard\""),
    w(&[Key::Esc], "Consumed | closed payloads=0"),
    w(&[Key::Ctrl('c')], "Consumed | closed payloads=0"),
    w(&[Key::Ctrl('x')], "Consumed | Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=true"),
    w(&[Key::Ctrl('p')], "Consumed | Command palette frames=2 payloads=1 retained=20"),
    w(&[Key::Ctrl('q')], "Quit | Quit confirmation frames=2 payloads=1 retained=20"),
];

/// The inherited MarkdownBody rows (`h l 0 $ v`) as one concrete S-TOOL sees them:
/// the 99-row Output is short and wrapped, so the horizontal rows are inapplicable
/// no-ops; `v` renders it and the viewer starts again at the top.
#[rustfmt::skip]
const MARKDOWN: &[W<ToolRuntime>] = &[
    w(&[Key::Char('h')], "Consumed | Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('l')], "Consumed | Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('0')], "Consumed | Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('$')], "Consumed | Output@20 \"row 020\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
    w(&[Key::Char('v')], "Consumed | rendered Output@0 \"row 000 row 001 row 002 row 003 row 004 row 005 row 006 row 007 row 008 row\" tabs=[Output]  Input  Raw frames=1 payloads=1 leader=false"),
];

/// A case lists the rows its surface inherits: the shared body's rows plus the
/// MarkdownBody rows (witnesses are plain copies, so the lists are built once).
static TOOL_INHERITED: LazyLock<Vec<W<ToolRuntime>>> = LazyLock::new(|| [BODY, MARKDOWN].concat());
static BODY_INHERITED: LazyLock<Vec<W<ToolRuntime>>> = LazyLock::new(|| [TOOL, MARKDOWN].concat());

pub(super) fn cases() -> [Case<ToolRuntime>; 2] {
    let case = |context, witnesses, inherited| Case {
        context,
        surface: Context::ToolInspector,
        setup,
        salient,
        witnesses,
        inherited,
        acts_without_row: never,
        door,
        off: &[],
    };
    [
        case(Context::ToolInspector, TOOL, TOOL_INHERITED.as_slice()),
        case(Context::InspectorBody, BODY, BODY_INHERITED.as_slice()),
    ]
}

#[test]
fn tool_and_shared_body_cases_drive_every_row_and_sweep_negative_space() {
    for case in cases() {
        witness::run(&case);
    }
}

#[test]
fn every_body_row_clamps_at_both_ends_in_real_cells() {
    // BODY order is itself checked against every registry row by witness::run.
    // Values here are independent arithmetic, including moves away from each edge.
    for (prep, expected) in [
        (
            top as fn(&mut ToolRuntime),
            [1, 1, 0, 0, 6, 0, 11, 0, 0, 88],
        ),
        (
            tail as fn(&mut ToolRuntime),
            [88, 88, 87, 87, 88, 82, 88, 77, 0, 88],
        ),
    ] {
        for (witness, expected) in BODY.iter().zip(expected) {
            let mut runtime = setup();
            prep(&mut runtime);
            assert_eq!(door(&mut runtime, witness.keys[0]).0, "Consumed");
            runtime.0.top_is(expected);
            assert_eq!(runtime.0.body_row(0).trim(), format!("row {expected:03}"));
        }
    }
    let (mut f, _) = Fixture::single(
        80,
        22,
        "Bash",
        json!({"command":"short"}),
        InvocationStatus::Success,
        Some((&rows(4), WriteDiffState::NotAWrite)),
    );
    f.open();
    assert_eq!(f.tool().rows().len(), 5);
    let mut runtime = ToolRuntime(f);
    for witness in BODY {
        assert_eq!(door(&mut runtime, witness.keys[0]).0, "Consumed");
        runtime.0.top_is(0);
        assert_eq!(runtime.0.body_row(0).trim(), "row 000");
    }
}

#[test]
fn tabs_paint_sources_stop_at_ends_and_back_releases_the_frame() {
    let (mut f, _) = Fixture::single(
        80,
        22,
        "Write",
        json!({"file_path":"witness.txt", "content":"witness-input"}),
        InvocationStatus::Success,
        Some((&rows(99), WriteDiffState::NewFile)),
    );
    f.open();
    let before = stage(&f);
    for (key, strip, first) in [
        (Key::Left, "[Output]  Input  Raw  Diff", "row 000"),
        (Key::Right, "Output  [Input]  Raw  Diff", "{"),
        (Key::Right, "Output  Input  [Raw]  Diff", "{"),
        (Key::Right, "Output  Input  Raw  [Diff]", "+ witness-input"),
        (Key::Right, "Output  Input  Raw  [Diff]", "+ witness-input"),
        (Key::Left, "Output  Input  [Raw]  Diff", "{"),
    ] {
        f.press(key);
        f.compose();
        let tabs = tool_inspector::layout::parts(f.frame_rect()).tabs;
        assert_eq!(row(f.buffer(), tabs, tabs.y).trim(), strip);
        assert_eq!(f.body_row(0).trim(), first);
        assert_eq!(stage(&f), before);
    }
    for key in [Key::Esc, Key::Ctrl('c')] {
        if f.h.state.modal_stack.is_empty() {
            f.open();
        }
        assert_eq!(f.press(key), InputAction::Consumed);
        assert!(f.h.state.modal_stack.is_empty());
        assert!(f.h.state.tool_inspectors.is_empty());
        assert_eq!(stage(&f), before);
        assert!(!f.tabs.active_tab().turn_cancel.is_cancelled());
    }
}

#[test]
fn stage_and_draft_snapshots_have_observable_change_controls() {
    let mut runtime = setup();
    let before = stage(&runtime.0);
    let body = pixels(runtime.0.buffer(), runtime.0.body_rect());
    door(&mut runtime, Key::Down);
    assert_ne!(pixels(runtime.0.buffer(), runtime.0.body_rect()), body);
    assert_eq!(stage(&runtime.0), before);
    door(&mut runtime, Key::Esc);
    let f = &mut runtime.0;
    f.h.state.focus = FocusState::Input;
    let draft = f.h.state.input_buffer.clone();
    press(&mut f.h.state, Key::Char('x'));
    assert_ne!(
        f.h.state.input_buffer, draft,
        "control: composer edits are observable"
    );
    let view = f.view();
    f.tabs.active_tab_mut().view_state.scroll_offset += 1;
    assert_ne!(
        f.view(),
        view,
        "control: the snapshot reads the actual owning view"
    );
    let cache = f.cache();
    f.tabs.active_tab_mut().conversation.messages.insert(
        0,
        rustain::domain::models::ChatMessage {
            id: "registry-stage-cache-control".into(),
            role: rustain::domain::models::MessageRole::User,
            content: "new Stage cache control".into(),
            ..Default::default()
        },
    );
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert_ne!(
        f.cache(),
        cache,
        "control: real Stage rendering changes its owning cache"
    );
}
