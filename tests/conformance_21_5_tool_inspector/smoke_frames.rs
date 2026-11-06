//! Portable predicate controls: only actual owning-runtime paints are exported.
use super::support::*;
use rustain::adapters::tui::{
    app::InputAction,
    keymap::Key,
    notifications::{self, NoticeSource},
    tool_inspector::layout,
    widgets::chat_pane,
};
use rustain::domain::models::{InvocationStatus, Turn, WriteDiffState};
use serde_json::json;
use std::path::PathBuf;

const COMMAND: &str = "seq 1 60 | sed 's/^/row /'";
const FULL: &str = "1\n2\n3\n4\n5\n6\n7\n8";

fn capture(f: &mut Fixture, stage: &str, full: bool, dir: &Option<PathBuf>) -> String {
    f.paint();
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
        let parts = layout::parts(rect);
        assert!(parts.body.height >= 1);
        assert!(row(f.buffer(), parts.tabs, parts.tabs.y).contains('['));
        assert!(row(f.buffer(), parts.rail, parts.rail.y).starts_with("[Esc] Back"));
        assert!(rect.bottom() <= shell.status_bar.y);
    }
    if let Some(dir) = dir {
        let record = json!({
            "width": f.h.state.terminal_width, "height": f.h.state.terminal_height,
            "stage": stage, "full": full, "draft": f.h.state.input_buffer,
            "screen": screen, "cursor_hidden": !f.h.terminal.backend().cursor_visible(),
        });
        let name = format!(
            "a-{}x{}-{}-{stage}.json",
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
