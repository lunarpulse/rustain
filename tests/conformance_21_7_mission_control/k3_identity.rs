//! K3 / AC4 / A3: selection is a row id. A synthetic two-row tab registered only here goes
//! through the shared list builder (7-A proves it; 7-B..7-D rerun it on real tabs): a refresh
//! that removes the selected row leaves a muted `… is no longer available` placeholder at the
//! old position and rebinds nothing; a refresh that only moves it keeps it selected; the
//! master/detail split needs 96 inner columns; every painted row is a row the keys can reach
//! (height == paint) at widths 60-200 and heights 16-60.
use super::support::*;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::Surface;

async fn probe(width: u16, height: u16, ids: &[&str]) -> Rig {
    let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
    open_room(&mut rig).await;
    rig.f
        .h
        .state
        .mission_control
        .register_probe(probe_rows(ids));
    for tab in [
        rustain::adapters::tui::mission_control::McTab::Artifacts,
        rustain::adapters::tui::mission_control::McTab::Probe,
    ] {
        assert_eq!(
            rig.f.press(Key::Right),
            InputAction::OpenMissionControl(tab)
        );
    }
    rig.f.paint();
    assert_eq!(top_title(&rig).as_deref(), Some("Mission Control · Probe"));
    rig
}

/// The painted master rows, marker included, in order.
fn rows_of(rig: &Rig) -> Vec<String> {
    body_lines(rig)
        .iter()
        .filter_map(|line| {
            let at = line.find("▸ ").or_else(|| line.find("  row "))?;
            Some(line[at..].trim_end().to_owned())
        })
        .filter(|row| row.contains("row ") || row.contains("no longer available"))
        .collect()
}

#[tokio::test]
async fn removed_selection_shows_unavailable_not_the_next_row() {
    let mut rig = probe(80, 24, &["a", "b", "c", "d"]).await;
    // Select `b` with the real key and see it marked.
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    let rows = rows_of(&rig);
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(rows[1].starts_with("▸ row b"), "{rows:?}");
    // Refresh: `b` is gone; the rows below it moved up.
    rig.f
        .h
        .state
        .mission_control
        .register_probe(probe_rows(&["a", "c", "d"]));
    rig.f.paint();
    let rows = rows_of(&rig);
    assert_eq!(
        rows.len(),
        4,
        "the placeholder takes the old position: {rows:?}"
    );
    assert!(rows[0].starts_with("  row a"), "{rows:?}");
    assert!(
        rows[1].starts_with("▸ row b is no longer available"),
        "the placeholder sits where `b` sat: {rows:?}"
    );
    assert!(
        rows[2].starts_with("  row c") && rows[3].starts_with("  row d"),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.starts_with("▸ row c")),
        "the next row is never silently selected: {rows:?}"
    );
    // Enter acts on nothing (no window exists yet and no row is selected): still the hub.
    assert_eq!(rig.f.press(Key::Enter), InputAction::Consumed);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    // Moving off the placeholder is the operator's choice: down lands on the row that took
    // its position, and the placeholder is gone.
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    let rows = rows_of(&rig);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[1].starts_with("▸ row c"), "{rows:?}");
}

#[tokio::test]
async fn a_moved_selection_keeps_its_identity_and_a_removed_one_up_moves_to_the_row_above() {
    let mut rig = probe(80, 24, &["a", "b", "c"]).await;
    rig.f.press(Key::Char('j'));
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    assert!(
        rows_of(&rig)[2].starts_with("▸ row c"),
        "{:?}",
        rows_of(&rig)
    );
    // Positive control: `c` is still `c` at a new index (a row was inserted above it).
    rig.f
        .h
        .state
        .mission_control
        .register_probe(probe_rows(&["x", "a", "b", "c"]));
    rig.f.paint();
    let rows = rows_of(&rig);
    assert!(
        rows[3].starts_with("▸ row c"),
        "still `c`, now at index 3: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("no longer available")),
        "{rows:?}"
    );
    // The selected row is removed and the cursor moves up: the row above is chosen.
    rig.f
        .h
        .state
        .mission_control
        .register_probe(probe_rows(&["x", "a", "b"]));
    rig.f.press(Key::Char('k'));
    rig.f.paint();
    let rows = rows_of(&rig);
    assert!(rows[2].starts_with("▸ row b"), "{rows:?}");
}

#[tokio::test]
async fn the_split_needs_96_inner_columns_and_tab_moves_between_the_panes() {
    // 98 columns is the narrowest wide frame: 96 inner columns.
    // The frame is a full sheet below 100 terminal columns and 90 % of the terminal from 100
    // (A2 measured against `frame_rect`): 100 columns paint a 90-column frame (88 inner), 109
    // columns the first 98-column one.
    for (width, wide) in [
        (80u16, false),
        (97, false),
        (98, true),
        (99, true),
        (100, false),
        (108, false),
        (109, true),
        (130, true),
    ] {
        let mut rig = probe(width, 30, &["a", "b"]).await;
        let detail = body_lines(&rig)
            .iter()
            .any(|line| line.contains("detail of a"));
        assert_eq!(detail, wide, "{width} columns: {:?}", body_lines(&rig));
        let rail = rail_line(&rig);
        assert_eq!(rail.contains("[Tab]"), wide, "{width}: {rail:?}");
        let _ = rig.f.press(Key::Tab);
        rig.f.paint();
        let focused = rail_line(&rig).contains("[Tab] List");
        assert_eq!(
            focused, wide,
            "{width}: Tab moves to the detail only where it is painted"
        );
    }
    // In the detail pane the movement keys scroll the preview and never the selection.
    let mut rig = probe(130, 24, &["a", "b"]).await;
    let long: String = (0..60).map(|n| format!("line {n:02}\n")).collect();
    rig.f.h.state.mission_control.register_probe(vec![
        rustain::adapters::tui::mission_control::McRow::new("a", "row a", &long),
        rustain::adapters::tui::mission_control::McRow::new("b", "row b", "short"),
    ]);
    rig.f.paint();
    assert!(body_lines(&rig).iter().any(|line| line.contains("line 00")));
    rig.f.press(Key::Tab);
    rig.f.paint();
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    let rows = rows_of(&rig);
    assert!(
        rows[0].starts_with("▸ row a"),
        "selection unmoved: {rows:?}"
    );
    assert!(
        !body_lines(&rig).iter().any(|line| line.contains("line 00"))
            && body_lines(&rig).iter().any(|line| line.contains("line 01")),
        "the preview scrolled one row: {:?}",
        body_lines(&rig)
    );
    // Back in the master pane the same key moves the selection.
    rig.f.press(Key::Tab);
    rig.f.paint();
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    assert!(
        rows_of(&rig)[1].starts_with("▸ row b"),
        "{:?}",
        rows_of(&rig)
    );
}

#[tokio::test]
async fn height_equals_paint_at_every_width_and_height() {
    let ids: Vec<String> = (0..70).map(|n| format!("{n:03}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    for width in [60u16, 61, 80, 97, 98, 99, 100, 130, 131, 200] {
        for height in [16u16, 17, 20, 24, 30, 45, 60] {
            let mut rig = probe(width, height, &refs).await;
            let rect = frame_rect(&rig);
            let viewport = usize::from(rect.height - 2 - 1 - 1);
            let fit = rig.f.h.state.mission_control.fit();
            assert_eq!(
                fit.list_viewport, viewport,
                "{width}x{height}: the fit is the painted body"
            );
            let painted = rows_of(&rig);
            assert_eq!(
                painted.len(),
                viewport.min(70),
                "{width}x{height}: every viewport row is painted: {painted:?}"
            );
            // Every painted row is reachable by the keys: G reaches the last, g the first.
            rig.f.press(Key::Char('G'));
            rig.f.paint();
            let painted = rows_of(&rig);
            assert!(
                painted
                    .last()
                    .is_some_and(|row| row.starts_with("▸ row 069")),
                "{width}x{height}: G selects and shows the last row: {painted:?}"
            );
            assert_eq!(painted.len(), viewport, "{width}x{height}");
            rig.f.press(Key::Char('g'));
            rig.f.paint();
            assert!(
                rows_of(&rig)[0].starts_with("▸ row 000"),
                "{width}x{height}: {:?}",
                rows_of(&rig)
            );
        }
    }
}

#[tokio::test]
async fn a_list_tab_with_no_rows_says_so() {
    let mut rig = probe(80, 24, &[]).await;
    assert!(
        body_lines(&rig)
            .iter()
            .any(|line| line.contains("Nothing here yet.")),
        "{:?}",
        body_lines(&rig)
    );
    assert_eq!(rig.f.press(Key::Char('j')), InputAction::Consumed);
    assert_eq!(rig.f.press(Key::Char('G')), InputAction::Consumed);
    rig.f.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
}

// 7A-03 (patch round 1): the detail preview is laid out once per (selection, width, content
// revision) and reused by every unchanged repaint.
#[tokio::test]
async fn the_detail_preview_is_cached_across_unchanged_repaints() {
    let mut rig = probe(130, 30, &["a", "b"]).await;
    let long: String = (0..400)
        .map(|n| format!("line {n:03} {}\n", "w".repeat(150)))
        .collect();
    rig.f.h.state.mission_control.register_probe(vec![
        rustain::adapters::tui::mission_control::McRow::new("a", "row a", &long),
        rustain::adapters::tui::mission_control::McRow::new("b", "row b", "short"),
    ]);
    rig.f.paint();
    let built = rig.f.h.state.mission_control.detail_builds();
    for _ in 0..5 {
        rig.f.paint();
    }
    assert_eq!(
        rig.f.h.state.mission_control.detail_builds(),
        built,
        "unchanged repaints reuse the wrapped rows"
    );
    // Scrolling the preview and moving the pane are not content changes.
    rig.f.press(Key::Tab);
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    assert_eq!(rig.f.h.state.mission_control.detail_builds(), built);
    // A changed selection, a refresh of the content and a resize each rebuild exactly once.
    rig.f.press(Key::Tab);
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    rig.f.paint();
    assert_eq!(
        rig.f.h.state.mission_control.detail_builds(),
        built + 1,
        "selection"
    );
    rig.f.h.state.mission_control.register_probe(vec![
        rustain::adapters::tui::mission_control::McRow::new("a", "row a", &long),
        rustain::adapters::tui::mission_control::McRow::new("b", "row b", "changed"),
    ]);
    rig.f.paint();
    rig.f.paint();
    assert_eq!(
        rig.f.h.state.mission_control.detail_builds(),
        built + 2,
        "content"
    );
    rig.f.h.terminal.backend_mut().resize(120, 30);
    rig.f.h.state.terminal_width = 120;
    rig.f.paint();
    rig.f.paint();
    assert_eq!(
        rig.f.h.state.mission_control.detail_builds(),
        built + 3,
        "width"
    );
}
