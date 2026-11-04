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

// Render-only retained-data fixture. No TaskHandle, run, or drill capability is
// fabricated: the native leg below /fanout owns that proof. The sealed DrillBody
// cannot be constructed by integration tests; subview text is fixture DATA.
#[derive(Debug)]
struct Bb1bPayload(super::keymap_support::StubWave);

impl rustain::domain::ports::wave_handle::WaveHandle for Bb1bPayload {
    fn snapshot(&self) -> rustain::domain::ports::wave_handle::WaveSnapshot {
        use rustain::domain::models::{AgentId, SpokeResult};
        let mut snap = self.0.snapshot();
        snap.completed = snap.spoke_count;
        snap.outcome.spokes = (0..snap.spoke_count)
            .map(|slot| {
                (
                    AgentId::root(),
                    SpokeResult::Completed {
                        summary: ["a1-first-invalid", "a1-second-invalid"][slot % 2].into(),
                    },
                )
            })
            .collect();
        snap.slots = self.slots();
        snap.rerun_counts = vec![0; snap.spoke_count];
        snap
    }
    fn cancel(&self) {
        self.0.cancel();
    }
    fn drill(&self, slot: usize) -> Option<rustain::domain::ports::wave_handle::DrillBody> {
        self.0.drill(slot)
    }
    fn drill_id(&self, slot: usize) -> Option<rustain::domain::models::orchestration::DrillId> {
        self.0.drill_id(slot)
    }
    fn slots(&self) -> Vec<rustain::domain::models::AgentId> {
        vec![rustain::domain::models::AgentId::root(); self.0.0]
    }
    fn rerun_count_for_slot(&self, slot: usize) -> u8 {
        self.0.rerun_count_for_slot(slot)
    }
    fn resolve_count(&self) -> usize {
        self.0.resolve_count()
    }
}

fn bb1b_capture(
    h: &mut TestHarness,
    tabs: &TabManager,
    stage: &str,
    full: bool,
    dir: &Option<PathBuf>,
) {
    let screen = capture(h, tabs, stage, full, &None);
    if let Some(dir) = dir {
        let buffer = h.terminal.backend().buffer();
        let reversed: Vec<usize> = (0..buffer.area.height)
            .filter(|&y| {
                (0..buffer.area.width).any(|x| {
                    buffer[(x, y)]
                        .modifier
                        .contains(ratatui::style::Modifier::REVERSED)
                })
            })
            .map(usize::from)
            .collect();
        let record = serde_json::json!({
            "width": h.state.terminal_width, "height": h.state.terminal_height,
            "stage": stage, "full": full, "screen": screen,
            "draft": h.state.input_buffer, "cursor_hidden": !h.terminal.backend().cursor_visible(),
            "reversed": reversed,
        });
        std::fs::write(
            dir.join(format!(
                "bb1b-i-{}x{}-{}-{stage}.json",
                h.state.terminal_width,
                h.state.terminal_height,
                if full { "full" } else { "normal" }
            )),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn bb1b_i_real_frames_for_smoke_predicates() {
    use rustain::adapters::tui::{
        app::InputAction,
        handlers::{bookmark, wave_overlay},
    };
    use std::sync::Arc;
    let dir = output_dir();
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let (mut h, mut tabs) = super::stage_support::stage(width, height);
            h.finalize_turn();
            let draft = if full {
                "1\n2\n3\n4\n5\n6\n7\n8"
            } else {
                DRAFT
            };
            h.state.input_buffer = draft.into();
            h.state.cursor_position = draft.chars().count();
            bb1b_capture(&mut h, &tabs, "before", full, &dir);
            let before = super::stage_support::view(&tabs);
            let action = press(&mut h.state, Key::Char('\''));
            assert_eq!(action, InputAction::OpenBookmarkList);
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            bb1b_capture(&mut h, &tabs, "bookmark-open", full, &dir);
            press(&mut h.state, Key::Char('j'));
            assert_eq!(h.state.bookmark_list_selected, 1);
            bb1b_capture(&mut h, &tabs, "bookmark-selected", full, &dir);
            press(&mut h.state, Key::Esc);
            bookmark::close(&mut h.state);
            assert_eq!(super::stage_support::view(&tabs), before);
            bb1b_capture(&mut h, &tabs, "bookmark-back", full, &dir);
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            let conversation = tabs.active_tab().conversation.clone();
            bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &conversation);
            assert_ne!(super::stage_support::view(&tabs), before);
            bb1b_capture(&mut h, &tabs, "bookmark-jump", full, &dir);
            tabs.active_tab_mut().session_meta.bookmarks = vec![0, 1, 2];
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            press(&mut h.state, Key::Char('j'));
            press(&mut h.state, Key::Char('j'));
            bb1b_capture(&mut h, &tabs, "bookmark-counted", full, &dir);
            bookmark::close(&mut h.state);
            tabs.active_tab_mut().session_meta.bookmarks.clear();
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            assert!(!presentation::is_open(&h.state, Surface::BookmarkList));
            h.state.notifications.clear_toasts();
            bb1b_capture(&mut h, &tabs, "bookmark-empty", full, &dir);
            h.state.wave_run = Some(Arc::new(Bb1bPayload(super::keymap_support::StubWave(2))));
            let id = tabs.active_tab().conversation.id.clone();
            for (key, stage) in [
                (Key::Char('r'), "wave-open"),
                (Key::Char('j'), "wave-selected"),
            ] {
                let action = press(&mut h.state, key);
                wave_overlay::apply(&mut h.state, &id, &action);
                bb1b_capture(&mut h, &tabs, stage, full, &dir);
            }
            // Fixture payload, not a claimed successful lazy handle resolution.
            h.state.wave_drill_body = Some((1, "a1-second-invalid".into()));
            bb1b_capture(&mut h, &tabs, "wave-drill", full, &dir);
            for (key, stage) in [
                (Key::Char('d'), "wave-diverge"),
                (Key::Char('s'), "wave-drill-return"),
                (Key::Ctrl('c'), "wave-closed"),
                (Key::Char('d'), "wave-direct-diverge"),
                (Key::Esc, "wave-compact"),
                (Key::Enter, "wave-reopen"),
                (Key::Char('d'), "wave-list-diverge"),
                (Key::Esc, "wave-list-compact"),
            ] {
                let action = press(&mut h.state, key);
                wave_overlay::apply(&mut h.state, &id, &action);
                bb1b_capture(&mut h, &tabs, stage, full, &dir);
            }
            for (count, selected, stage) in [(0, 0, "wave-empty"), (24, 23, "wave-counted")] {
                h.state.wave_run = Some(Arc::new(Bb1bPayload(super::keymap_support::StubWave(
                    count,
                ))));
                let action = press(&mut h.state, Key::Char('r'));
                wave_overlay::apply(&mut h.state, &id, &action);
                h.state.wave_overlay_selected = selected;
                bb1b_capture(&mut h, &tabs, stage, full, &dir);
                wave_overlay::apply(&mut h.state, &id, &InputAction::CloseWaveOverlay);
            }
        }
    }
}
