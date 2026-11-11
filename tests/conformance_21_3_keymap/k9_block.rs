//! K7 (AC8): every BlockViewer row and the MarkdownBody/InspectorBody rows it
//! inherits, by value through the real frame door, plus the negative sweep.
use rustain::adapters::tui::{
    app::InputAction,
    block_viewer,
    clipboard::ClipboardResult,
    keymap::{Context, Key},
    markdown::BlockMode,
    notifications::NoticeSource,
    presentation::{self, Surface},
    quit_confirm::{self, QuitFacts, QuitHost},
    state::TuiState,
};

use super::witness::{self, Case, Runtime, W, never, w, wp};
use crate::conformance_21_13_block_viewer::support::{
    Fixture, block, body_row, code, open, single,
};

pub(super) struct BlockRuntime(Fixture);

impl Runtime for BlockRuntime {
    fn state_mut(&mut self) -> &mut TuiState {
        &mut self.0.h.state
    }
}

fn setup() -> BlockRuntime {
    // 25 rows: the Stage keeps the 20 rows (80x24 before the mission strip) the witnesses pin.
    let (mut f, id) = single(80, 25, &code(60));
    f.h.state.input_buffer = "retained draft".into();
    f.h.state.cursor_position = 4;
    open(&mut f, &id);
    for _ in 0..20 {
        assert_eq!(f.press(Key::Down), InputAction::Consumed);
    }
    for _ in 0..2 {
        assert_eq!(f.press(Key::Char('l')), InputAction::Consumed);
    }
    f.compose();
    BlockRuntime(f)
}

fn stage(f: &Fixture) -> String {
    let s = &f.h.state;
    format!(
        "{} {:?} {:?} {:?}",
        f.view(),
        f.cache(),
        (&s.input_buffer, s.cursor_position),
        (&s.focus, &s.focused_item_id)
    )
}

fn salient(runtime: &BlockRuntime) -> String {
    let f = &runtime.0;
    let s = &f.h.state;
    let Some(frame) = s.modal_stack.top() else {
        return format!("closed payloads={}", s.block_viewers.len());
    };
    if !matches!(frame.surface, Surface::BlockViewer(_)) {
        return format!(
            "{} frames={} payloads={}",
            frame.surface.context().name(),
            s.modal_stack.len(),
            s.block_viewers.len()
        );
    }
    if presentation::frame_context(s, frame.surface) == Context::InspectorSearch {
        return format!("search-editing frames={}", s.modal_stack.len());
    }
    if let Some(notice) = s
        .notifications
        .visible()
        .find(|notice| notice.source == NoticeSource::Clipboard)
    {
        return format!("clipboard={:?} {:?}", notice.severity, notice.message);
    }
    let block = block(f);
    let row: Vec<char> = body_row(f, 0).chars().collect();
    let words = [
        (block.mode == BlockMode::Source, " source"),
        (block.numbers, " numbers"),
        (block.wrap, " wrap"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, word)| *word)
    .collect::<String>();
    format!(
        "@{} h{} {}{}{words} frames={} leader={}",
        block.viewer.top,
        block.viewer.h_off,
        row[0],
        row[row.len() - 1],
        s.modal_stack.len(),
        s.pending_leader.is_some()
    )
}

fn door(runtime: &mut BlockRuntime, key: Key) -> (String, bool) {
    let f = &mut runtime.0;
    let before = stage(f);
    let action = f.press(key);
    match &action {
        InputAction::Quit => {
            quit_confirm::request(
                &mut f.h.state,
                QuitFacts {
                    streams: 1,
                    ..QuitFacts::idle(QuitHost::Local)
                },
            );
        }
        InputAction::CopyBlockSource => {
            let tab = f.tabs.active_tab();
            block_viewer::copy_source(
                &mut f.h.state,
                &tab.conversation,
                tab.reducer.open_turn.as_ref(),
                &|text| {
                    assert!(text.starts_with("fn line_01()"), "the exact block source");
                    assert_eq!(text.len(), 1064);
                    ClipboardResult::Osc52Success
                },
            );
        }
        _ => {}
    }
    f.compose();
    assert_eq!(
        stage(f),
        before,
        "{key:?}: the owning Stage or draft changed"
    );
    assert!(presentation::consistent(&f.h.state));
    let shown = format!("{action:?}");
    (
        shown,
        matches!(action, InputAction::Consumed | InputAction::Ignored),
    )
}

fn searchable(runtime: &mut BlockRuntime) {
    let f = &mut runtime.0;
    f.press(Key::Char('/'));
    for ch in "line_".chars() {
        f.press(Key::Char(ch));
    }
    f.press(Key::Enter);
    f.compose();
}

#[rustfmt::skip]
const INHERITED: &[W<BlockRuntime>] = &[
    w(&[Key::Char('h')], "Consumed | @20 h8 ←→ frames=1 leader=false"),
    w(&[Key::Char('l')], "Consumed | @20 h24 ←→ frames=1 leader=false"),
    w(&[Key::Char('0')], "Consumed | @20 h0 │→ frames=1 leader=false"),
    w(&[Key::Char('$')], "Consumed | @20 h44 ←│ frames=1 leader=false"),
    w(&[Key::Char('j')], "Consumed | @21 h16 ←→ frames=1 leader=false"),
    w(&[Key::Down], "Consumed | @21 h16 ←→ frames=1 leader=false"),
    w(&[Key::Char('k')], "Consumed | @19 h16 ←→ frames=1 leader=false"),
    w(&[Key::Up], "Consumed | @19 h16 ←→ frames=1 leader=false"),
    w(&[Key::Ctrl('d')], "Consumed | @28 h16 ←→ frames=1 leader=false"),
    w(&[Key::Ctrl('u')], "Consumed | @12 h16 ←→ frames=1 leader=false"),
    w(&[Key::PageDown], "Consumed | @35 h16 ←→ frames=1 leader=false"),
    w(&[Key::PageUp], "Consumed | @5 h16 ←→ frames=1 leader=false"),
    w(&[Key::Char('g')], "Consumed | @0 h16 ←→ frames=1 leader=false"),
    w(&[Key::Char('G')], "Consumed | @46 h16 ←→ frames=1 leader=false"),
    w(&[Key::Char('v')], "Consumed | @0 h0 f  source frames=1 leader=false"),
];

#[rustfmt::skip]
const BLOCK: &[W<BlockRuntime>] = &[
    w(&[Key::Char('/')], "Consumed | search-editing frames=1"),
    wp(&[Key::Char('n')], "Consumed | @21 h3 ←→ frames=1 leader=false", searchable),
    wp(&[Key::Char('N')], "Consumed | @19 h3 ←→ frames=1 leader=false", searchable),
    w(&[Key::Char('c')], "CopyBlockSource | clipboard=Success \"Copied to clipboard\""),
    w(&[Key::Char('#')], "Consumed | @20 h16 2→ numbers frames=1 leader=false"),
    w(&[Key::Char('w')], "Consumed | @20 h0 ││ wrap frames=1 leader=false"),
    w(&[Key::Char('[')], "StepBlock { forward: false } | @20 h16 ←→ frames=1 leader=false"),
    w(&[Key::Char(']')], "StepBlock { forward: true } | @20 h16 ←→ frames=1 leader=false"),
    w(&[Key::Esc], "Consumed | closed payloads=0"),
    w(&[Key::Ctrl('c')], "Consumed | closed payloads=0"),
    w(&[Key::Ctrl('x')], "Consumed | @20 h16 ←→ frames=1 leader=true"),
    w(&[Key::Ctrl('p')], "Consumed | Command palette frames=2 payloads=1"),
    w(&[Key::Ctrl('q')], "Quit | Quit confirmation frames=2 payloads=1"),
];

pub(super) fn cases() -> [Case<BlockRuntime>; 1] {
    [Case {
        context: Context::BlockViewer,
        surface: Context::BlockViewer,
        setup,
        salient,
        witnesses: BLOCK,
        inherited: INHERITED,
        acts_without_row: never,
        door,
        off: &[],
    }]
}

#[test]
fn block_and_markdown_body_cases_drive_every_row_and_sweep_negative_space() {
    for case in cases() {
        witness::run(&case);
    }
}
