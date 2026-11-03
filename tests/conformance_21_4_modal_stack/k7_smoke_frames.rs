//! Bb1a smoke predicate controls: complete production frames, never native captures.
use super::keymap_support::press;
use crate::e2e_harness::TestHarness;
use rustain::adapters::tui::{
    handlers::search,
    keymap::Key,
    presentation::{self, Surface},
};
use rustain::domain::models::{StopReason, StreamChunk, TabManager};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

const DRAFT: &str = "A1-kept-draft";
const TURN: &str = "A2b-held-budget";
const REPLY: &str = "A2b budget completion";

fn key(h: &mut TestHarness, tabs: &mut TabManager, key: Key) {
    let action = press(&mut h.state, key);
    let conversation = tabs.active_tab().conversation.clone();
    search::handle_search_action(tabs, &mut h.state, &conversation, &action);
}

fn text(h: &mut TestHarness, tabs: &mut TabManager, text: &str) {
    for c in text.chars() {
        key(h, tabs, Key::Char(c));
    }
}

fn open(h: &mut TestHarness, tabs: &mut TabManager, full: bool) {
    if full {
        // Full-composer cases are real-render geometry fixtures through the
        // production reducer/effect seam. Ctrl+F intentionally requires empty
        // Input; Chat Ctrl+F is page-down, not an alternate Search shortcut.
        presentation::reducer::open(&mut h.state, Surface::Search);
        let conversation = tabs.active_tab().conversation.clone();
        search::handle_search_action(
            tabs,
            &mut h.state,
            &conversation,
            &rustain::adapters::tui::app::InputAction::OpenSearch,
        );
    } else {
        key(h, tabs, Key::Ctrl('f'));
    }
}

fn output_dir() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("RUSTAIN_STORY_21_4_FRAME_DIR")?);
    // Canonicalize after creation as well: a symlink cannot escape scratch.
    let scratch = std::fs::canonicalize("/home/lunarpulse/.omp-sessions/21-4")
        .expect("session scratch exists for explicit frame export");
    assert!(path.is_absolute() && path.starts_with(&scratch));
    assert!(
        !path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    );
    let ancestor = path.ancestors().find(|p| p.exists()).unwrap();
    assert!(
        std::fs::canonicalize(ancestor)
            .unwrap()
            .starts_with(&scratch)
    );
    std::fs::create_dir_all(&path).unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    assert!(path.starts_with(scratch));
    Some(path)
}

fn capture(
    h: &mut TestHarness,
    tabs: &TabManager,
    stage: &str,
    full: bool,
    dir: &Option<PathBuf>,
) -> String {
    h.render_active_tab(tabs);
    let buffer = h.terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    let screen = rows.join("\n");
    assert!(
        rows.iter().any(|row| row.contains("Ready")),
        "complete status: {screen}"
    );
    assert!(rows.last().unwrap().starts_with('└'));
    assert_eq!(
        h.terminal.backend().cursor_visible(),
        h.state.modal_stack.is_empty()
            && h.state.focus == rustain::domain::models::FocusState::Input
    );
    if let Some(dir) = dir {
        let cursor = h.terminal.get_cursor_position().unwrap();
        let record = serde_json::json!({
            "width": h.state.terminal_width, "height": h.state.terminal_height,
            "stage": stage, "full": full, "screen": screen,
            "draft": h.state.input_buffer, "cursor": [cursor.x, cursor.y],
            "cursor_hidden": !h.terminal.backend().cursor_visible(),
        });
        let name = format!(
            "bb1a-{}x{}-{}-{stage}.json",
            h.state.terminal_width,
            h.state.terminal_height,
            if full { "full" } else { "normal" }
        );
        std::fs::write(dir.join(name), serde_json::to_vec(&record).unwrap()).unwrap();
    }
    screen
}

#[test]
fn bb1a_real_frames_for_smoke_predicates() {
    let dir = output_dir();
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let mut h = TestHarness::with_size(width, height);
            // Markdown soft breaks join; hard breaks preserve sixty real rows.
            let reply = (0..60)
                .map(|row| format!("Bb1a-row-{row:02}  \n"))
                .collect::<String>()
                + REPLY;
            h.complete_turn(
                TURN,
                vec![
                    StreamChunk::Text {
                        content: reply,
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            );
            let mut tabs = TabManager::new(CancellationToken::new());
            tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
            h.state.active_tab_id = tabs.active_tab().id;
            let draft = if full { "1\n2\n3\n4\n5\n6\n7\n8" } else { "" };
            h.state.input_buffer = draft.into();
            h.state.cursor_position = draft.chars().count();
            let before = capture(&mut h, &tabs, "before", full, &dir);
            let view = format!("{:?}", tabs.active_tab().view_state);
            open(&mut h, &mut tabs, full);
            assert!(presentation::is_open(&h.state, Surface::Search));
            capture(&mut h, &tabs, "search-unfiltered", full, &dir);
            text(&mut h, &mut tabs, "zzzz");
            assert!(h.state.search_state.matches.is_empty());
            capture(&mut h, &tabs, "search-empty", full, &dir);
            key(&mut h, &mut tabs, Key::Esc);
            for keep in [false, true] {
                open(&mut h, &mut tabs, full);
                text(&mut h, &mut tabs, "A2b");
                assert_eq!(h.state.search_state.matches.len(), 2);
                capture(&mut h, &tabs, "search-filtered", full, &dir);
                key(&mut h, &mut tabs, Key::Enter);
                capture(&mut h, &tabs, "search-navigating-1", full, &dir);
                key(&mut h, &mut tabs, Key::Char('n'));
                assert_eq!(h.state.search_state.focused_match_index, 1);
                let matched_view = format!("{:?}", tabs.active_tab().view_state);
                assert_ne!(matched_view, view, "long assistant jump must leave bottom");
                let matched = capture(&mut h, &tabs, "search-navigating-2", full, &dir);
                assert!(matched.contains("Bb1a-row-01") && !matched.contains(REPLY));
                key(&mut h, &mut tabs, if keep { Key::Enter } else { Key::Esc });
                assert!(!presentation::is_open(&h.state, Surface::Search));
                let after = capture(
                    &mut h,
                    &tabs,
                    if keep {
                        "search-kept"
                    } else {
                        "search-restored"
                    },
                    full,
                    &dir,
                );
                if keep {
                    assert_eq!(format!("{:?}", tabs.active_tab().view_state), matched_view);
                    assert_ne!(after, before);
                    assert!(after.contains("Bb1a-row-01") && !after.contains(REPLY));
                } else {
                    assert_eq!(format!("{:?}", tabs.active_tab().view_state), view);
                    assert_eq!(after, before);
                }
                assert_eq!(h.state.input_buffer, draft);
            }
            h.focus_input();
            let draft = if full { draft } else { DRAFT };
            h.state.input_buffer = draft.into();
            h.state.cursor_position = draft.chars().count();
            // Match the native notice commands, including their slash delimiter.
            for entry in [
                "/a1-first-invalid ",
                "/a1-second-invalid ",
                "/a1-third-invalid ",
                TURN,
            ] {
                h.state.input_history.push(entry.into());
            }
            key(&mut h, &mut tabs, Key::Ctrl('r'));
            capture(&mut h, &tabs, "reverse-open", full, &dir);
            text(&mut h, &mut tabs, "a1-");
            assert_eq!(h.state.reverse_search.matches.len(), 3);
            capture(&mut h, &tabs, "reverse-filtered", full, &dir);
            key(&mut h, &mut tabs, Key::Ctrl('r'));
            assert_eq!(h.state.reverse_search.selected_match, Some(1));
            capture(&mut h, &tabs, "reverse-cycled", full, &dir);
            key(&mut h, &mut tabs, Key::Enter);
            assert_eq!(h.state.input_buffer, "/a1-second-invalid ");
            assert!(!h.state.reverse_search.is_open());
            capture(&mut h, &tabs, "reverse-accepted", full, &dir);
            h.state.input_buffer = draft.into();
            h.state.cursor_position = draft.chars().count();
            key(&mut h, &mut tabs, Key::Ctrl('r'));
            text(&mut h, &mut tabs, "a1-");
            key(&mut h, &mut tabs, Key::Esc);
            assert!(!h.state.reverse_search.is_open());
            assert_eq!(h.state.input_buffer, draft);
            assert!(
                h.state.modal_stack.is_empty(),
                "ReverseSearch is never a frame"
            );
            capture(&mut h, &tabs, "reverse-restored", full, &dir);
        }
    }
}
