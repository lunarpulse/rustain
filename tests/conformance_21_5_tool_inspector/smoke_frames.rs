//! Portable predicate controls: only actual owning-runtime paints are exported.
use super::support::*;
use rustain::adapters::tui::{
    app::InputAction,
    clipboard::ClipboardResult,
    handlers::{
        clipboard, scroll,
        tool_focus::{CycleOutcome, cycle_item},
    },
    keymap::Key,
    notifications::{self, NoticeSeverity, NoticeSource},
    presentation::Surface,
    tool_inspector::{self, layout},
    widgets::chat_pane,
};
use rustain::domain::models::{
    AnchorMode, InvocationStatus, StopReason, Turn, TurnPart, WriteDiffState,
    view_state::ScrollDelta,
};
use serde_json::json;
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    time::Duration,
};

const COMMAND: &str = "seq 1 60 | sed 's/^/row /'";
const FULL: &str = "1\n2\n3\n4\n5\n6\n7\n8";

fn capture(f: &mut Fixture, stage: &str, full: bool, dir: &Option<PathBuf>) -> String {
    f.paint();
    let (leg, stage) = stage
        .split_once('-')
        .filter(|(leg, _)| matches!(*leg, "b" | "c"))
        .unwrap_or(("a", stage));
    let area = f.buffer().area;
    let screen = (area.y..area.bottom())
        .map(|y| row(f.buffer(), area, y))
        .collect::<Vec<_>>()
        .join("\n");
    let lines: Vec<_> = screen.split('\n').collect();
    let shell = f.layout();
    assert!(
        lines[usize::from(shell.status_bar.y)].contains("Ready"),
        "{stage}: {screen}"
    );
    assert!(lines.last().unwrap().starts_with('└'));
    assert_eq!(lines.len(), usize::from(area.height), "{stage}");
    assert!(
        lines
            .iter()
            .all(|line| line.chars().count() == usize::from(area.width))
    );
    assert_eq!(f.h.state.input_buffer, if full { FULL } else { "" });
    let composer = shell.input_area;
    assert!(lines[usize::from(composer.y)].starts_with("┌ Message "));
    let draft_rect = composer.inner(ratatui::layout::Margin::new(1, 1));
    let draft: Vec<_> = (draft_rect.y..draft_rect.bottom())
        .map(|y| row(f.buffer(), draft_rect, y).trim().to_owned())
        .collect();
    assert_eq!(
        draft,
        if full {
            FULL.split('\n').collect::<Vec<_>>()
        } else {
            vec![""]
        }
    );
    assert!(
        !f.h.terminal.backend().cursor_visible(),
        "{stage}: hidden Chat/frame cursor"
    );
    if !f.h.state.modal_stack.is_empty() {
        let rect = f.frame_rect();
        assert_eq!(f.buffer()[(rect.x, rect.y)].symbol(), "┌");
        assert_eq!(
            f.buffer()[(rect.right() - 1, rect.bottom() - 1)].symbol(),
            "┘"
        );
        match f.h.state.modal_stack.top().unwrap().surface {
            Surface::ToolInspector(_) => {
                let parts = layout::parts(rect);
                assert!(parts.body.height >= 1);
                assert!(row(f.buffer(), parts.tabs, parts.tabs.y).contains('['));
                assert!(row(f.buffer(), parts.rail, parts.rail.y).starts_with("[Esc] Back"));
            }
            Surface::NotificationLog => {
                assert!(
                    row(f.buffer(), rect, rect.y)
                        .starts_with("┌ Tool · Bash #1 › System · Notifications ")
                );
                assert!(screen.contains("Notifications (1)"));
                assert!(screen.contains("clipboard · Copied to "));
                assert!(screen.contains("[Enter] Detail"));
                assert!(screen.contains("[c] Copy"));
            }
            other => panic!("unexpected exported surface: {other:?}"),
        }
        assert!(rect.bottom() <= shell.status_bar.y);
    }
    if let Some(dir) = dir {
        let record = json!({
            "leg": leg, "width": f.h.state.terminal_width, "height": f.h.state.terminal_height,
            "stage": stage, "full": full, "draft": f.h.state.input_buffer,
            "screen": screen, "cursor_hidden": !f.h.terminal.backend().cursor_visible(),
        });
        let name = format!(
            "{leg}-{}x{}-{}-{stage}.json",
            f.h.state.terminal_width,
            f.h.state.terminal_height,
            if full { "full" } else { "empty" }
        );
        std::fs::write(dir.join(name), serde_json::to_vec(&record).unwrap()).unwrap();
    }
    screen
}

fn key(f: &mut Fixture, key: Key, stage: &str, full: bool, dir: &Option<PathBuf>) -> String {
    f.press(key);
    capture(f, stage, full, dir)
}

#[test]
fn production_smoke_frames_cover_leg_a_at_every_geometry() {
    // Without an export opt-in this still performs every real paint/assertion.
    let dir = std::env::var_os("RUSTAIN_STORY_21_5_FRAME_DIR").map(PathBuf::from);
    if let Some(dir) = &dir {
        assert!(dir.is_absolute());
        assert!(
            !dir.components()
                .any(|c| c == std::path::Component::ParentDir)
        );
        std::fs::create_dir_all(dir).unwrap();
    }
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let mut turn = Turn::new("fixture-model".into(), 0);
            let output = (1..=60)
                .map(|n| format!("row {n}"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let bash = call(
                &mut turn,
                "Bash",
                json!({"command": COMMAND}),
                InvocationStatus::Success,
                Some((&output, WriteDiffState::NotAWrite)),
            );
            prose(&mut turn, &rows(28));
            let edit = call(
                &mut turn,
                "Edit",
                json!({"file_path":"notes.txt","old_string":"alpha","new_string":"beta"}),
                InvocationStatus::Success,
                Some(("edited notes.txt", WriteDiffState::NotAWrite)),
            );
            prose(&mut turn, &rows(28));
            call(
                &mut turn,
                "Write",
                json!({"file_path":"fresh.txt","content":"line one\nline two\n"}),
                InvocationStatus::Success,
                Some(("wrote fresh.txt", WriteDiffState::NewFile)),
            );
            prose(&mut turn, "Inspected three tools.");
            let mut f = Fixture::new(width, height, turn);
            f.h.state.input_buffer = if full { FULL.into() } else { String::new() };
            f.h.state.cursor_position = f.h.state.input_buffer.chars().count();
            // Same real zR command/effect as event_loop; do not change legacy folding.
            f.press(Key::Char('z'));
            assert_eq!(f.press(Key::Char('R')), InputAction::ExpandAllTurns);
            let turns = f.conversation().turns.clone();
            let tab_id = f.tabs.active_tab_id();
            let tab = f.tabs.active_tab_mut();
            let render = f.h.state.tab_render_states.get_mut(&tab_id).unwrap();
            chat_pane::expand_all_turns(&mut tab.view_state, render, &turns);
            capture(&mut f, "completed", full, &dir);
            f.focus_call(&bash);
            let before = capture(&mut f, "stage", full, &dir);
            notifications::info(
                &mut f.h.state,
                NoticeSource::System,
                "INSPECTOR NOTICE CONTROL",
            );
            let notice = capture(&mut f, "stage-notice", full, &dir);
            assert!(notice.contains("Info INSPECTOR NOTICE CONTROL"));
            assert_ne!(
                notice, before,
                "the real toast painter changes Stage pixels"
            );
            assert_eq!(f.h.state.notifications.clear_toasts(), 1);
            f.open();
            assert_eq!(
                f.h.state.modal_stack.top().unwrap().title(),
                "Tool · Bash #1"
            );
            capture(&mut f, "output", full, &dir);
            assert!(f.body_row(0).trim() == "row 1");
            key(&mut f, Key::Left, "left-end", full, &dir);
            for (keycode, stage) in [
                (Key::Char('j'), "line-down"),
                (Key::Char('k'), "line-up"),
                (Key::Ctrl('d'), "half-down"),
                (Key::Ctrl('u'), "half-up"),
                (Key::PageDown, "page-down"),
                (Key::PageUp, "page-up"),
                (Key::Char('G'), "tail"),
                (Key::Char('g'), "top"),
            ] {
                key(&mut f, keycode, stage, full, &dir);
                if stage == "tail" {
                    if f.body_rect().height == 1 {
                        key(&mut f, Key::Char('k'), "tail-row60", full, &dir);
                    }
                    assert!((0..f.body_rect().height).any(|n| f.body_row(n).trim() == "row 60"));
                }
            }
            key(&mut f, Key::Right, "input", full, &dir);
            key(&mut f, Key::Char('j'), "input-command", full, &dir);
            assert!((0..f.body_rect().height).any(|n| f.body_row(n).contains("\"command\"")));
            let raw = key(&mut f, Key::Right, "raw", full, &dir);
            if width == 80 && !full {
                assert!(
                    (0..f.body_rect().height).any(|n| f.body_row(n).contains("\"tool\": \"Bash\""))
                );
            }
            assert_eq!(key(&mut f, Key::Right, "raw-end", full, &dir), raw);
            // A one-row body cannot show the complete JSON identity at once.
            f.press(Key::Char('g'));
            f.paint();
            for _ in 0..2 {
                f.press(Key::Char('j'));
                f.paint();
            }
            capture(&mut f, "raw-tool", full, &dir);
            assert!(
                (0..f.body_rect().height).any(|n| f.body_row(n).contains("\"tool\": \"Bash\""))
            );
            key(&mut f, Key::Char('G'), "raw-tail", full, &dir);
            key(&mut f, Key::Left, "input-return", full, &dir);
            key(&mut f, Key::Left, "output-return", full, &dir);
            let back = key(&mut f, Key::Esc, "back", full, &dir);
            assert_eq!(back, before, "Back restores exact Stage/composer glyphs");
            assert_eq!(key(&mut f, Key::Char('p'), "p-inert", full, &dir), before);
            f.focus_call(&edit);
            let edit_before = capture(&mut f, "edit-stage", full, &dir);
            f.open();
            assert_eq!(
                f.h.state.modal_stack.top().unwrap().title(),
                "Tool · Edit #2"
            );
            capture(&mut f, "edit-output", full, &dir);
            key(&mut f, Key::Right, "edit-input", full, &dir);
            key(&mut f, Key::Right, "edit-raw", full, &dir);
            key(&mut f, Key::Right, "diff", full, &dir);
            assert!(f.body_row(0).starts_with("- alpha"));
            if f.body_rect().height > 1 {
                assert!(f.body_row(1).starts_with("+ beta"));
            }
            key(&mut f, Key::Char('G'), "diff-tail", full, &dir);
            assert!((0..f.body_rect().height).any(|n| f.body_row(n).starts_with("+ beta")));
            key(&mut f, Key::Char('g'), "diff-top", full, &dir);
            assert_eq!(key(&mut f, Key::Esc, "edit-back", full, &dir), edit_before);
            f.open();
            assert_eq!(
                key(&mut f, Key::Ctrl('c'), "ctrl-c-back", full, &dir),
                edit_before
            );
        }
    }
}

fn output() -> String {
    (1..=60).map(|n| format!("row {n}\n")).collect()
}

/// Each leg owns a fresh canonical conversation, manager, runtime and clock.
fn launch(width: u16, height: u16, full: bool, spaced: bool) -> (Fixture, [String; 3]) {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let bash = call(
        &mut turn,
        "Bash",
        json!({"command": COMMAND}),
        InvocationStatus::Success,
        Some((&output(), WriteDiffState::NotAWrite)),
    );
    if spaced {
        prose(&mut turn, &rows(28));
    }
    let edit = call(
        &mut turn,
        "Edit",
        json!({"file_path":"notes.txt","old_string":"alpha","new_string":"beta"}),
        InvocationStatus::Success,
        Some(("edited notes.txt", WriteDiffState::NotAWrite)),
    );
    if spaced {
        prose(&mut turn, &rows(28));
    }
    let write = call(
        &mut turn,
        "Write",
        json!({"file_path":"fresh.txt","content":"line one\nline two\n"}),
        InvocationStatus::Success,
        Some(("wrote fresh.txt", WriteDiffState::NewFile)),
    );
    prose(&mut turn, "Inspected three tools.");
    turn.stop_reason = Some(StopReason::EndTurn);
    let mut f = Fixture::new(width, height, turn);
    f.h.state.input_buffer = if full { FULL.into() } else { String::new() };
    f.h.state.cursor_position = f.h.state.input_buffer.chars().count();
    f.press(Key::Char('z'));
    assert_eq!(f.press(Key::Char('R')), InputAction::ExpandAllTurns);
    let turns = f.tabs.active_tab().conversation.turns.clone();
    let tab_id = f.tabs.active_tab_id();
    chat_pane::expand_all_turns(
        &mut f.tabs.active_tab_mut().view_state,
        f.h.state.tab_render_states.get_mut(&tab_id).unwrap(),
        &turns,
    );
    f.paint();
    // gg reaches the same owning-view scroll reducer used by the runtime.
    // Reading mode means the public scroll-intent entry has no anchor gate.
    for _ in 0..2 {
        match f.press(Key::Char('g')) {
            InputAction::ScrollToTop => {
                scroll::handle_apply_scroll_intent(
                    &mut f.tabs,
                    &mut f.h.state,
                    &f.h.conversation,
                    ScrollDelta::Top,
                );
            }
            InputAction::Consumed => {}
            other => panic!("Stage gg emitted {other:?}"),
        }
    }
    f.paint();
    // gg scrolls, but correctly retains any still-visible current header. Load
    // the fresh owning tab's unselected launch state instead of pinning a tool.
    assert!(f.tabs.active_tab().focused_item_id.is_none());
    let launch_view = f.view();
    f.load();
    assert_eq!(
        f.view(),
        launch_view,
        "the owning launch view is current before load"
    );
    assert!(f.h.state.focused_item_id.is_none());
    f.h.state.input_buffer = if full { FULL.into() } else { String::new() };
    f.h.state.cursor_position = f.h.state.input_buffer.chars().count();
    if f.h.state.focus == rustain::domain::models::FocusState::Input {
        assert_eq!(f.press(Key::Esc), InputAction::Consumed);
    }
    f.paint();
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(bash.as_str()));
    assert!(f.tabs.active_tab().view_state.focused_turn.is_none());
    (f, [bash, edit, write])
}

fn stage_tab(f: &mut Fixture, wanted: &str) {
    assert_eq!(f.press(Key::Tab), InputAction::CycleInvocationInFocusedTurn);
    let active = f.tabs.active_tab_mut();
    let CycleOutcome::Cycled { item_id } = cycle_item(
        &mut f.h.state,
        &mut active.view_state,
        &active.conversation.turns,
        active.reducer.open_turn.as_ref(),
    ) else {
        panic!("real Stage Tab must cycle this qualifying turn");
    };
    assert_eq!(item_id, wanted);
    f.h.state.select_item_explicitly(item_id);
    f.h.state.set_scroll_offset(active.view_state.scroll_offset);
    f.h.state
        .set_auto_scroll(matches!(active.view_state.mode, AnchorMode::Following));
    f.h.state.needs_redraw = true;
    f.paint();
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(wanted));
    assert_eq!(
        f.tabs.active_tab().view_state.focused_turn.as_ref(),
        Some(&f.conversation().turns[0].id)
    );
}

fn marked(f: &Fixture, name: &str) {
    let rect = f.layout().chat_pane;
    let header = (rect.y..rect.bottom())
        .map(|y| row(f.buffer(), rect, y))
        .find(|line| line.contains("▸ "))
        .expect("actual focused header marker");
    assert!(header.contains(name), "{header}");
    // The marker replaces the original lead, after any focused-turn gutter.
    assert_eq!(header.matches("▸ ").count(), 1);
    let cue = super::k9_marker::cue();
    let trimmed = header.trim_end();
    let suffix = format!(" {cue}");
    let base = trimmed.strip_suffix(&suffix).unwrap_or(trimmed);
    let fits = rustain::adapters::tui::text_geometry::cells(base)
        + 1
        + rustain::adapters::tui::text_geometry::cells(&cue)
        <= usize::from(rect.width);
    assert_eq!(
        trimmed.ends_with(&suffix),
        fits,
        "A10 complete-fit cue: {header}"
    );
    assert!(
        !trimmed.contains("[E") || trimmed.ends_with(&suffix),
        "no partial cue: {header}"
    );
    if f.h.state.terminal_width == 80 && name == "Bash" {
        assert!(
            fits,
            "the initial Bash fixture is the fitting 80-column control"
        );
    }
}

fn title(f: &Fixture, expected: &str) {
    assert_eq!(f.h.state.modal_stack.top().unwrap().title(), expected);
}

fn query(f: &mut Fixture, text: &str) {
    assert_eq!(f.press(Key::Char('/')), InputAction::Consumed);
    for ch in text.chars() {
        assert_eq!(f.press(Key::Char(ch)), InputAction::Consumed);
    }
}

fn search_status(f: &Fixture, expected: &str) {
    let parts = layout::parts(f.frame_rect());
    let rect = parts.status.unwrap_or(parts.tabs);
    assert!(
        row(f.buffer(), rect, rect.y).contains(expected),
        "{}",
        row(f.buffer(), rect, rect.y)
    );
}

fn copy_file(f: &mut Fixture, path: &Path) {
    let attempts = Cell::new(0);
    let copier = |text: &str| {
        attempts.set(attempts.get() + 1);
        std::fs::write(path, text.as_bytes()).unwrap();
        ClipboardResult::FallbackSuccess(path.to_owned())
    };
    let action = f.press(Key::Char('c'));
    let active = f.tabs.active_tab();
    match action {
        InputAction::CopyToolSource => assert!(tool_inspector::copy::copy_source(
            &mut f.h.state,
            &active.conversation,
            Some(&active.reducer),
            &copier,
        )),
        InputAction::CopyToClipboard(content) => {
            clipboard::requested(&mut f.h.state, &active.conversation, content, &copier)
        }
        other => panic!("copy decoder emitted {other:?}"),
    }
    assert_eq!(attempts.get(), 1);
    assert_eq!(std::fs::read(path).unwrap(), output().as_bytes());
    let notice = f.h.state.notifications.records().last().unwrap();
    assert_eq!(notice.source, NoticeSource::Clipboard);
    assert_eq!(notice.severity, NoticeSeverity::Success);
    assert_eq!(notice.message, format!("Copied to {}", path.display()));
}

fn leg_b(width: u16, height: u16, full: bool, dir: &Option<PathBuf>) {
    let (mut f, ids) = launch(width, height, full, false);
    capture(&mut f, "b-stage", full, dir);
    marked(&f, "Bash");
    stage_tab(&mut f, &ids[1]);
    let before = capture(&mut f, "b-tab-edit", full, dir);
    marked(&f, "Edit");
    let view = f.view();
    let cache = f.cache();
    f.open();
    title(&f, "Tool · Edit #2");
    capture(&mut f, "b-edit-output", full, dir);
    assert!(f.body_row(0).starts_with("edited notes.txt"));
    assert!(f.step(Key::Char(']')));
    capture(&mut f, "b-write-output", full, dir);
    title(&f, "Tool · Write #3");
    for _ in 0..3 {
        f.press(Key::Right);
    }
    let last = capture(&mut f, "b-write-diff", full, dir);
    assert!(f.body_row(0).starts_with("+ line one"));
    assert!(!f.step(Key::Char(']')));
    assert_eq!(capture(&mut f, "b-write-boundary", full, dir), last);
    assert!(f.step(Key::Char('[')));
    capture(&mut f, "b-edit-diff", full, dir);
    title(&f, "Tool · Edit #2");
    assert!(f.body_row(0).starts_with("- alpha"));
    assert!(f.step(Key::Char('[')));
    let first = capture(&mut f, "b-bash-output", full, dir);
    title(&f, "Tool · Bash #1");
    assert_eq!(f.body_row(0).trim(), "row 1");
    assert!(!f.step(Key::Char('[')));
    assert_eq!(capture(&mut f, "b-bash-boundary", full, dir), first);
    assert_eq!(f.view(), view);
    assert_eq!(f.cache(), cache);
    assert_eq!(key(&mut f, Key::Esc, "b-back", full, dir), before);
    marked(&f, "Edit");
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(ids[1].as_str()));

    // Same actual calls, with real prose between them: Tab must reveal a header
    // below the viewport, not merely change an invisible selected ID.
    let (mut f, ids) = launch(width, height, full, true);
    let before = capture(&mut f, "b-offscreen-stage", full, dir);
    assert!(!before.contains("Edit"));
    let offset = f.tabs.active_tab().view_state.scroll_offset;
    stage_tab(&mut f, &ids[1]);
    let after = capture(&mut f, "b-offscreen-tab", full, dir);
    assert_ne!(f.tabs.active_tab().view_state.scroll_offset, offset);
    let chat = f.layout().chat_pane;
    assert!(row(f.buffer(), chat, chat.y).contains("▸ "));
    marked(&f, "Edit");
    f.open();
    title(&f, "Tool · Edit #2");
    assert_eq!(key(&mut f, Key::Esc, "b-offscreen-back", full, dir), after);
}

fn leg_c(width: u16, height: u16, full: bool, dir: &Option<PathBuf>, files: &Path) {
    let (mut f, _) = launch(width, height, full, false);
    let before = capture(&mut f, "c-stage", full, dir);
    let transcript = serde_json::to_vec(f.conversation()).unwrap();
    let view = f.view();
    let cache = f.cache();
    f.open();
    capture(&mut f, "c-output", full, dir);
    query(&mut f, "row 4");
    f.press(Key::Char('j'));
    assert_eq!(f.tool().search.editor.query.as_str(), "row 4j");
    f.press(Key::Backspace);
    capture(&mut f, "c-query-field", full, dir);
    assert!(f.tool().search.editor.editing);
    assert_eq!(f.tool().viewers[0].top, 0, "typing cannot scroll the body");
    key(&mut f, Key::Enter, "c-search1", full, dir);
    search_status(&f, "Search 1/11");
    assert_eq!(f.body_row(0).trim(), "row 4");
    key(&mut f, Key::Char('n'), "c-search2", full, dir);
    search_status(&f, "Search 2/11");
    assert_eq!(f.body_row(0).trim(), "row 40");
    key(&mut f, Key::Char('w'), "c-wrap-off", full, dir);
    assert!(!f.tool().wrap);
    assert_eq!(f.body_row(0).trim(), "row 40");
    let path = files.join(format!(
        "c-{width}x{height}-{}-clipboard.txt",
        if full { "full" } else { "empty" }
    ));
    let sheet = pixels(f.buffer(), f.frame_rect());
    copy_file(&mut f, &path);
    capture(&mut f, "c-copied", full, dir);
    assert_eq!(
        pixels(f.buffer(), f.frame_rect()),
        sheet,
        "the existing opaque sheet does not paint the underlying copy toast"
    );
    f.press(Key::Ctrl('x'));
    f.press(Key::Char('n'));
    capture(&mut f, "c-copy-notifications", full, dir);
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::NotificationLog
    ));
    key(&mut f, Key::Esc, "c-copy-return", full, dir);
    assert_eq!(pixels(f.buffer(), f.frame_rect()), sheet);
    // The real expiry path uses the fixture's injected monotonic clock.
    f.clock.advance(Duration::from_secs(3600));
    f.press(Key::Char('/'));
    key(&mut f, Key::Esc, "c-cleared", full, dir);
    assert!(f.tool().search.query().is_empty());
    assert_eq!(key(&mut f, Key::Esc, "c-back", full, dir), before);
    assert_eq!(f.view(), view);
    assert_eq!(f.cache(), cache);
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);

    let (mut f, _) = launch(width, height, full, false);
    let before = capture(&mut f, "c-control-stage", full, dir);
    f.open();
    let body_width = usize::from(f.body_rect().width);
    let source = format!(
        "{}ALPHA\nquiet control\n{}",
        "x".repeat(body_width - 2),
        rows(40)
    );
    replace_output(&mut f, &source);
    capture(&mut f, "c-split-output", full, dir);
    assert!(f.body_row(0).ends_with("AL"));
    query(&mut f, "alpha");
    capture(&mut f, "c-split-field", full, dir);
    key(&mut f, Key::Enter, "c-split-match", full, dir);
    search_status(&f, "Search 1/1");
    assert!(f.body_row(0).ends_with("AL"));
    let body = f.body_rect();
    for x in body.right() - 2..body.right() {
        let cell = &f.buffer()[(x, body.y)];
        assert_eq!(
            cell.style(),
            cell.style().patch(f.h.state.theme.search_highlight_focused)
        );
    }
    key(&mut f, Key::Char('j'), "c-split-next", full, dir);
    assert!(f.body_row(0).starts_with("PHA"));
    for x in 0..3 {
        let cell = &f.buffer()[(body.x + x, body.y)];
        assert_eq!(
            cell.style(),
            cell.style().patch(f.h.state.theme.search_highlight_focused)
        );
    }
    f.press(Key::Char('/'));
    f.press(Key::Esc);
    query(&mut f, "missing");
    key(&mut f, Key::Enter, "c-split-no-match", full, dir);
    search_status(&f, "Search: no match");
    assert_eq!(f.tool().viewers[0].top, 1);
    assert_eq!(f.body_row(0).trim(), "PHA");
    f.press(Key::Char('/'));
    key(&mut f, Key::Esc, "c-split-clear", full, dir);
    key(&mut f, Key::Char('w'), "c-split-wrap-off", full, dir);
    assert!(f.body_row(0).ends_with('→'));
    f.press(Key::Char('w'));
    key(&mut f, Key::Char('g'), "c-split-wrap-on", full, dir);
    assert!(f.body_row(0).ends_with("AL"));
    replace_output(
        &mut f,
        &format!("changed source\nquiet control\n{}", rows(40)),
    );
    key(&mut f, Key::Char('g'), "c-changed-source", full, dir);
    assert!(f.body_row(0).starts_with("changed source"));
    assert_eq!(key(&mut f, Key::Esc, "c-control-back", full, dir), before);
}

fn replace_output(f: &mut Fixture, text: &str) {
    let active = f.tabs.active_tab_mut();
    let output = active.conversation.turns[0]
        .parts
        .iter_mut()
        .find_map(|part| match part {
            TurnPart::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    output.content = text.into();
    active.conversation.rebuild_messages_mirror();
}

#[test]
fn production_smoke_frames_cover_legs_b_c_at_every_geometry() {
    let dir = std::env::var_os("RUSTAIN_STORY_21_5_FRAME_DIR").map(PathBuf::from);
    if let Some(dir) = &dir {
        assert!(dir.is_absolute());
        assert!(
            !dir.components()
                .any(|c| c == std::path::Component::ParentDir)
        );
        std::fs::create_dir_all(dir).unwrap();
    }
    let temporary = tempfile::tempdir().unwrap();
    let files = dir.as_deref().unwrap_or(temporary.path());
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            leg_b(width, height, full, &dir);
            leg_c(width, height, full, &dir, files);
        }
    }
}
