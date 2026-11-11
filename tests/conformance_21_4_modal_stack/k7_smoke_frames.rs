//! Bb1a smoke predicate controls: complete production frames, never native captures.
use super::keymap_support::press;
use super::stage_support::paste;
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
    selected_output_dir(std::env::var_os("RUSTAIN_STORY_21_4_FRAME_DIR").map(PathBuf::from))
}

fn selected_output_dir(path: Option<PathBuf>) -> Option<PathBuf> {
    let path = path?;
    assert!(path.is_absolute());
    assert!(
        !path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    );
    std::fs::create_dir_all(&path).unwrap();
    Some(std::fs::canonicalize(path).unwrap())
}

#[test]
fn frame_output_dir_is_portable_and_opt_in() {
    // Keep this external to a launcher's session-specific TMPDIR.
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let path = root.path().join("external-evidence/frames");
    assert_eq!(selected_output_dir(None), None);
    assert!(
        !path.exists(),
        "no opt-in must not create an export directory"
    );
    let selected = selected_output_dir(Some(path.clone())).unwrap();
    assert!(selected.is_dir());
    assert_eq!(selected, std::fs::canonicalize(&path).unwrap());
    assert_eq!(selected_output_dir(Some(path)), Some(selected));
}

#[test]
fn frame_output_dir_rejects_relative_paths_and_parent_traversal() {
    let root = tempfile::tempdir().unwrap();
    for path in [
        PathBuf::from("relative-frame-export"),
        root.path().join("frames/../escaped"),
    ] {
        assert!(std::panic::catch_unwind(|| selected_output_dir(Some(path))).is_err());
    }
    assert!(!root.path().join("frames").exists());
    assert!(!root.path().join("escaped").exists());
}

fn capture(
    h: &mut TestHarness,
    tabs: &TabManager,
    family: &str,
    stage: &str,
    full: bool,
    dir: &Option<PathBuf>,
) -> String {
    h.render_active_tab(tabs);
    export_frame(h, family, stage, full, dir)
}

fn export_frame(
    h: &mut TestHarness,
    family: &str,
    stage: &str,
    full: bool,
    dir: &Option<PathBuf>,
) -> String {
    let buffer = h.terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    let screen = rows.join("\n");
    let reversed_cells: Vec<_> = buffer
        .content
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.modifier.contains(ratatui::style::Modifier::REVERSED))
        .map(|(index, _)| {
            (
                index % usize::from(buffer.area.width),
                index / usize::from(buffer.area.width),
            )
        })
        .collect();
    assert!(
        rows.iter().any(|row| row.contains("Ready")),
        "complete status: {screen}"
    );
    assert!(rows.last().unwrap().starts_with('└'));
    assert_eq!(
        h.terminal.backend().cursor_visible(),
        h.state.modal_stack.is_empty()
            && h.state.focus == rustain::domain::models::FocusState::Input
            && h.state.pending_leader.is_none()
            && !h.state.reverse_search.is_open()
    );
    if let Some(dir) = dir {
        let cursor = h.terminal.get_cursor_position().unwrap();
        let mut record = serde_json::json!({
            "width": h.state.terminal_width, "height": h.state.terminal_height,
            "stage": stage, "full": full, "screen": screen,
            "draft": h.state.input_buffer, "cursor": [cursor.x, cursor.y],
            "cursor_hidden": !h.terminal.backend().cursor_visible(),
            "reversed_cells": reversed_cells,
        });
        if let Some(host) = family.strip_prefix("c-") {
            record["host"] = host.into();
        }
        let name = format!(
            "{family}-{}x{}-{}-{stage}.json",
            h.state.terminal_width,
            h.state.terminal_height,
            if full { "full" } else { "normal" }
        );
        std::fs::write(dir.join(name), serde_json::to_vec(&record).unwrap()).unwrap();
    }
    screen
}

#[tokio::test]
async fn bb1a_real_frames_for_smoke_predicates() {
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
            let before = capture(&mut h, &tabs, "bb1a", "before", full, &dir);
            let view = format!("{:?}", tabs.active_tab().view_state);
            open(&mut h, &mut tabs, full);
            assert!(presentation::is_open(&h.state, Surface::Search));
            capture(&mut h, &tabs, "bb1a", "search-unfiltered", full, &dir);
            capture(&mut h, &tabs, "bb2a", "search-unfiltered", full, &dir);
            text(&mut h, &mut tabs, "zzzz");
            assert!(h.state.search_state.matches.is_empty());
            capture(&mut h, &tabs, "bb1a", "search-empty", full, &dir);
            capture(&mut h, &tabs, "bb2a", "search-empty", full, &dir);
            key(&mut h, &mut tabs, Key::Esc);
            for keep in [false, true] {
                open(&mut h, &mut tabs, full);
                text(&mut h, &mut tabs, "A2b");
                assert_eq!(h.state.search_state.matches.len(), 2);
                capture(&mut h, &tabs, "bb1a", "search-filtered", full, &dir);
                if !keep {
                    capture(&mut h, &tabs, "bb2a", "search-filtered", full, &dir);
                    key(&mut h, &mut tabs, Key::Home);
                    key(&mut h, &mut tabs, Key::Right);
                    capture(&mut h, &tabs, "bb2a", "search-caret", full, &dir);
                    paste(&mut h, &mut tabs, "x");
                    capture(&mut h, &tabs, "bb2a", "search-pasted", full, &dir);
                    key(&mut h, &mut tabs, Key::Ctrl('h'));
                    capture(&mut h, &tabs, "bb2a", "search-refiltered", full, &dir);
                    key(&mut h, &mut tabs, Key::End);
                }
                key(&mut h, &mut tabs, Key::Enter);
                capture(&mut h, &tabs, "bb1a", "search-navigating-1", full, &dir);
                key(&mut h, &mut tabs, Key::Char('n'));
                assert_eq!(h.state.search_state.focused_match_index, 1);
                let matched_view = format!("{:?}", tabs.active_tab().view_state);
                assert_ne!(matched_view, view, "long assistant jump must leave bottom");
                let matched = capture(&mut h, &tabs, "bb1a", "search-navigating-2", full, &dir);
                assert!(matched.contains("Bb1a-row-01") && !matched.contains(REPLY));
                key(&mut h, &mut tabs, if keep { Key::Enter } else { Key::Esc });
                assert!(!presentation::is_open(&h.state, Surface::Search));
                let after = capture(
                    &mut h,
                    &tabs,
                    "bb1a",
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
            capture(&mut h, &tabs, "bb1a", "reverse-open", full, &dir);
            capture(&mut h, &tabs, "bb2a", "reverse-unfiltered", full, &dir);
            text(&mut h, &mut tabs, "a1-");
            assert_eq!(h.state.reverse_search.matches.len(), 3);
            capture(&mut h, &tabs, "bb1a", "reverse-filtered", full, &dir);
            capture(&mut h, &tabs, "bb2a", "reverse-filtered", full, &dir);
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            capture(&mut h, &tabs, "bb2a", "reverse-caret", full, &dir);
            paste(&mut h, &mut tabs, "x");
            capture(&mut h, &tabs, "bb2a", "reverse-pasted", full, &dir);
            key(&mut h, &mut tabs, Key::Ctrl('h'));
            capture(&mut h, &tabs, "bb2a", "reverse-refiltered", full, &dir);
            key(&mut h, &mut tabs, Key::End);
            key(&mut h, &mut tabs, Key::Ctrl('r'));
            assert_eq!(h.state.reverse_search.selected_match, Some(1));
            capture(&mut h, &tabs, "bb1a", "reverse-cycled", full, &dir);
            key(&mut h, &mut tabs, Key::Enter);
            assert_eq!(h.state.input_buffer, "/a1-second-invalid ");
            assert!(!h.state.reverse_search.is_open());
            capture(&mut h, &tabs, "bb1a", "reverse-accepted", full, &dir);
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
            capture(&mut h, &tabs, "bb1a", "reverse-restored", full, &dir);
            key(&mut h, &mut tabs, Key::Ctrl('p'));
            let mut commands = rustain::adapters::command_registry::CommandRegistry::new();
            let mut palette = rustain::adapters::palette_registry::PaletteRegistry::new();
            let mut gate = rustain::adapters::tui::palette_gate::PaletteGate::new();
            let workspace = tempfile::tempdir().unwrap();
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            let unfiltered_count = h.state.command_palette.filtered_entries.len();
            capture(&mut h, &tabs, "bb2a", "palette-unfiltered", full, &dir);
            paste(&mut h, &mut tabs, "/ml");
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            assert!(
                h.state
                    .command_palette
                    .filtered_entries
                    .iter()
                    .any(|entry| entry.name == "/ml")
            );
            assert!(h.state.command_palette.filtered_entries.len() < unfiltered_count);
            capture(&mut h, &tabs, "bb2a", "palette-filtered", full, &dir);
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            capture(&mut h, &tabs, "bb2a", "palette-caret", full, &dir);
            paste(&mut h, &mut tabs, "~");
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            assert!(h.state.command_palette.filtered_entries.is_empty());
            capture(&mut h, &tabs, "bb2a", "palette-pasted", full, &dir);
            key(&mut h, &mut tabs, Key::Ctrl('k'));
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            capture(&mut h, &tabs, "bb2a", "palette-killed", full, &dir);
            key(&mut h, &mut tabs, Key::Ctrl('u'));
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            capture(&mut h, &tabs, "bb2a", "palette-cleared", full, &dir);
            key(&mut h, &mut tabs, Key::Esc);
            assert_eq!(h.state.input_buffer, draft);
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
    let screen = capture(h, tabs, "bb1a", stage, full, &None);
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

fn bb1b_ii_write(
    h: &mut TestHarness,
    stage: &str,
    full: bool,
    screen: &str,
    history_title: Option<&str>,
    dir: &Option<PathBuf>,
) {
    if let Some(dir) = dir {
        use std::io::Write as _;
        let cursor = h.terminal.get_cursor_position().unwrap();
        let record = serde_json::json!({
            "width": h.state.terminal_width, "height": h.state.terminal_height,
            "stage": stage, "full": full, "screen": screen,
            "draft": h.state.input_buffer, "cursor": [cursor.x, cursor.y],
            "cursor_hidden": !h.terminal.backend().cursor_visible(),
            "conversation_title": h.conversation.title,
            "history_title": history_title,
        });
        let name = format!(
            "bb1b-ii-{}x{}-{}-{stage}.json",
            h.state.terminal_width,
            h.state.terminal_height,
            if full { "full" } else { "normal" }
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .expect("CrossSearch export must not overwrite a frame");
        file.write_all(&serde_json::to_vec(&record).unwrap())
            .unwrap();
    }
}

fn bb1b_ii_capture(
    h: &mut TestHarness,
    tabs: &mut TabManager,
    stage: &str,
    full: bool,
    dir: &Option<PathBuf>,
) {
    crate::conformance_tui_orchestration::render_runtime(h, tabs);
    let buffer = h.terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    let screen = rows.join("\n");
    let draft = if full {
        "1\n2\n3\n4\n5\n6\n7\n8"
    } else {
        DRAFT
    };
    assert_eq!(h.state.input_buffer, draft);
    assert_eq!(h.state.cursor_position, draft.chars().count());
    assert_eq!(h.state.active_tab_id, tabs.active_tab_id());
    assert_eq!(h.conversation.id, tabs.active_tab().conversation.id);
    assert_eq!(
        h.conversation.messages.len(),
        tabs.active_tab().conversation.messages.len()
    );
    for (live, owned) in h
        .conversation
        .messages
        .iter()
        .zip(&tabs.active_tab().conversation.messages)
    {
        assert_eq!((&live.id, &live.content), (&owned.id, &owned.content));
    }
    assert!(!h.terminal.backend().cursor_visible(), "{stage}: {screen}");
    assert_eq!(buffer.area.width, h.state.terminal_width);
    assert_eq!(buffer.area.height, h.state.terminal_height);
    let layout = rustain::adapters::tui::layout::compute_layout(
        buffer.area,
        &h.state.theme,
        draft,
        tabs.tab_count(),
        h.state.monitor,
    )
    .unwrap();
    assert!(
        rows[usize::from(layout.status_bar.y)].contains("Ready"),
        "{screen}"
    );
    assert!(rows.last().unwrap().starts_with('└'), "{screen}");
    if stage == "history-return" {
        assert_eq!(buffer.area.width, 130);
        assert_eq!(h.state.modal_stack.len(), 1);
        assert_eq!(
            h.state.modal_stack.top().unwrap().surface,
            Surface::History,
            "closing the search returns to the History frame it was opened from"
        );
        assert!(screen.contains("History"), "{screen}");
        assert!(h.state.cross_search.query.is_empty() && !h.state.cross_search.running);
    } else {
        let owner = h.state.modal_stack.top().unwrap();
        assert_eq!(
            h.state.modal_stack.len(),
            2,
            "CrossSearch sits above the History frame"
        );
        assert_eq!(owner.surface, Surface::CrossSearch);
        assert_eq!(owner.title(), "Cross-Search");
        assert!(owner.conversation_id.is_none(), "CrossSearch is global");
        let rect = presentation::geometry::frame_rect(presentation::geometry::modal_region(
            &layout,
            buffer.area,
        ));
        assert!(rect.bottom() <= layout.status_bar.y && rect.bottom() <= layout.input_area.y);
        if full && buffer.area.width == 60 {
            assert_eq!(rect.height, 5, "exercise the minimum fitted body");
        }
        assert!(
            rows[usize::from(rect.y)].contains("Cross-Search"),
            "{screen}"
        );
        assert!(
            rows[usize::from(rect.y + 1)]
                .contains(&format!("/ {}", h.state.cross_search.query.as_str())),
            "{screen}"
        );
        let primary = match stage {
            "cross-unfiltered" => "Type at least 2 characters",
            "cross-filtered" => "1/1 matches",
            "cross-empty" => "No matches found",
            "cross-counted" => "20/20 matches",
            _ => panic!("unknown CrossSearch export stage: {stage}"),
        };
        assert!(rows[usize::from(rect.y + 2)].contains(primary), "{screen}");
        assert!(
            rows[usize::from(rect.bottom() - 1)].contains("[Enter] open  [Esc] close"),
            "{screen}"
        );
        assert!(!h.state.cross_search.running && !h.state.cross_search.load_failed);
        if matches!(stage, "cross-filtered" | "cross-counted") {
            assert!(
                rows[usize::from(rect.y + 3)..usize::from(rect.bottom() - 1)]
                    .iter()
                    .any(|row| row.contains(&format!("▸ {TURN}"))),
                "{screen}"
            );
        }
        if stage == "cross-counted" {
            assert_eq!(h.state.cross_search.selected, 19);
            assert!(h.state.cross_search.truncated_by_count);
            assert!(
                !screen.contains("Earlier result 00"),
                "selection must be windowed: {screen}"
            );
            assert!(
                !screen.contains(&h.state.cross_search.results[19].title),
                "the long selected title must actually truncate: {screen}"
            );
        }
    }
    bb1b_ii_write(h, stage, full, &screen, None, dir);
}

fn bb1b_ii_results(h: &mut TestHarness, query: &str, count: usize) {
    use rustain::adapters::tui::{
        app::InputAction,
        handlers::export::{self, CrossSearchResultsOutcome, CrossSearchScanAction},
    };
    use rustain::domain::services::cross_search::CrossSearchResult;
    press(&mut h.state, Key::Ctrl('e'));
    let changed = !h.state.cross_search.query.is_empty();
    assert_eq!(
        press(&mut h.state, Key::Ctrl('u')),
        if changed {
            InputAction::CrossSearchQueryChanged
        } else {
            InputAction::Consumed
        }
    );
    if changed {
        assert_eq!(
            export::apply_cross_search_query_change(&mut h.state),
            CrossSearchScanAction::Cleared
        );
    }
    let mut pending = CrossSearchScanAction::Cleared;
    for ch in query.chars() {
        assert_eq!(
            press(&mut h.state, Key::Char(ch)),
            InputAction::CrossSearchQueryChanged
        );
        pending = export::apply_cross_search_query_change(&mut h.state);
    }
    let CrossSearchScanAction::Spawn {
        query: claimed,
        frame_id,
        revision,
    } = pending
    else {
        panic!("the real query guard must claim the completion");
    };
    assert_eq!(claimed, query);
    assert_eq!(frame_id, h.state.modal_stack.top().unwrap().id);
    assert_eq!(revision, h.state.cross_search.request_revision);
    let results = (0..count)
        .map(|index| CrossSearchResult {
            conversation_id: h.conversation.id.clone(),
            title: if count == 1 {
                TURN.into()
            } else if index == 19 {
                format!("{TURN}-last-selected {}", "long trailing title ".repeat(16))
            } else {
                format!("Earlier result {index:02}")
            },
            excerpt: "Bb1a-row-00".into(),
            timestamp: h.conversation.updated_at,
            first_match_message_index: 1,
        })
        .collect();
    assert_eq!(
        export::apply_cross_search_results(
            &mut h.state,
            frame_id,
            revision,
            claimed,
            results,
            count == 20,
            false,
        ),
        CrossSearchResultsOutcome::Applied
    );
    assert_eq!(h.state.cross_search.query.as_str(), query);
    assert_eq!(h.state.cross_search.results.len(), count);
}

fn bb1b_ii_open_fixture(width: u16, height: u16, draft: &str) -> (TestHarness, TabManager, u64) {
    let mut h = TestHarness::with_size(130, 30);
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
    tabs.active_tab_mut().conversation = h.conversation.clone();
    h.state = super::k7_cross_search::history();
    h.state.active_tab_id = tabs.active_tab_id();
    h.state.input_buffer = draft.into();
    h.state.cursor_position = draft.chars().count();
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs);
    assert!(
        h.screen_text().contains("History"),
        "paint supported History before slash"
    );
    super::k7_cross_search::open(&mut h.state);
    let owner = h.state.modal_stack.top().unwrap().id;
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs);
    assert!(h.screen_text().contains("Cross-Search"));
    h.terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    rustain::adapters::tui::app::handle_input(
        &mut h.state,
        &rustain::domain::events::DomainInputEvent::Resize(width, height),
    );
    (h, tabs, owner)
}

#[test]
fn bb1b_ii_real_frames_for_smoke_predicates() {
    use rustain::adapters::tui::{app::InputAction, handlers::export};
    let dir = output_dir();
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let draft = if full {
                "1\n2\n3\n4\n5\n6\n7\n8"
            } else {
                DRAFT
            };
            let (mut h, mut tabs, owner) = bb1b_ii_open_fixture(width, height, draft);
            bb1b_ii_capture(&mut h, &mut tabs, "cross-unfiltered", full, &dir);
            for (query, count, stage) in [
                ("Bb1a", 1, "cross-filtered"),
                ("zzzz", 0, "cross-empty"),
                ("Bb1a", 20, "cross-counted"),
            ] {
                bb1b_ii_results(&mut h, query, count);
                if count == 20 {
                    for _ in 0..19 {
                        assert_eq!(press(&mut h.state, Key::Char('j')), InputAction::Consumed);
                    }
                }
                assert_eq!(h.state.modal_stack.top().unwrap().id, owner);
                bb1b_ii_capture(&mut h, &mut tabs, stage, full, &dir);
            }
            assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
            export::close_cross_search(&mut h.state);
            assert_eq!(h.state.modal_stack.len(), 1);
            assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::History);
            assert_eq!(h.state.input_buffer, draft);
            assert_eq!(h.state.cursor_position, draft.chars().count());
            if width == 130 {
                bb1b_ii_capture(&mut h, &mut tabs, "history-return", full, &dir);
            }
        }
    }
}

fn bb1b_ii_aux_capture(
    h: &mut TestHarness,
    tabs: &mut TabManager,
    index: &rustain::domain::services::session_index::SessionIndex,
    stage: &str,
    full: bool,
    dir: &Option<PathBuf>,
) {
    rustain::infrastructure::runtime::event_loop::render(
        &mut h.terminal,
        &mut h.state,
        &h.conversation,
        &h.streaming,
        "fixture-model",
        "fixture-provider",
        &rustain::adapters::noop::NoOpSecurity,
        1,
        0,
        Some(&mut *tabs),
        index,
        128_000,
    )
    .unwrap();
    let buffer = h.terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    let screen = rows.join("\n");
    let draft = if full {
        "1\n2\n3\n4\n5\n6\n7\n8"
    } else {
        DRAFT
    };
    assert_eq!(h.state.input_buffer, draft);
    assert_eq!(h.state.cursor_position, draft.chars().count());
    assert_eq!(h.state.active_tab_id, tabs.active_tab_id());
    assert!(!h.terminal.backend().cursor_visible());
    let layout = rustain::adapters::tui::layout::compute_layout(
        buffer.area,
        &h.state.theme,
        draft,
        tabs.tab_count(),
        h.state.monitor,
    )
    .unwrap();
    assert!(
        rows[layout.status_bar.y as usize].contains("Ready"),
        "{screen}"
    );
    match stage {
        "cross-quit" | "base-quit" => {
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::QuitConfirm
            );
            assert_eq!(
                presentation::is_open(&h.state, Surface::CrossSearch),
                stage == "cross-quit"
            );
            for primary in [
                "Quit rustain?",
                "Your unsent draft will be lost.",
                "[y] Quit",
                "[n/Esc/Enter] Stay",
            ] {
                assert!(screen.contains(primary), "{stage}: {screen}");
            }
        }
        "cross-untitled" => {
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::CrossSearch
            );
            assert_eq!(h.state.cross_search.results.len(), 1);
            assert_eq!(h.state.cross_search.results[0].title, "(untitled)");
            assert!(
                screen.contains("(untitled)") && screen.contains("1/1 matches"),
                "{screen}"
            );
        }
        "accepted-base" => {
            assert!(h.state.modal_stack.is_empty());
            assert_eq!(h.state.focus, rustain::domain::models::FocusState::Chat);
            assert!(
                screen.contains("Bb1a-row-0") && !screen.contains(REPLY),
                "{screen}"
            );
            assert!(h.state.search_state.peek_highlight.is_some());
        }
        "history-populated" => {
            assert_eq!(index.entries().len(), 1);
            assert_eq!(index.entries()[0].message_count, 2);
            let title = index.entries()[0].title.trim();
            let shown = if title.is_empty() {
                "(Untitled)"
            } else {
                title
            };
            assert!(
                screen.contains(shown) && screen.contains("2 msgs"),
                "{screen}"
            );
            assert_eq!(h.state.modal_stack.len(), 1);
            assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::History);
        }
        _ => panic!("unknown auxiliary frame {stage}"),
    }
    bb1b_ii_write(
        h,
        stage,
        full,
        &screen,
        index.entries().first().map(|entry| entry.title.as_str()),
        dir,
    );
}

fn bb1b_ii_request_quit(state: &mut rustain::adapters::tui::state::TuiState) {
    use rustain::adapters::tui::{
        app::InputAction,
        quit_confirm::{self, QuitFacts, QuitHost, QuitRequest},
    };
    assert_eq!(press(state, Key::Ctrl('q')), InputAction::Quit);
    let facts = QuitFacts {
        composer_draft: !state.input_buffer.is_empty(),
        ..QuitFacts::idle(QuitHost::Local)
    };
    assert_eq!(quit_confirm::request(state, facts), QuitRequest::Confirming);
}

#[tokio::test]
async fn bb1b_ii_native_predicates_have_real_runtime_positive_controls() {
    use rustain::adapters::tui::{app::InputAction, handlers::export};
    use rustain::domain::{
        models::session::{SessionManager, SessionState},
        ports::StoragePort,
        services::session_index::SessionIndex,
    };
    use rustain::infrastructure::runtime::event_loop;
    let dir = output_dir();
    let tmp = tempfile::tempdir().unwrap();
    let storage =
        rustain::adapters::filesystem::FileSystemStorage::new(tmp.path().join("sessions"));
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let draft = if full {
                "1\n2\n3\n4\n5\n6\n7\n8"
            } else {
                DRAFT
            };
            let (mut h, mut tabs, owner) = bb1b_ii_open_fixture(width, height, draft);
            bb1b_ii_results(&mut h, "Bb1a", 1);
            let mut index = SessionIndex::new();
            assert!(
                h.conversation.title.is_empty(),
                "exercise the actual untitled model"
            );
            index.touch(
                &h.conversation.id,
                None,
                Some(h.conversation.messages.len()),
            );
            index.set_open(&h.conversation.id, true);
            index.set_active(Some(&h.conversation.id));
            storage.save_conversation(&h.conversation).await.unwrap();
            let export::CrossSearchScanAction::Spawn {
                query,
                frame_id,
                revision,
            } = export::apply_cross_search_query_change(&mut h.state)
            else {
                panic!("the actual query must claim a scan");
            };
            let outcome = rustain::domain::services::cross_search::run_cross_search(
                &storage,
                &index,
                &query,
                Default::default(),
            )
            .await;
            assert_eq!(
                (outcome.scanned, outcome.total, outcome.results.len()),
                (1, 1, 1)
            );
            assert_eq!(
                export::apply_cross_search_results(
                    &mut h.state,
                    frame_id,
                    revision,
                    query,
                    outcome.results,
                    outcome.truncated_by_count,
                    outcome.truncated_by_time,
                ),
                export::CrossSearchResultsOutcome::Applied
            );
            bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "cross-untitled", full, &dir);
            bb1b_ii_request_quit(&mut h.state);
            bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "cross-quit", full, &dir);
            press(&mut h.state, Key::Char('n'));
            assert_eq!(h.state.modal_stack.top().unwrap().id, owner);
            assert_eq!(h.state.cross_search.query.as_str(), "Bb1a");
            if width < 100 {
                assert_eq!(
                    press(&mut h.state, Key::Enter),
                    InputAction::OpenCrossSearchResult
                );
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                event_loop::apply_open_cross_search_result(
                    &mut tabs,
                    &mut h.conversation,
                    &mut h.streaming,
                    &mut SessionManager::new(SessionState::Empty),
                    &mut h.state,
                    &mut h.turn_queue,
                    &storage,
                    &storage,
                    &mut index,
                    &tx,
                )
                .await;
                bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "accepted-base", full, &dir);
            } else {
                assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
                export::close_cross_search(&mut h.state);
                bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "history-populated", full, &dir);
                if full && let Some(dir) = &dir {
                    let persisted = &h.conversation;
                    assert_eq!(persisted.messages.len(), 2);
                    assert_eq!(persisted.messages[0].content, TURN);
                    let store = rustain::adapters::filesystem::FileSystemStorage::new(
                        dir.join("persisted-control/.claude/sessions"),
                    );
                    store.save_conversation(persisted).await.unwrap();
                }
            }
            bb1b_ii_request_quit(&mut h.state);
            bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "base-quit", full, &dir);
            press(&mut h.state, Key::Char('n'));
            if width < 100 {
                // A successful open closed the History frame too.
                assert!(h.state.modal_stack.is_empty());
            } else {
                assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::History);
            }
        }
    }
}

fn bb2b_publish_cross(h: &mut TestHarness, count: usize) {
    use rustain::adapters::tui::handlers::export::{
        self, CrossSearchResultsOutcome, CrossSearchScanAction,
    };
    let CrossSearchScanAction::Spawn {
        query,
        frame_id,
        revision,
    } = export::apply_cross_search_query_change(&mut h.state)
    else {
        panic!("a settled multi-character query must claim the actual completion");
    };
    let results = (0..count)
        .map(
            |n| rustain::domain::services::cross_search::CrossSearchResult {
                conversation_id: h.conversation.id.clone(),
                title: if count == 1 {
                    TURN.into()
                } else {
                    format!(
                        "{TURN}-last-selected-{n} {}",
                        "long trailing title ".repeat(16)
                    )
                },
                excerpt: "Bb1a-row-00".into(),
                timestamp: h.conversation.updated_at,
                first_match_message_index: 1,
            },
        )
        .collect();
    assert_eq!(
        export::apply_cross_search_results(
            &mut h.state,
            frame_id,
            revision,
            query,
            results,
            count == 20,
            false,
        ),
        CrossSearchResultsOutcome::Applied
    );
    assert!(
        !h.state.cross_search.running,
        "save only a complete settled frame"
    );
}

#[tokio::test]
async fn bb2b_real_frames_for_smoke_predicates() {
    use rustain::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
    let dir = output_dir();
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let draft = if full {
                "1\n2\n3\n4\n5\n6\n7\n8"
            } else {
                DRAFT
            };
            for (kind, needle) in [("log", "aone"), ("help", "notification")] {
                let mut h = TestHarness::with_size(width, height);
                h.complete_turn(
                    TURN,
                    vec![
                        StreamChunk::Text {
                            content: REPLY.into(),
                            parent_tool_use_id: None,
                        },
                        StreamChunk::TurnComplete {
                            stop_reason: StopReason::EndTurn,
                        },
                    ],
                );
                let mut tabs = TabManager::new(CancellationToken::new());
                tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
                h.state.active_tab_id = tabs.active_tab_id();
                h.state.focus = rustain::domain::models::FocusState::Input;
                h.state.input_buffer = draft.into();
                h.state.cursor_position = draft.chars().count();
                notifications::notify(
                    &mut h.state,
                    NoticeSource::System,
                    NoticeSeverity::Error,
                    "Unknown command: /aone",
                );
                let retained = h.state.notifications.records().next_back().unwrap().id;
                notifications::notify(
                    &mut h.state,
                    NoticeSource::System,
                    NoticeSeverity::Info,
                    "Unknown command: /btwo",
                );
                key(&mut h, &mut tabs, Key::Ctrl('x'));
                key(
                    &mut h,
                    &mut tabs,
                    Key::Char(if kind == "log" { 'n' } else { '?' }),
                );
                if kind == "log" {
                    key(&mut h, &mut tabs, Key::Char('j'));
                    assert_eq!(
                        h.state.notification_log.as_ref().unwrap().selected_id,
                        Some(retained)
                    );
                } else {
                    for _ in 0..3 {
                        key(&mut h, &mut tabs, Key::Char('j'));
                    }
                    assert_eq!(h.state.help_overlay.scroll_offset, 3);
                }
                let owner = h.state.modal_stack.top().unwrap().clone();
                key(&mut h, &mut tabs, Key::Char('/'));
                capture(
                    &mut h,
                    &tabs,
                    "bb2b",
                    &format!("{kind}-unfiltered"),
                    full,
                    &dir,
                );
                paste(&mut h, &mut tabs, needle);
                capture(
                    &mut h,
                    &tabs,
                    "bb2b",
                    &format!("{kind}-filtered"),
                    full,
                    &dir,
                );
                key(&mut h, &mut tabs, Key::Home);
                key(&mut h, &mut tabs, Key::Right);
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-caret"), full, &dir);
                paste(&mut h, &mut tabs, "x");
                let empty = capture(&mut h, &tabs, "bb2b", &format!("{kind}-empty"), full, &dir);
                assert!(empty.contains(if kind == "log" {
                    "No matching notifications."
                } else {
                    "No matching keybindings."
                }));
                key(&mut h, &mut tabs, Key::Ctrl('h'));
                capture(
                    &mut h,
                    &tabs,
                    "bb2b",
                    &format!("{kind}-restored"),
                    full,
                    &dir,
                );
                key(&mut h, &mut tabs, Key::Enter);
                capture(
                    &mut h,
                    &tabs,
                    "bb2b",
                    &format!("{kind}-accepted"),
                    full,
                    &dir,
                );
                key(&mut h, &mut tabs, Key::Char('/'));
                key(&mut h, &mut tabs, Key::Esc);
                capture(
                    &mut h,
                    &tabs,
                    "bb2b",
                    &format!("{kind}-cleared"),
                    full,
                    &dir,
                );
                assert_eq!(
                    h.state.modal_stack.top(),
                    Some(&owner),
                    "sub-context back keeps its exact owner"
                );
                if kind == "log" {
                    assert_eq!(
                        h.state.notification_log.as_ref().unwrap().selected_id,
                        Some(retained)
                    );
                } else {
                    assert_eq!(
                        h.state.help_overlay.scroll_offset, 3,
                        "filter never overwrites original scroll"
                    );
                }
                assert_eq!(h.state.input_buffer, draft);
            }
            let (mut h, mut tabs, owner) = bb1b_ii_open_fixture(width, height, draft);
            capture(&mut h, &tabs, "bb2b", "cross-unfiltered", full, &dir);
            paste(&mut h, &mut tabs, "Bb1a");
            bb2b_publish_cross(&mut h, 1);
            capture(&mut h, &tabs, "bb2b", "cross-filtered", full, &dir);
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            capture(&mut h, &tabs, "bb2b", "cross-caret", full, &dir);
            paste(&mut h, &mut tabs, "x");
            bb2b_publish_cross(&mut h, 0);
            capture(&mut h, &tabs, "bb2b", "cross-empty", full, &dir);
            key(&mut h, &mut tabs, Key::Ctrl('h'));
            bb2b_publish_cross(&mut h, 1);
            capture(&mut h, &tabs, "bb2b", "cross-restored", full, &dir);
            key(&mut h, &mut tabs, Key::End);
            bb2b_publish_cross(&mut h, 20);
            for _ in 0..19 {
                key(&mut h, &mut tabs, Key::Down);
            }
            capture(&mut h, &tabs, "bb2b", "cross-counted", full, &dir);
            assert_eq!(
                h.state.cross_search.selected, 19,
                "selected-window control is reachable"
            );
            assert_eq!(h.state.modal_stack.top().unwrap().id, owner);
            assert_eq!(h.state.input_buffer, draft);
        }
    }
}

fn c_capture_local(
    h: &mut TestHarness,
    tabs: &mut TabManager,
    stage: &str,
    dir: &Option<PathBuf>,
) -> String {
    rustain::infrastructure::runtime::event_loop::render(
        &mut h.terminal,
        &mut h.state,
        &h.conversation,
        &h.streaming,
        "fixture-model",
        "fixture-provider",
        &rustain::adapters::noop::NoOpSecurity,
        1,
        0,
        Some(tabs),
        &rustain::domain::services::session_index::SessionIndex::new(),
        128_000,
    )
    .unwrap();
    export_frame(h, "c-local", stage, false, dir)
}

#[tokio::test]
async fn c_real_local_frames_for_smoke_predicates() {
    use rustain::adapters::tui::{
        app::{InputAction, handle_input},
        handlers::scroll,
        notifications::{self, NoticeSeverity, NoticeSource},
    };
    use rustain::domain::{
        events::DomainInputEvent,
        models::{FocusState, view_state::ScrollDelta},
    };
    let dir = output_dir();
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut h = TestHarness::with_size(width, height);
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
        // The harness retains the live conversation mirror used by the actual
        // scroll effect; the same manager owns the view and production paint.
        tabs.active_tab_mut().conversation = h.conversation.clone();
        h.state.active_tab_id = tabs.active_tab_id();
        c_capture_local(&mut h, &mut tabs, "initial", &dir);
        for c in "/cnotice ".chars() {
            press(&mut h.state, Key::Char(c));
        }
        assert_eq!(h.state.input_buffer, "/cnotice ");
        assert_eq!(h.state.cursor_position, 9);
        c_capture_local(&mut h, &mut tabs, "notice-command", &dir);
        assert!(matches!(
            press(&mut h.state, Key::Enter),
            InputAction::SubmitWithContext { command: Some(command), .. } if command == "cnotice"
        ));
        notifications::notify(
            &mut h.state,
            NoticeSource::System,
            NoticeSeverity::Warning,
            "Unknown command: /cnotice",
        );
        c_capture_local(&mut h, &mut tabs, "notice", &dir);
        for k in [Key::Ctrl('x'), Key::Char('n')] {
            press(&mut h.state, k);
        }
        c_capture_local(&mut h, &mut tabs, "notice-log", &dir);
        for k in [Key::Char('x'), Key::Esc] {
            press(&mut h.state, k);
        }
        c_capture_local(&mut h, &mut tabs, "notice-dismiss-return", &dir);
        for c in "C-kept draft".chars() {
            press(&mut h.state, Key::Char(c));
        }
        c_capture_local(&mut h, &mut tabs, "typed", &dir);
        press(&mut h.state, Key::Ctrl('a'));
        assert_eq!(h.state.cursor_position, 0);
        c_capture_local(&mut h, &mut tabs, "readline-home", &dir);
        press(&mut h.state, Key::Ctrl('e'));
        assert_eq!(h.state.cursor_position, 12);
        press(&mut h.state, Key::Left);
        assert_eq!(h.state.cursor_position, 11);
        c_capture_local(&mut h, &mut tabs, "left", &dir);
        press(&mut h.state, Key::End);
        press(&mut h.state, Key::Home);
        press(&mut h.state, Key::Right);
        assert_eq!(h.state.cursor_position, 1);
        c_capture_local(&mut h, &mut tabs, "caret", &dir);
        handle_input(&mut h.state, &DomainInputEvent::Paste("X".into()));
        assert_eq!(
            (&*h.state.input_buffer, h.state.cursor_position),
            ("CX-kept draft", 2)
        );
        c_capture_local(&mut h, &mut tabs, "paste", &dir);
        press(&mut h.state, Key::Ctrl('k'));
        assert_eq!((&*h.state.input_buffer, h.state.cursor_position), ("CX", 2));
        c_capture_local(&mut h, &mut tabs, "kill-tail", &dir);
        press(&mut h.state, Key::Ctrl('u'));
        assert!(h.state.input_buffer.is_empty());
        c_capture_local(&mut h, &mut tabs, "kill-prefix", &dir);
        for c in "C-kept draft".chars() {
            press(&mut h.state, Key::Char(c));
        }
        let bottom = c_capture_local(&mut h, &mut tabs, "bottom", &dir);
        assert!(bottom.contains(REPLY));
        // Local full-page up is Chat Ctrl+B; attached offers Input PgUp.
        press(&mut h.state, Key::Esc);
        assert_eq!(h.state.focus, FocusState::Chat);
        c_capture_local(&mut h, &mut tabs, "chat-before-scroll", &dir);
        assert_eq!(
            press(&mut h.state, Key::Ctrl('b')),
            InputAction::ScrollFullPageUp
        );
        scroll::handle_apply_scroll_intent(
            &mut tabs,
            &mut h.state,
            &h.conversation,
            ScrollDelta::FullPageUp,
        );
        c_capture_local(&mut h, &mut tabs, "chat-scrolled", &dir);
        press(&mut h.state, Key::Char('i'));
        assert_eq!(h.state.focus, FocusState::Input);
        let scrolled = c_capture_local(&mut h, &mut tabs, "scrolled", &dir);
        assert!(scrolled.contains("Bb1a-row-") && !scrolled.contains(REPLY));
        assert!(
            tabs.active_tab().view_state.scroll_offset > 0,
            "observable scroll control"
        );
        let view = tabs.active_tab().view_state.clone();
        for (kind, opener) in [("help", '?'), ("log", 'n')] {
            press(&mut h.state, Key::Ctrl('x'));
            press(&mut h.state, Key::Char(opener));
            assert_eq!(h.state.modal_stack.len(), 1, "cover reached");
            c_capture_local(&mut h, &mut tabs, kind, &dir);
            if kind == "help" {
                let before = h.state.help_overlay.scroll_offset;
                press(&mut h.state, Key::Char('j'));
                assert_eq!(h.state.help_overlay.scroll_offset, before + 1);
                c_capture_local(&mut h, &mut tabs, "help-scrolled", &dir);
            }
            press(&mut h.state, Key::Esc);
            assert!(h.state.modal_stack.is_empty(), "owner actually released");
            let restored = c_capture_local(&mut h, &mut tabs, &format!("{kind}-return"), &dir);
            assert_eq!(
                restored
                    .lines()
                    .take(usize::from(height - 4))
                    .collect::<Vec<_>>(),
                scrolled
                    .lines()
                    .take(usize::from(height - 4))
                    .collect::<Vec<_>>(),
                "same production Stage after cover/back",
            );
            let current = &tabs.active_tab().view_state;
            assert_eq!(current.mode, view.mode);
            assert_eq!(current.scroll_offset, view.scroll_offset);
            assert_eq!(current.focused_turn, view.focused_turn);
            assert_eq!(current.collapsed, view.collapsed);
            assert_eq!(current.summary_tier, view.summary_tier);
            assert_eq!(current.pending_append_lines, view.pending_append_lines);
            assert_eq!(h.state.input_buffer, "C-kept draft");
        }
    }
}
