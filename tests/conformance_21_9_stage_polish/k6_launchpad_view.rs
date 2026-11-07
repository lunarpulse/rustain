//! K6 (AC12, AC13): the launchpad an empty conversation paints, through the
//! production front doors: `launchpad_facts::refresh` builds the facts, the
//! chat pane (`render_with_search` for the local host, `render_attached` for the
//! attached host) paints them into a `TestBackend`. Expected rows are literals;
//! the key line is checked against the registry rows' availability.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use rustain::adapters::tui::{
    launchpad::LaunchpadEnv,
    state::{TabRenderState, TuiState},
    theme::Theme,
    widgets::chat_pane,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{Conversation, ConversationSummary, StreamingState, ViewState};
use rustain::domain::services::session_index::SessionIndex;
use rustain::infrastructure::runtime::launchpad_facts;

use crate::conformance_21_12_markdown::support::rows;
use crate::render_helpers::make_conversation;

/// The wall time of the injected clock (s = 1_700_000_000).
pub(crate) const NOW_MS: i64 = 1_700_000_000_000;
pub(crate) const NOW_S: i64 = 1_700_000_000;

pub(crate) fn empty_conversation(id: &str) -> Conversation {
    let mut conversation = make_conversation(Vec::new(), Vec::new());
    conversation.id = id.to_owned();
    conversation
}

pub(crate) fn summary(id: &str, title: &str, age_s: i64, messages: usize) -> ConversationSummary {
    ConversationSummary {
        id: id.to_owned(),
        title: title.to_owned(),
        created_at: NOW_S - age_s - 60,
        updated_at: NOW_S - age_s,
        message_count: messages,
        has_fork_source: false,
    }
}

/// A local `TuiState` whose launchpad refresh reads `workspace` and treats the
/// providers named in `locked` as locked.
pub(crate) fn local_state(width: u16, height: u16, workspace: &Path, locked: &[&str]) -> TuiState {
    let mut state = TuiState::new(width, height);
    state.terminal_width = width;
    let locked: Vec<String> = locked.iter().map(|id| (*id).to_owned()).collect();
    state.launchpad_env = Some(LaunchpadEnv::new(workspace.to_path_buf(), move |id| {
        locked.iter().any(|candidate| candidate == id)
    }));
    state
}

/// Refresh then paint the local host's chat pane for `conversation`.
pub(crate) fn paint_local(
    state: &mut TuiState,
    conversation: &Conversation,
    index: &SessionIndex,
    model: &str,
    provider: &str,
    width: u16,
    height: u16,
) -> Vec<String> {
    let clock = MockClock::at_wall_ms(NOW_MS);
    launchpad_facts::refresh(state, conversation, model, provider, index, &clock);
    let mut tab = state
        .tab_render_states
        .remove(&state.active_tab_id)
        .unwrap_or_default();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            chat_pane::render_with_search(
                frame,
                Rect::new(0, 0, width, height),
                conversation,
                None,
                &StreamingState::default(),
                &ViewState::default(),
                &clock,
                0,
                true,
                &Theme::dark(),
                &mut tab,
                &BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                Default::default(),
            );
        })
        .unwrap();
    state.tab_render_states.insert(state.active_tab_id, tab);
    rows(terminal.backend().buffer())
}

/// Paint the attached host's chat pane with the facts `launchpad_facts` builds for it.
pub(crate) fn paint_attached(
    workspace: &Path,
    conversation: &Conversation,
    width: u16,
    height: u16,
) -> Vec<String> {
    let mut tab = TabRenderState {
        launchpad: Some(launchpad_facts::attached_facts(workspace, width)),
        ..TabRenderState::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            chat_pane::render_attached(
                frame,
                Rect::new(0, 0, width, height),
                conversation,
                &StreamingState::default(),
                &ViewState::default(),
                &Theme::dark(),
                &mut tab,
                &BTreeMap::new(),
                &HashMap::new(),
            );
        })
        .unwrap();
    rows(terminal.backend().buffer())
}

/// The launchpad column's left edge for a `width`-wide area.
pub(crate) fn x0(width: u16) -> usize {
    (usize::from(width) - usize::from(width).saturating_sub(2).min(72)) / 2
}

/// The painted rows of the launchpad column, indent removed, trailing blanks
/// trimmed and the blank rows kept (empty strings).
pub(crate) fn column(painted: &[String], width: u16) -> Vec<String> {
    let skip = x0(width);
    let mut out: Vec<String> = painted
        .iter()
        .map(|row| row.chars().skip(skip).collect::<String>())
        .collect();
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

pub(crate) fn git_workspace(branch_line: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/HEAD"), branch_line).unwrap();
    dir
}

fn three_recent() -> SessionIndex {
    SessionIndex::build(vec![
        summary("old", "Add the parser", 7_200, 4),
        summary("mid", "Fix the lexer", 300, 2),
        summary("new", "Refactor config", 30, 6),
    ])
}

fn ws(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

#[test]
fn the_launchpad_at_130x30_lists_every_row_of_the_design() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(130, 30, dir.path(), &[]);
    let painted = paint_local(
        &mut state,
        &empty_conversation("fresh"),
        &three_recent(),
        "claude-opus",
        "anthropic",
        130,
        30,
    );
    assert_eq!(
        column(&painted, 130),
        [
            "Welcome to Rustain.".to_owned(),
            format!("{} · main", ws(&dir)),
            "claude-opus · Anthropic · signed in".to_owned(),
            String::new(),
            "Type a message to start.".to_owned(),
            "  • Ask about this project: \"explain this codebase\"".to_owned(),
            "  • @ Mention a file to add it as context".to_owned(),
            "  • /plan on — plan before changing files".to_owned(),
            String::new(),
            "Recent".to_owned(),
            "  Refactor config · just now".to_owned(),
            "  Fix the lexer · 5m ago".to_owned(),
            "  Add the parser · 2h ago".to_owned(),
            String::new(),
            "^X ? Help · ^P Commands · ^X m Model · ^X h History".to_owned(),
        ]
    );
}

#[test]
fn the_launchpad_at_80x24_drops_the_history_key_it_cannot_open() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(80, 24, dir.path(), &[]);
    let painted = paint_local(
        &mut state,
        &empty_conversation("fresh"),
        &three_recent(),
        "claude-opus",
        "anthropic",
        80,
        24,
    );
    let rows = column(&painted, 80);
    assert_eq!(rows.len(), 15, "every design row fits 80x24: {rows:?}");
    assert_eq!(rows[14], "^X ? Help · ^P Commands · ^X m Model");
    assert!(!painted.iter().any(|row| row.contains("History")));
    assert_eq!(rows[0], "Welcome to Rustain.");
    assert_eq!(rows[4], "Type a message to start.");
}

#[test]
fn the_launchpad_at_60x16_keeps_the_design_inside_the_column() {
    let dir = git_workspace("ref: refs/heads/feature/a-very-long-branch-name-for-this-test\n");
    let mut state = local_state(60, 16, dir.path(), &[]);
    let painted = paint_local(
        &mut state,
        &empty_conversation("fresh"),
        &three_recent(),
        "claude-opus",
        "anthropic",
        60,
        16,
    );
    let rows = column(&painted, 60);
    assert_eq!(rows.len(), 15);
    for row in &rows {
        assert!(
            unicode_width::UnicodeWidthStr::width(row.as_str()) <= 58,
            "{row:?} is wider than the 58-cell column"
        );
    }
    // The workspace row loses its head, never the branch.
    assert!(
        rows[1].ends_with(" · feature/a-very-long-branch-name-for-this-test"),
        "{}",
        rows[1]
    );
    assert_eq!(rows[4], "Type a message to start.");
    assert_eq!(rows[14], "^X ? Help · ^P Commands · ^X m Model");
}

#[test]
fn a_locked_provider_offers_sign_in_from_the_registry_row() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(100, 30, dir.path(), &["openai"]);
    let painted = paint_local(
        &mut state,
        &empty_conversation("fresh"),
        &SessionIndex::new(),
        "gpt-5",
        "openai",
        100,
        30,
    );
    let rows = column(&painted, 100);
    assert_eq!(rows[2], "gpt-5 · OpenAI · not signed in");
    assert_eq!(rows[5], "  • ^X i Sign in to OpenAI");
    // No sessions: no Recent block, and the blank rows collapse around it.
    assert!(!painted.iter().any(|row| row.contains("Recent")));
    // A keyless provider needs no sign-in; an unknown one makes no claim.
    let mut state = local_state(100, 30, dir.path(), &[]);
    let keyless = paint_local(
        &mut state,
        &empty_conversation("fresh"),
        &SessionIndex::new(),
        "llama3",
        "ollama",
        100,
        30,
    );
    assert_eq!(
        column(&keyless, 100)[2],
        "llama3 · Ollama · no sign-in needed"
    );
    assert_eq!(
        column(&keyless, 100)[5],
        "  • Ask about this project: \"explain this codebase\""
    );
    let mut state = local_state(100, 30, dir.path(), &[]);
    let unknown = paint_local(
        &mut state,
        &empty_conversation("fresh"),
        &SessionIndex::new(),
        "m-1",
        "my-gateway",
        100,
        30,
    );
    assert_eq!(column(&unknown, 100)[2], "m-1 · my-gateway");
}

#[test]
fn the_attached_launchpad_has_its_own_variant() {
    let dir = git_workspace("ref: refs/heads/main\n");
    for width in [80u16, 130] {
        let painted = paint_attached(dir.path(), &empty_conversation("fresh"), width, 24);
        assert_eq!(
            column(&painted, width),
            [
                "Welcome to Rustain.".to_owned(),
                format!("{} · main", ws(&dir)),
                "Attached to the workspace daemon".to_owned(),
                String::new(),
                "Type a message to start.".to_owned(),
                "  • Ask about this project: \"explain this codebase\"".to_owned(),
                "  • / Browse commands".to_owned(),
                "  • ^P Open the command palette".to_owned(),
                String::new(),
                "^X ? Help · ^P Commands".to_owned(),
            ],
            "width {width}"
        );
        // `@` and the model/history rows are local-only or sidebar-only.
        let text = painted.join("\n");
        assert!(!text.contains('@') && !text.contains("History") && !text.contains("Model"));
        assert!(!text.contains("Recent"));
    }
}

#[test]
fn fit_keeps_core_rows_and_drops_recent_first() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let welcome = "Welcome to Rustain.";
    let start = "Type a message to start.";
    let keys = "^X ? Help · ^P Commands · ^X m Model";
    let model = "claude-opus · Anthropic · signed in";
    let ask = "  • Ask about this project: \"explain this codebase\"";
    let mention = "  • @ Mention a file to add it as context";
    let workspace = format!("{} · main", ws(&dir));
    let pinned: Vec<(u16, Vec<String>)> = vec![
        (1, vec![welcome.into()]),
        (2, vec![welcome.into(), start.into()]),
        (3, vec![welcome.into(), start.into(), keys.into()]),
        (
            4,
            vec![welcome.into(), model.into(), start.into(), keys.into()],
        ),
        (
            5,
            vec![
                welcome.into(),
                workspace.clone(),
                model.into(),
                start.into(),
                keys.into(),
            ],
        ),
        (
            7,
            vec![
                welcome.into(),
                workspace.clone(),
                model.into(),
                start.into(),
                ask.into(),
                mention.into(),
                keys.into(),
            ],
        ),
        // Ten rows: every starter, then the Recent heading with its newest entry.
        (
            10,
            vec![
                welcome.into(),
                workspace.clone(),
                model.into(),
                start.into(),
                ask.into(),
                mention.into(),
                "  • /plan on — plan before changing files".into(),
                "Recent".into(),
                "  Refactor config · just now".into(),
                keys.into(),
            ],
        ),
    ];
    for (height, expected) in pinned {
        let mut state = local_state(80, height, dir.path(), &[]);
        let painted = paint_local(
            &mut state,
            &empty_conversation("fresh"),
            &three_recent(),
            "claude-opus",
            "anthropic",
            80,
            height,
        );
        assert_eq!(column(&painted, 80), expected, "height {height}");
    }
}

#[test]
fn every_area_paints_inside_itself_and_keeps_the_core_rows_in_order() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let index = three_recent();
    for width in 20..=130u16 {
        for height in 1..=30u16 {
            // A 1-cell margin around the area must stay untouched.
            let (outer_w, outer_h) = (width + 2, height + 2);
            let mut state = local_state(width, height, dir.path(), &[]);
            let conversation = empty_conversation("fresh");
            let clock = MockClock::at_wall_ms(NOW_MS);
            launchpad_facts::refresh(
                &mut state,
                &conversation,
                "claude-opus",
                "anthropic",
                &index,
                &clock,
            );
            let mut tab = state
                .tab_render_states
                .remove(&state.active_tab_id)
                .unwrap();
            let mut terminal = Terminal::new(TestBackend::new(outer_w, outer_h)).unwrap();
            terminal
                .draw(|frame| {
                    chat_pane::render_with_search(
                        frame,
                        Rect::new(1, 1, width, height),
                        &conversation,
                        None,
                        &StreamingState::default(),
                        &ViewState::default(),
                        &clock,
                        0,
                        true,
                        &Theme::dark(),
                        &mut tab,
                        &BTreeMap::new(),
                        None,
                        None,
                        &[],
                        &[],
                        None,
                        None,
                        None,
                        Default::default(),
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            for y in 0..outer_h {
                for x in 0..outer_w {
                    let inside = (1..=width).contains(&x) && (1..=height).contains(&y);
                    if !inside {
                        assert_eq!(
                            buffer[(x, y)].symbol(),
                            " ",
                            "{width}x{height}: painted outside the area at ({x},{y})"
                        );
                    }
                }
            }
            let painted = rows(&buffer);
            let inner: Vec<&str> = painted
                .iter()
                .skip(1)
                .take(usize::from(height))
                .map(String::as_str)
                .collect();
            let find = |needle: &str| inner.iter().position(|row| row.contains(needle));
            // Verbatim whenever the column holds the text; a narrower terminal
            // ellipsizes it (its first word still shows).
            let column = usize::from(width).saturating_sub(2).min(72);
            let whole = |needle: &'static str| {
                if column >= needle.chars().count() {
                    needle
                } else {
                    &needle[..4]
                }
            };
            let welcome = find(whole("Welcome to Rustain."));
            let start = find(whole("Type a message to start."));
            assert!(welcome.is_some(), "{width}x{height}: heading kept");
            if height >= 2 {
                assert!(start.is_some(), "{width}x{height}: wait string kept");
                assert!(welcome < start);
            }
            if height >= 3 {
                let keys = find("^X ? Help").expect("the key line is kept from three rows up");
                assert!(start.unwrap() < keys, "{width}x{height}: order");
            }
            for row in &painted {
                assert!(!row.contains("Ready"), "{width}x{height}: {row:?}");
                assert!(
                    !row.trim_start().starts_with('│'),
                    "{width}x{height}: a gutter-looking row {row:?}"
                );
                assert!(
                    !row.chars().any(|c| "┌┐└┘─│".contains(c)),
                    "{width}x{height}: box glyph in {row:?}"
                );
            }
        }
    }
}

#[test]
fn key_line_paints_only_available_rows() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let index = SessionIndex::new();
    let key_line = |width: u16| {
        let mut state = local_state(width, 30, dir.path(), &[]);
        let painted = paint_local(
            &mut state,
            &empty_conversation("fresh"),
            &index,
            "m",
            "anthropic",
            width,
            30,
        );
        painted
            .iter()
            .find(|row| row.contains("^X ? Help"))
            .map(|row| row.trim().to_owned())
            .expect("a key line")
    };
    // History is a sidebar panel: unavailable below 120 columns, available at 120.
    assert!(!key_line(119).contains("History"));
    assert!(key_line(120).contains("^X h History"));
    assert!(key_line(130).ends_with("^X m Model · ^X h History"));
}

#[test]
fn an_unrefreshed_tab_keeps_the_receipt_bound_rows() {
    // A caller that never refreshed (a preview, a test) still paints the two
    // strings a native driver waits for, and a local key line.
    let mut tab = TabRenderState::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            chat_pane::render_with_search(
                frame,
                Rect::new(0, 0, 80, 24),
                &empty_conversation("fresh"),
                None,
                &StreamingState::default(),
                &ViewState::default(),
                &MockClock::at_wall_ms(NOW_MS),
                0,
                true,
                &Theme::dark(),
                &mut tab,
                &BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                Default::default(),
            );
        })
        .unwrap();
    let painted = rows(terminal.backend().buffer());
    assert_eq!(
        column(&painted, 80),
        [
            "Welcome to Rustain.".to_owned(),
            String::new(),
            "Type a message to start.".to_owned(),
            String::new(),
            "^X ? Help · ^P Commands · ^X m Model".to_owned(),
        ]
    );
}

// ── patch round (review 9-B P13) ───────────────────────────────────────────

fn full_rows(workspace: &str, keys: &str) -> Vec<String> {
    vec![
        "Welcome to Rustain.".to_owned(),
        format!("{workspace} · main"),
        "claude-opus · Anthropic · signed in".to_owned(),
        String::new(),
        "Type a message to start.".to_owned(),
        "  • Ask about this project: \"explain this codebase\"".to_owned(),
        "  • @ Mention a file to add it as context".to_owned(),
        "  • /plan on — plan before changing files".to_owned(),
        String::new(),
        "Recent".to_owned(),
        "  Refactor config · just now".to_owned(),
        "  Fix the lexer · 5m ago".to_owned(),
        "  Add the parser · 2h ago".to_owned(),
        String::new(),
        keys.to_owned(),
    ]
}

#[test]
fn the_launchpad_at_80x24_and_60x16_pins_every_row() {
    let dir = git_workspace("ref: refs/heads/main\n");
    for (width, height) in [(80u16, 24u16), (60, 16)] {
        let mut state = local_state(width, height, dir.path(), &[]);
        let painted = paint_local(
            &mut state,
            &empty_conversation("fresh"),
            &three_recent(),
            "claude-opus",
            "anthropic",
            width,
            height,
        );
        assert_eq!(
            column(&painted, width),
            full_rows(&ws(&dir), "^X ? Help · ^P Commands · ^X m Model"),
            "{width}x{height}"
        );
    }
}

#[test]
fn the_blank_separators_are_added_after_content_in_a_fixed_order() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let full = full_rows(&ws(&dir), "^X ? Help · ^P Commands · ^X m Model");
    // Separators are the lowest priority: first the one after the meta rows
    // (row 4), then the one before the key line (row 14), then the one before
    // Recent (row 9).
    let without = |blanks: &[usize]| -> Vec<String> {
        full.iter()
            .enumerate()
            .filter(|(i, _)| !matches!(i, 3 | 8 | 13) || blanks.contains(i))
            .map(|(_, row)| row.clone())
            .collect()
    };
    let cases: Vec<(u16, Vec<String>)> = vec![
        (11, {
            let mut rows = without(&[]);
            rows.remove(10);
            rows
        }),
        (12, without(&[])),
        (13, without(&[3])),
        (14, without(&[3, 13])),
        (15, without(&[3, 8, 13])),
    ];
    for (height, expected) in cases {
        let mut state = local_state(80, height, dir.path(), &[]);
        let painted = paint_local(
            &mut state,
            &empty_conversation("fresh"),
            &three_recent(),
            "claude-opus",
            "anthropic",
            80,
            height,
        );
        assert_eq!(column(&painted, 80), expected, "height {height}");
    }
}

#[test]
fn no_row_is_wider_than_the_column_for_any_width_or_provider_state() {
    let dir = git_workspace("ref: refs/heads/a-fairly-long-branch-name-to-force-ellipsis\n");
    for width in 20..=130u16 {
        let column_width = usize::from(width).saturating_sub(2).min(72);
        for (locked, provider) in [(&[][..], "anthropic"), (&["openai"][..], "openai")] {
            let mut state = local_state(width, 30, dir.path(), locked);
            let painted = paint_local(
                &mut state,
                &empty_conversation("fresh"),
                &three_recent(),
                "a-model-with-a-rather-long-name-for-the-column",
                provider,
                width,
                30,
            );
            let attached = paint_attached(dir.path(), &empty_conversation("fresh"), width, 30);
            for rows in [&painted, &attached] {
                for row in column(rows, width) {
                    assert!(
                        unicode_width::UnicodeWidthStr::width(row.as_str()) <= column_width,
                        "{width}: {row:?} exceeds the {column_width}-cell column"
                    );
                }
            }
        }
    }
}
