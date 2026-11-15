//! Painted-frame controls for the modal-stack smoke predicates: Search, ReverseSearch, Palette,
//! Bookmark, Wave, CrossSearch, Help and Notification frames are driven through the production
//! decoder and handlers and painted by the production renderer at (60x16), (80x24) and (130x30),
//! with and without a full-height composer. Every frame must complete the shell (status row,
//! bottom border, cursor visibility), keep the draft and its composer row, and (at the
//! standard 80x24 shell) paint its owner's predicates.
use super::keymap_support::press;
use super::stage_support::paste;
use crate::e2e_harness::TestHarness;
use rustain::adapters::tui::{
    handlers::search,
    keymap::Key,
    presentation::{self, Surface},
};
use rustain::domain::models::{StopReason, StreamChunk, TabManager};
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

/// Paint the active tab through the production renderer and return the complete screen.
fn capture(
    h: &mut TestHarness,
    tabs: &TabManager,
    family: &str,
    stage: &str,
    full: bool,
) -> String {
    h.render_active_tab(tabs);
    check_frame(h, family, stage, full)
}

/// The shell invariants every painted frame satisfies; returns the rows joined by `\n`.
fn check_frame(h: &TestHarness, family: &str, stage: &str, full: bool) -> String {
    let buffer = h.terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    let screen = rows.join("\n");
    let label = format!("{family} {stage} full={full}: {screen}");
    assert!(
        rows.iter().any(|row| row.contains("Ready")),
        "complete status: {label}"
    );
    assert!(rows.last().unwrap().starts_with('└'), "{label}");
    assert_eq!(
        h.terminal.backend().cursor_visible(),
        h.state.modal_stack.is_empty()
            && h.state.focus == rustain::domain::models::FocusState::Input
            && h.state.pending_leader.is_none()
            && !h.state.reverse_search.is_open(),
        "{label}"
    );
    screen
}

/// The rows of a screen returned by `capture`.
fn screen_rows(screen: &str) -> Vec<&str> {
    screen.split('\n').collect()
}

/// The composer's last body row (absolute row `height - 2`) is `│` + the draft's last line
/// padded to the inner width + `│`.
fn assert_composer_last_row(h: &TestHarness, screen: &str, draft: &str, stage: &str) {
    let rows = screen_rows(screen);
    let last_line = draft.lines().last().unwrap_or("");
    let inner = usize::from(h.state.terminal_width) - 2;
    assert_eq!(
        rows[rows.len() - 2],
        format!("│{last_line:<inner$}│"),
        "{stage}: the composer paints the draft's last line: {screen}"
    );
}

#[tokio::test]
async fn bb1a_real_frames_for_smoke_predicates() {
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
            let before = capture(&mut h, &tabs, "bb1a", "before", full);
            let view = format!("{:?}", tabs.active_tab().view_state);
            open(&mut h, &mut tabs, full);
            assert!(presentation::is_open(&h.state, Surface::Search));
            capture(&mut h, &tabs, "bb1a", "search-unfiltered", full);
            capture(&mut h, &tabs, "bb2a", "search-unfiltered", full);
            text(&mut h, &mut tabs, "zzzz");
            assert!(h.state.search_state.matches.is_empty());
            capture(&mut h, &tabs, "bb1a", "search-empty", full);
            capture(&mut h, &tabs, "bb2a", "search-empty", full);
            key(&mut h, &mut tabs, Key::Esc);
            for keep in [false, true] {
                open(&mut h, &mut tabs, full);
                text(&mut h, &mut tabs, "A2b");
                assert_eq!(h.state.search_state.matches.len(), 2);
                capture(&mut h, &tabs, "bb1a", "search-filtered", full);
                if !keep {
                    capture(&mut h, &tabs, "bb2a", "search-filtered", full);
                    key(&mut h, &mut tabs, Key::Home);
                    key(&mut h, &mut tabs, Key::Right);
                    capture(&mut h, &tabs, "bb2a", "search-caret", full);
                    paste(&mut h, &mut tabs, "x");
                    capture(&mut h, &tabs, "bb2a", "search-pasted", full);
                    key(&mut h, &mut tabs, Key::Ctrl('h'));
                    capture(&mut h, &tabs, "bb2a", "search-refiltered", full);
                    key(&mut h, &mut tabs, Key::End);
                }
                key(&mut h, &mut tabs, Key::Enter);
                capture(&mut h, &tabs, "bb1a", "search-navigating-1", full);
                key(&mut h, &mut tabs, Key::Char('n'));
                assert_eq!(h.state.search_state.focused_match_index, 1);
                let matched_view = format!("{:?}", tabs.active_tab().view_state);
                assert_ne!(matched_view, view, "long assistant jump must leave bottom");
                let matched = capture(&mut h, &tabs, "bb1a", "search-navigating-2", full);
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
            capture(&mut h, &tabs, "bb1a", "reverse-open", full);
            capture(&mut h, &tabs, "bb2a", "reverse-unfiltered", full);
            text(&mut h, &mut tabs, "a1-");
            assert_eq!(h.state.reverse_search.matches.len(), 3);
            capture(&mut h, &tabs, "bb1a", "reverse-filtered", full);
            capture(&mut h, &tabs, "bb2a", "reverse-filtered", full);
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            capture(&mut h, &tabs, "bb2a", "reverse-caret", full);
            paste(&mut h, &mut tabs, "x");
            capture(&mut h, &tabs, "bb2a", "reverse-pasted", full);
            key(&mut h, &mut tabs, Key::Ctrl('h'));
            capture(&mut h, &tabs, "bb2a", "reverse-refiltered", full);
            key(&mut h, &mut tabs, Key::End);
            key(&mut h, &mut tabs, Key::Ctrl('r'));
            assert_eq!(h.state.reverse_search.selected_match, Some(1));
            capture(&mut h, &tabs, "bb1a", "reverse-cycled", full);
            key(&mut h, &mut tabs, Key::Enter);
            assert_eq!(h.state.input_buffer, "/a1-second-invalid ");
            assert!(!h.state.reverse_search.is_open());
            capture(&mut h, &tabs, "bb1a", "reverse-accepted", full);
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
            capture(&mut h, &tabs, "bb1a", "reverse-restored", full);
            key(&mut h, &mut tabs, Key::Ctrl('p'));
            let mut commands = rustain::adapters::command_registry::CommandRegistry::new();
            let mut palette = rustain::adapters::palette_registry::PaletteRegistry::new();
            let mut gate = rustain::adapters::tui::palette_gate::PaletteGate::new();
            let workspace = tempfile::tempdir().unwrap();
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            let unfiltered_count = h.state.command_palette.filtered_entries.len();
            capture(&mut h, &tabs, "bb2a", "palette-unfiltered", full);
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
            capture(&mut h, &tabs, "bb2a", "palette-filtered", full);
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            capture(&mut h, &tabs, "bb2a", "palette-caret", full);
            paste(&mut h, &mut tabs, "~");
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            assert!(h.state.command_palette.filtered_entries.is_empty());
            capture(&mut h, &tabs, "bb2a", "palette-pasted", full);
            key(&mut h, &mut tabs, Key::Ctrl('k'));
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            capture(&mut h, &tabs, "bb2a", "palette-killed", full);
            key(&mut h, &mut tabs, Key::Ctrl('u'));
            gate.sync(&mut h.state, &mut commands, &mut palette, workspace.path())
                .await;
            capture(&mut h, &tabs, "bb2a", "palette-cleared", full);
            key(&mut h, &mut tabs, Key::Esc);
            assert_eq!(h.state.input_buffer, draft);
        }
    }
}

// Render-only retained-data fixture. No TaskHandle, run, or drill capability is
// fabricated: the `/fanout` command arm is proven by
// `fanout_consults_gate_decision_before_launch` (tests/conformance_tui_orchestration.rs) and the
// real launch front door by the `spawn_wave_run` keystones. The sealed DrillBody cannot be
// constructed by integration tests; subview text is fixture DATA.
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

/// Paint the active tab and keep the draft: the state's buffer is the draft and the composer's
/// last body row paints its last line, at every viewport and for both composer sizes.
fn bb1b_capture(
    h: &mut TestHarness,
    tabs: &TabManager,
    stage: &str,
    full: bool,
    draft: &str,
) -> String {
    let screen = capture(h, tabs, "bb1b-i", stage, full);
    assert_eq!(h.state.input_buffer, draft, "{stage} full={full}: {screen}");
    assert_composer_last_row(h, &screen, draft, stage);
    screen
}

/// The absolute row of the status bar in the current layout.
fn status_row(h: &TestHarness, tabs: &TabManager) -> usize {
    let area = h.terminal.backend().buffer().area;
    let layout = rustain::adapters::tui::layout::compute_layout(
        area,
        &h.state.theme,
        &h.state.input_buffer,
        tabs.tab_count(),
        h.state.monitor,
    )
    .unwrap();
    usize::from(layout.status_bar.y)
}

/// The modal frame's rectangle in the current layout (the same geometry the Stage hands every
/// S-frame: `frame_rect(modal_region(..))`).
fn modal_box_rect(h: &TestHarness, tabs: &TabManager) -> ratatui::layout::Rect {
    let area = h.terminal.backend().buffer().area;
    let layout = rustain::adapters::tui::layout::compute_layout(
        area,
        &h.state.theme,
        &h.state.input_buffer,
        tabs.tab_count(),
        h.state.monitor,
    )
    .unwrap();
    presentation::geometry::frame_rect(presentation::geometry::modal_region(&layout, area))
}

/// The frame's cells (frame columns only), top border row to bottom border row.
fn modal_box_rows(h: &TestHarness, tabs: &TabManager) -> Vec<String> {
    let buffer = h.terminal.backend().buffer();
    let rect = modal_box_rect(h, tabs);
    (rect.y..rect.bottom())
        .map(|y| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

/// The complete frame rectangle at its expected place: titled top border, plain bottom border,
/// a `│` rail on both sides of every inner row (the deleted `_bb1b_frame`).
fn titled_box(h: &TestHarness, tabs: &TabManager, title: &str) -> Vec<String> {
    let rows = modal_box_rows(h, tabs);
    let width = rows.first().map_or(0, |row| row.chars().count());
    let prefix = format!("┌ {title} ");
    assert!(rows.len() >= 5, "frame too short: {rows:#?}");
    assert_eq!(
        rows[0],
        format!(
            "{prefix}{}┐",
            "─".repeat(width - prefix.chars().count() - 1)
        ),
        "top border: {rows:#?}"
    );
    assert_eq!(
        rows[rows.len() - 1],
        format!("└{}┘", "─".repeat(width - 2)),
        "bottom border: {rows:#?}"
    );
    for row in &rows[1..rows.len() - 1] {
        assert!(
            row.starts_with('│') && row.ends_with('│'),
            "side rail: {rows:#?}"
        );
    }
    rows
}

/// The inner text of a frame row, borders and padding removed.
fn inner(row: &str) -> &str {
    row.trim_matches('│').trim()
}

/// The last non-empty inner row of the frame: its action footer.
fn footer(rows: &[String]) -> &str {
    rows[1..rows.len() - 1]
        .iter()
        .rev()
        .map(|row| row.as_str())
        .find(|row| !inner(row).is_empty())
        .unwrap_or("")
}

/// The Bookmark frame inside its rectangle: the count title, exactly one selected row naming
/// the durable index and the lede, and the action footer (the deleted `_bb1b_book`).
fn assert_bookmark_frame(
    h: &TestHarness,
    tabs: &TabManager,
    count: usize,
    index: usize,
    lede: &str,
) {
    let rows = titled_box(h, tabs, &format!("Bookmarks ({count})"));
    let selected: Vec<&str> = rows[1..rows.len() - 1]
        .iter()
        .map(|row| inner(row))
        .filter(|row| row.contains("▸ "))
        .collect();
    assert_eq!(selected.len(), 1, "exactly one selected row: {rows:#?}");
    assert!(
        selected[0].starts_with(&format!("▸ msg {index}: {lede}")),
        "{rows:#?}"
    );
    let footer = footer(&rows);
    assert!(
        ["Jump", "Delete", "Undo", "Close"]
            .iter()
            .all(|action| footer.contains(action)),
        "{rows:#?}"
    );
}

/// The rows whose cells carry `Modifier::REVERSED` in the last painted frame.
fn reversed_rows(h: &TestHarness) -> Vec<u16> {
    let buffer = h.terminal.backend().buffer();
    (0..buffer.area.height)
        .filter(|&y| {
            (0..buffer.area.width).any(|x| {
                buffer[(x, y)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
        })
        .collect()
}

/// The Wave list inside its rectangle: the `▾ k of N` header on the first inner row, the list
/// actions in the footer, and exactly one reversed row, inside the frame, that carries the
/// selected spoke's summary and the `↵` drill mark (the deleted `_bb1b_wave` "list").
fn assert_wave_list(h: &TestHarness, tabs: &TabManager, position: &str, summary: &str) {
    let rows = titled_box(h, tabs, "Wave");
    assert_eq!(inner(&rows[1]), position, "{rows:#?}");
    let footer = footer(&rows);
    assert!(
        ["Enter drill", "r rerun", "d diverge", "Esc/Ctrl+C close"]
            .iter()
            .all(|action| footer.contains(action)),
        "{rows:#?}"
    );
    let rect = modal_box_rect(h, tabs);
    let reversed = reversed_rows(h);
    assert_eq!(reversed.len(), 1, "exactly one selected row: {rows:#?}");
    assert!(
        (rect.y + 1..rect.bottom() - 1).contains(&reversed[0]),
        "the selected row lies inside the frame: {rows:#?}"
    );
    let selected = &rows[usize::from(reversed[0] - rect.y)];
    assert!(
        selected.contains(summary) && selected.contains('↵'),
        "{rows:#?}"
    );
}

/// The Wave drill and divergence frames: the header on the first inner row, the fixture payload
/// in the body and one of the footer action sets (the deleted `_bb1b_wave` "drill" /
/// "diverge").
fn assert_wave_subview(
    h: &TestHarness,
    tabs: &TabManager,
    header: &str,
    payload: &str,
    footers: &[&[&str]],
) {
    let rows = titled_box(h, tabs, "Wave");
    assert_eq!(inner(&rows[1]), header, "{rows:#?}");
    let body = rows[2..rows.len() - 1].join("\n");
    assert!(
        body.contains(payload),
        "payload `{payload}` in the body: {rows:#?}"
    );
    let footer = footer(&rows);
    assert!(
        footers
            .iter()
            .any(|set| set.iter().all(|action| footer.contains(action))),
        "{rows:#?}"
    );
}

#[test]
fn bb1b_i_real_frames_for_smoke_predicates() {
    use rustain::adapters::tui::{
        app::InputAction,
        handlers::{bookmark, wave_overlay},
    };
    use std::sync::Arc;
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            // Owner predicates are painted-and-asserted at the standard shell; every viewport and
            // both composer sizes still keep the draft and its composer row at every step.
            let painted = width == 80 && !full;
            let (mut h, mut tabs) = super::stage_support::stage(width, height);
            h.finalize_turn();
            let draft = if full {
                "1\n2\n3\n4\n5\n6\n7\n8"
            } else {
                DRAFT
            };
            h.state.input_buffer = draft.into();
            h.state.cursor_position = draft.chars().count();
            let before_screen = bb1b_capture(&mut h, &tabs, "before", full, draft);
            let before = super::stage_support::view(&tabs);
            assert_eq!(
                press(&mut h.state, Key::Char('\'')),
                InputAction::OpenBookmarkList
            );
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            bb1b_capture(&mut h, &tabs, "bookmark-open", full, draft);
            if painted {
                assert_bookmark_frame(&h, &tabs, 2, 0, "stage-0-row-00");
            }
            assert_eq!(press(&mut h.state, Key::Char('j')), InputAction::Consumed);
            assert_eq!(h.state.bookmark_list_selected, 1);
            bb1b_capture(&mut h, &tabs, "bookmark-selected", full, draft);
            if painted {
                assert_bookmark_frame(&h, &tabs, 2, 2, "stage-2-row-00");
            }
            assert_eq!(
                press(&mut h.state, Key::Esc),
                InputAction::CloseBookmarkList
            );
            assert_eq!(super::stage_support::view(&tabs), before);
            let screen = bb1b_capture(&mut h, &tabs, "bookmark-back", full, draft);
            if painted {
                let status = status_row(&h, &tabs);
                assert_eq!(
                    &screen_rows(&screen)[..status],
                    &screen_rows(&before_screen)[..status],
                    "the rows above the status row are the pre-open rows: {screen}"
                );
            }
            assert_eq!(
                press(&mut h.state, Key::Char('\'')),
                InputAction::OpenBookmarkList
            );
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            assert_eq!(press(&mut h.state, Key::Enter), InputAction::JumpToBookmark);
            let conversation = tabs.active_tab().conversation.clone();
            bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &conversation);
            assert_ne!(super::stage_support::view(&tabs), before);
            let screen = bb1b_capture(&mut h, &tabs, "bookmark-jump", full, draft);
            if painted {
                assert!(screen.contains("stage-0-row-00"), "{screen}");
            }
            tabs.active_tab_mut().session_meta.bookmarks = vec![0, 1, 2];
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            for _ in 0..2 {
                assert_eq!(press(&mut h.state, Key::Char('j')), InputAction::Consumed);
            }
            bb1b_capture(&mut h, &tabs, "bookmark-counted", full, draft);
            bookmark::close(&mut h.state);
            tabs.active_tab_mut().session_meta.bookmarks.clear();
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
            assert!(!presentation::is_open(&h.state, Surface::BookmarkList));
            h.state.notifications.clear_toasts();
            bb1b_capture(&mut h, &tabs, "bookmark-empty", full, draft);
            h.state.wave_run = Some(Arc::new(Bb1bPayload(super::keymap_support::StubWave(2))));
            let id = tabs.active_tab().conversation.id.clone();
            for (key, expected, stage, position, summary) in [
                (
                    Key::Char('r'),
                    InputAction::OpenWaveOverlay,
                    "wave-open",
                    "▾ 1 of 2",
                    "a1-first-invalid",
                ),
                (
                    Key::Char('j'),
                    InputAction::Consumed,
                    "wave-selected",
                    "▾ 2 of 2",
                    "a1-second-invalid",
                ),
            ] {
                let action = press(&mut h.state, key);
                assert_eq!(action, expected, "{stage}");
                wave_overlay::apply(&mut h.state, &id, &action);
                bb1b_capture(&mut h, &tabs, stage, full, draft);
                if painted {
                    assert_wave_list(&h, &tabs, position, summary);
                }
            }
            // Fixture payload, not a claimed successful lazy handle resolution.
            h.state.wave_drill_body = Some((1, "a1-second-invalid".into()));
            bb1b_capture(&mut h, &tabs, "wave-drill", full, draft);
            if painted {
                assert_wave_subview(
                    &h,
                    &tabs,
                    "Drill: spoke 1",
                    "a1-second-invalid",
                    &[
                        &["Enter drill", "r rerun", "d diverge", "Esc/Ctrl+C close"],
                        &[
                            "Enter/r overlay",
                            "d diverge",
                            "Esc dismiss",
                            "Ctrl+C close",
                        ],
                    ],
                );
            }
            for (key, expected, stage) in [
                (Key::Char('d'), InputAction::OpenDivergeView, "wave-diverge"),
                (
                    Key::Char('s'),
                    InputAction::CloseDivergeView,
                    "wave-drill-return",
                ),
                (Key::Ctrl('c'), InputAction::CloseWaveOverlay, "wave-closed"),
                (
                    Key::Char('d'),
                    InputAction::OpenDivergeView,
                    "wave-direct-diverge",
                ),
                (Key::Esc, InputAction::CloseDivergeView, "wave-compact"),
                (Key::Enter, InputAction::OpenWaveOverlay, "wave-reopen"),
                (
                    Key::Char('d'),
                    InputAction::OpenDivergeView,
                    "wave-list-diverge",
                ),
                (Key::Esc, InputAction::CloseDivergeView, "wave-list-compact"),
            ] {
                let action = press(&mut h.state, key);
                assert_eq!(action, expected, "{stage}");
                wave_overlay::apply(&mut h.state, &id, &action);
                bb1b_capture(&mut h, &tabs, stage, full, draft);
                if painted && stage == "wave-diverge" {
                    assert_wave_subview(
                        &h,
                        &tabs,
                        "2 spokes disagree",
                        "a1-second-invalid",
                        &[&["s back", "Esc back", "Ctrl+C back"]],
                    );
                }
            }
            for (count, selected, stage) in [(0, 0, "wave-empty"), (24, 23, "wave-counted")] {
                h.state.wave_run = Some(Arc::new(Bb1bPayload(super::keymap_support::StubWave(
                    count,
                ))));
                let action = press(&mut h.state, Key::Char('r'));
                assert_eq!(action, InputAction::OpenWaveOverlay, "{stage}");
                wave_overlay::apply(&mut h.state, &id, &action);
                h.state.wave_overlay_selected = selected;
                bb1b_capture(&mut h, &tabs, stage, full, draft);
                wave_overlay::apply(&mut h.state, &id, &InputAction::CloseWaveOverlay);
            }
        }
    }
}

fn bb1b_ii_capture(h: &mut TestHarness, tabs: &mut TabManager, stage: &str, full: bool) {
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
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            let draft = if full {
                "1\n2\n3\n4\n5\n6\n7\n8"
            } else {
                DRAFT
            };
            let (mut h, mut tabs, owner) = bb1b_ii_open_fixture(width, height, draft);
            bb1b_ii_capture(&mut h, &mut tabs, "cross-unfiltered", full);
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
                bb1b_ii_capture(&mut h, &mut tabs, stage, full);
            }
            assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
            export::close_cross_search(&mut h.state);
            assert_eq!(h.state.modal_stack.len(), 1);
            assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::History);
            assert_eq!(h.state.input_buffer, draft);
            assert_eq!(h.state.cursor_position, draft.chars().count());
            if width == 130 {
                bb1b_ii_capture(&mut h, &mut tabs, "history-return", full);
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
            bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "cross-untitled", full);
            bb1b_ii_request_quit(&mut h.state);
            bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "cross-quit", full);
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
                bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "accepted-base", full);
            } else {
                assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
                export::close_cross_search(&mut h.state);
                bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "history-populated", full);
                if full {
                    let persisted = &h.conversation;
                    assert_eq!(persisted.messages.len(), 2);
                    assert_eq!(persisted.messages[0].content, TURN);
                }
            }
            bb1b_ii_request_quit(&mut h.state);
            bb1b_ii_aux_capture(&mut h, &mut tabs, &index, "base-quit", full);
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
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-unfiltered"), full);
                paste(&mut h, &mut tabs, needle);
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-filtered"), full);
                key(&mut h, &mut tabs, Key::Home);
                key(&mut h, &mut tabs, Key::Right);
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-caret"), full);
                paste(&mut h, &mut tabs, "x");
                let empty = capture(&mut h, &tabs, "bb2b", &format!("{kind}-empty"), full);
                assert!(empty.contains(if kind == "log" {
                    "No matching notifications."
                } else {
                    "No matching keybindings."
                }));
                key(&mut h, &mut tabs, Key::Ctrl('h'));
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-restored"), full);
                key(&mut h, &mut tabs, Key::Enter);
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-accepted"), full);
                key(&mut h, &mut tabs, Key::Char('/'));
                key(&mut h, &mut tabs, Key::Esc);
                capture(&mut h, &tabs, "bb2b", &format!("{kind}-cleared"), full);
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
            capture(&mut h, &tabs, "bb2b", "cross-unfiltered", full);
            paste(&mut h, &mut tabs, "Bb1a");
            bb2b_publish_cross(&mut h, 1);
            capture(&mut h, &tabs, "bb2b", "cross-filtered", full);
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            capture(&mut h, &tabs, "bb2b", "cross-caret", full);
            paste(&mut h, &mut tabs, "x");
            bb2b_publish_cross(&mut h, 0);
            capture(&mut h, &tabs, "bb2b", "cross-empty", full);
            key(&mut h, &mut tabs, Key::Ctrl('h'));
            bb2b_publish_cross(&mut h, 1);
            capture(&mut h, &tabs, "bb2b", "cross-restored", full);
            key(&mut h, &mut tabs, Key::End);
            bb2b_publish_cross(&mut h, 20);
            for _ in 0..19 {
                key(&mut h, &mut tabs, Key::Down);
            }
            capture(&mut h, &tabs, "bb2b", "cross-counted", full);
            assert_eq!(
                h.state.cross_search.selected, 19,
                "selected-window control is reachable"
            );
            assert_eq!(h.state.modal_stack.top().unwrap().id, owner);
            assert_eq!(h.state.input_buffer, draft);
        }
    }
}

/// The local composer body row is `│` + the draft padded to the inner width + `│`, and the
/// visible cursor sits just inside the left border at `(cursor_position + 1, height - 2)`.
fn assert_local_composer(h: &mut TestHarness, screen: &str, draft: &str, caret: usize) {
    assert_eq!(
        (&*h.state.input_buffer, h.state.cursor_position),
        (draft, caret)
    );
    let rows = screen_rows(screen);
    let inner = usize::from(h.state.terminal_width) - 2;
    assert_eq!(
        rows[rows.len() - 2],
        format!("│{draft:<inner$}│"),
        "{screen}"
    );
    assert!(h.terminal.backend().cursor_visible(), "{screen}");
    let cursor = h.terminal.get_cursor_position().unwrap();
    assert_eq!(
        (usize::from(cursor.x), usize::from(cursor.y)),
        (caret + 1, rows.len() - 2),
        "{screen}"
    );
}

/// One Help `j` moves the body up by exactly one row; both border rows are unchanged.
fn assert_help_scrolled_one_row(before: &[String], after: &[String]) {
    let last = before.len() - 1;
    assert!(
        before[0].contains("Help — Rustain Keybindings"),
        "{before:?}"
    );
    assert!(before[last].contains("j/k scroll  ? close"), "{before:?}");
    assert_eq!(after[0], before[0], "top border row");
    assert_eq!(after[last], before[last], "bottom border row");
    let (before_body, after_body) = (&before[1..last], &after[1..last]);
    assert_eq!(
        &before_body[1..],
        &after_body[..after_body.len() - 1],
        "the Help body shifts up one row"
    );
}

fn c_capture_local(h: &mut TestHarness, tabs: &mut TabManager, stage: &str) -> String {
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
    check_frame(h, "c-local", stage, false)
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
        c_capture_local(&mut h, &mut tabs, "initial");
        for c in "/cnotice ".chars() {
            press(&mut h.state, Key::Char(c));
        }
        assert_eq!(h.state.input_buffer, "/cnotice ");
        assert_eq!(h.state.cursor_position, 9);
        c_capture_local(&mut h, &mut tabs, "notice-command");
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
        c_capture_local(&mut h, &mut tabs, "notice");
        for k in [Key::Ctrl('x'), Key::Char('n')] {
            press(&mut h.state, k);
        }
        c_capture_local(&mut h, &mut tabs, "notice-log");
        for k in [Key::Char('x'), Key::Esc] {
            press(&mut h.state, k);
        }
        c_capture_local(&mut h, &mut tabs, "notice-dismiss-return");
        for c in "C-kept draft".chars() {
            press(&mut h.state, Key::Char(c));
        }
        let typed = c_capture_local(&mut h, &mut tabs, "typed");
        assert_local_composer(&mut h, &typed, "C-kept draft", 12);
        press(&mut h.state, Key::Ctrl('a'));
        assert_eq!(h.state.cursor_position, 0);
        c_capture_local(&mut h, &mut tabs, "readline-home");
        press(&mut h.state, Key::Ctrl('e'));
        assert_eq!(h.state.cursor_position, 12);
        press(&mut h.state, Key::Left);
        assert_eq!(h.state.cursor_position, 11);
        c_capture_local(&mut h, &mut tabs, "left");
        press(&mut h.state, Key::End);
        press(&mut h.state, Key::Home);
        press(&mut h.state, Key::Right);
        assert_eq!(h.state.cursor_position, 1);
        let caret = c_capture_local(&mut h, &mut tabs, "caret");
        assert_local_composer(&mut h, &caret, "C-kept draft", 1);
        handle_input(&mut h.state, &DomainInputEvent::Paste("X".into()));
        assert_eq!(
            (&*h.state.input_buffer, h.state.cursor_position),
            ("CX-kept draft", 2)
        );
        let pasted = c_capture_local(&mut h, &mut tabs, "paste");
        assert_local_composer(&mut h, &pasted, "CX-kept draft", 2);
        press(&mut h.state, Key::Ctrl('k'));
        assert_eq!((&*h.state.input_buffer, h.state.cursor_position), ("CX", 2));
        let kill_tail = c_capture_local(&mut h, &mut tabs, "kill-tail");
        assert_local_composer(&mut h, &kill_tail, "CX", 2);
        press(&mut h.state, Key::Ctrl('u'));
        assert!(h.state.input_buffer.is_empty());
        let cleared = c_capture_local(&mut h, &mut tabs, "kill-prefix");
        assert_local_composer(&mut h, &cleared, "", 0);
        for c in "CX".chars() {
            press(&mut h.state, Key::Char(c));
        }
        let word = c_capture_local(&mut h, &mut tabs, "word-fixture");
        assert_local_composer(&mut h, &word, "CX", 2);
        press(&mut h.state, Key::Ctrl('w'));
        let killed = c_capture_local(&mut h, &mut tabs, "kill-word");
        assert_local_composer(&mut h, &killed, "", 0);
        for c in "C-kept draft".chars() {
            press(&mut h.state, Key::Char(c));
        }
        let bottom = c_capture_local(&mut h, &mut tabs, "bottom");
        assert_local_composer(&mut h, &bottom, "C-kept draft", 12);
        assert!(bottom.contains(REPLY));
        // Local full-page up is Chat Ctrl+B; attached offers Input PgUp.
        press(&mut h.state, Key::Esc);
        assert_eq!(h.state.focus, FocusState::Chat);
        c_capture_local(&mut h, &mut tabs, "chat-before-scroll");
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
        c_capture_local(&mut h, &mut tabs, "chat-scrolled");
        press(&mut h.state, Key::Char('i'));
        assert_eq!(h.state.focus, FocusState::Input);
        let scrolled = c_capture_local(&mut h, &mut tabs, "scrolled");
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
            c_capture_local(&mut h, &mut tabs, kind);
            if kind == "help" {
                let before = h.state.help_overlay.scroll_offset;
                let open_rows = modal_box_rows(&h, &tabs);
                press(&mut h.state, Key::Char('j'));
                assert_eq!(h.state.help_overlay.scroll_offset, before + 1);
                c_capture_local(&mut h, &mut tabs, "help-scrolled");
                assert_help_scrolled_one_row(&open_rows, &modal_box_rows(&h, &tabs));
            }
            press(&mut h.state, Key::Esc);
            assert!(h.state.modal_stack.is_empty(), "owner actually released");
            let restored = c_capture_local(&mut h, &mut tabs, &format!("{kind}-return"));
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
