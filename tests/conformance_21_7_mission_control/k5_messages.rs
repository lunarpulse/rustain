//! K5 / AC20 / A12 / A13 (21-7 slice 7-B): the A2A log is Mission Control's Messages tab — 21-8's
//! log view re-homed from S-THREAD (`Surface::ThreadInspector` + `ThreadTarget::Log` are gone).
//! The page is one filterable turn per journal row with the closing claim kept under every
//! filter; its visit is acknowledged only after a completed draw painted the body on a hub that
//! is still the top frame (19.16g). Every fixture goes through a production door: the journal
//! is written by the production `NodeJournal`/`NodeRoomJournal`, `/team log` runs through the
//! production bridge, keys travel through the local host's decoder, and every assertion reads
//! painted cells, the one frame on the stack or the page the frame owns.
use super::support::*;
use crate::conformance_21_8_thread_inspector::support::{Item, consultation_workspace};
use ratatui::style::Modifier;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::{LogPage, McTab};
use rustain::adapters::tui::presentation::inspector_search::InspectorSearch;
use rustain::adapters::tui::presentation::markdown_body::BodyMode;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::thread_body::{self, PageView};
use rustain::domain::models::SemanticMessageType;
use rustain::infrastructure::runtime::transparency_bridge;

const TITLE: &str = "Mission Control · Messages";

/// `/team log [args]` through the production bridge (the journal service composed first).
async fn log(rig: &mut Rig, args: &str) {
    attach_journal(rig);
    transparency_bridge::team_command(&mut rig.f.h.state, "origin", Some(args), &rig.app).await;
}

/// One compose pass of the production frame pipeline as `host`, without the host's post-draw
/// hook: the visit is neither committed nor restored.
fn draw_as(rig: &mut Rig, host: rustain::adapters::tui::keymap::Host) {
    use rustain::adapters::tui::{notification_log::LogFacts, presentation::Shell};
    let layout = rig.f.layout();
    let state = &rig.f.h.state;
    rig.f
        .h
        .terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            presentation::compose(
                frame,
                state,
                &shell,
                host,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
}

fn draw_without_commit(rig: &mut Rig) {
    draw_as(rig, rustain::adapters::tui::keymap::Host::Local);
}

fn page(rig: &Rig) -> &LogPage {
    rig.f
        .h
        .state
        .mission_control
        .messages
        .as_ref()
        .expect("the Messages page is open")
}

fn items(count: usize) -> Vec<Item> {
    (0..count)
        .map(|n| {
            Item::new(
                &format!("ri_{n:03}"),
                SemanticMessageType::Consultation,
                &format!("log marker {n:03}"),
            )
        })
        .collect()
}

async fn populated() -> Rig {
    Rig::plain(Workspace::with(&items(25)).await, 130, 40).await
}

/// The painted body rows of the Messages tab, every one of them: collected from what is
/// PAINTED while the viewer pages from the top to the end through the real keys. The result
/// has exactly `fit().0` rows (the clamped row count is the painted row count).
fn painted_all(rig: &mut Rig) -> Vec<String> {
    rig.key(Key::Char('g'));
    rig.paint();
    let (rows, viewport) = page(rig).fit();
    let max = rows.saturating_sub(viewport);
    let mut all: Vec<Option<String>> = vec![None; rows];
    for _ in 0..=rows + 2 {
        let top = page(rig).viewer.top.min(max);
        for (n, row) in body_lines(rig)
            .into_iter()
            .take(viewport)
            .take(rows.saturating_sub(top))
            .enumerate()
        {
            all[top + n] = Some(row.trim_end().to_owned());
        }
        if top >= max {
            break;
        }
        rig.key(Key::PageDown);
        rig.paint();
    }
    all.into_iter()
        .enumerate()
        .map(|(n, row)| row.unwrap_or_else(|| panic!("body row {n} of {rows} was never painted")))
        .collect()
}

fn viewport_text(rig: &Rig) -> String {
    let viewport = page(rig).fit().1;
    body_lines(rig)
        .into_iter()
        .take(viewport)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn log_display_escapes_bidi_without_changing_export() {
    use rustain::domain::models::{Direction, PeerId, RejectReason, RoomEvent};
    use rustain::domain::services::transparency::render_export;
    let ws = Workspace::new().await;
    ws.room
        .record_event(RoomEvent::RemoteEnvelopeRejected {
            rail: None,
            peer: PeerId::from_public_key(&[7; 32]).unwrap(),
            reason: RejectReason::Policy {
                detail: "سلام\u{202e}A\u{2066}B\u{1b}".into(),
            },
            direction: Direction::Inbound,
            task: None,
        })
        .await
        .unwrap();
    let journal_bytes = std::fs::read(ws.node.path()).unwrap();
    let mut rig = Rig::plain(ws, 160, 40).await;
    for args in ["log", "log --json"] {
        log(&mut rig, args).await;
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{args}");
        let service = rig.app.transparency.as_ref().unwrap();
        let before = render_export(&service.report().await.unwrap().rows);
        assert!(before.contains('\u{202e}') && before.contains('\u{2066}'));
        let lines = thread_body::build(page(&rig).page(), &rig.f.h.state.theme, 150).lines;
        let spans: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        for expected in ["\\u{202e}", "\\u{2066}", "سلام"] {
            assert!(spans.contains(expected), "{args}: {spans:?}");
        }
        assert!(!spans.chars().any(|c| c == '\u{1b}'
            || ('\u{202a}'..='\u{202e}').contains(&c)
            || ('\u{2066}'..='\u{2069}').contains(&c)));
        let painted = painted_all(&mut rig).join("");
        for expected in ["\\u{202e}", "\\u{2066}", "سلام"] {
            assert!(painted.contains(expected), "{args}: {painted:?}");
        }
        assert!(
            !rig.f
                .buffer()
                .content
                .iter()
                .flat_map(|cell| cell.symbol().chars())
                .any(|c| c == '\u{1b}'
                    || ('\u{202a}'..='\u{202e}').contains(&c)
                    || ('\u{2066}'..='\u{2069}').contains(&c))
        );
        assert_eq!(
            before,
            render_export(
                &rig.app
                    .transparency
                    .as_ref()
                    .unwrap()
                    .report()
                    .await
                    .unwrap()
                    .rows
            )
        );
        assert_eq!(journal_bytes, std::fs::read(rig.ws.node.path()).unwrap());
    }
}

#[tokio::test]
async fn never_drawn_log_visit_never_commits() {
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    assert_eq!(rig.top(), Surface::MissionControl);
    assert_eq!(top_title(&rig).as_deref(), Some(TITLE));
    let visit = rig.f.h.state.pending_log_visit.expect("unfiltered visit");
    assert!(rig.f.h.state.log_awareness.presented.is_empty());
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.pending_log_visit, Some(visit));
    assert!(rig.f.h.state.log_awareness.presented.is_empty());
    draw_without_commit(&mut rig);
    assert!(lines(&rig)[usize::from(frame_rect(&rig).y)].contains(TITLE));
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.as_slice(), &[visit]);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 1);
}

// Mutation M3's killer: the commit rule is "a completed draw painted the Messages body while
// the hub was the top frame and no leader menu hid it". Each way of breaking one clause leaves
// the visit pending; the one clean draw commits it exactly once.
#[tokio::test]
async fn messages_visit_commits_only_after_a_painted_top_draw() {
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    let visit = rig.f.h.state.pending_log_visit.expect("unfiltered visit");
    // No draw yet: the hook has nothing to consume.
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.pending_log_visit, Some(visit), "never drawn");
    // A completed draw that painted a cover over the hub (a palette): not top, not presented,
    // and the paint of the covered body is consumed, never carried to a later call.
    rig.f.press(Key::Ctrl('p'));
    assert_ne!(top_surface(&rig), Some(Surface::MissionControl));
    rig.f.paint();
    assert_eq!(rig.f.h.state.pending_log_visit, Some(visit), "covered");
    assert!(rig.f.h.state.log_awareness.presented.is_empty());
    rig.f.press(Key::Esc);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    rig.f.h.state.log_visits_presented();
    assert_eq!(
        rig.f.h.state.pending_log_visit,
        Some(visit),
        "the covered draw's paint mark does not carry over"
    );
    // A pending leader menu hides the body: the draw under it is not a presentation.
    rig.f.press(Key::Ctrl('x'));
    assert!(rig.f.h.state.pending_leader.is_some());
    rig.f.paint();
    assert_eq!(rig.f.h.state.pending_log_visit, Some(visit), "leader menu");
    rig.f.press(Key::Esc);
    assert!(rig.f.h.state.pending_leader.is_none());
    rig.f.h.state.log_visits_presented();
    assert_eq!(
        rig.f.h.state.pending_log_visit,
        Some(visit),
        "no draw has painted the body since the menu"
    );
    // An interrupted draw (the pre-draw hook ran, the draw never completed) voids the paint.
    draw_without_commit(&mut rig);
    rig.f.h.state.restore_unflushed_log_visits();
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.pending_log_visit, Some(visit), "interrupted");
    // Positive control: one completed draw of the body on the top hub.
    rig.f.paint();
    assert_eq!(rig.f.h.state.pending_log_visit, None);
    assert_eq!(rig.f.h.state.log_awareness.presented.as_slice(), &[visit]);
    rig.f.paint();
    assert_eq!(
        rig.f.h.state.log_awareness.presented.len(),
        1,
        "committed once"
    );
}

#[tokio::test]
async fn json_filtered_replaced_closed_covered_and_interrupted_visits() {
    let mut rig = populated().await;
    log(&mut rig, "log --json").await;
    let json = rig.f.h.state.pending_log_visit.expect("JSON is unfiltered");
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.as_slice(), &[json]);
    log(&mut rig, "log --filter=kind=item-received").await;
    assert!(rig.f.h.state.pending_log_visit.is_none());
    assert_eq!(
        top_title(&rig).as_deref(),
        Some("Mission Control · Messages · filtered")
    );
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 1);
    log(&mut rig, "log").await;
    let older = rig.f.h.state.pending_log_visit.unwrap();
    rig.ws
        .receive(&Item::new(
            "ri_new",
            SemanticMessageType::Consultation,
            "new row",
        ))
        .await;
    log(&mut rig, "log").await;
    let newer = rig.f.h.state.pending_log_visit.unwrap();
    assert!(newer.seen_through > older.seen_through);
    assert_eq!(rig.depth(), 1, "a second open retargets the one frame");
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(
        rig.f.h.state.log_awareness.presented.as_slice(),
        &[json, newer]
    );
    // Closed before a draw: the visit is never committed.
    log(&mut rig, "log").await;
    rig.key(Key::Esc);
    assert!(rig.f.h.state.mission_control.messages.is_none());
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 2);
    // Covered by a palette.
    log(&mut rig, "log").await;
    rig.key(Key::Ctrl('p'));
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 2);
    rig.key(Key::Esc);
    // Interrupted draw, then the next completed one presents it.
    draw_without_commit(&mut rig);
    rig.f.h.state.restore_unflushed_log_visits();
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 2);
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 3);
}

#[tokio::test]
async fn log_is_uncapped_and_reader_only() {
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    let all = painted_all(&mut rig).join("\n");
    assert!(!all.contains("showing the 20 most recent"));
    for n in 0..25 {
        assert!(
            all.contains(&format!("ri_{n:03}")),
            "row {n} missing: {all}"
        );
    }
    assert!(all.contains("append-only"), "{all}");
    rig.key(Key::Char('G'));
    rig.paint();
    assert!(page(&rig).viewer.top > 0 || page(&rig).viewer.follow);
    assert!(viewport_text(&rig).contains("append-only"));
    let rail = rail_line(&rig);
    assert!(
        !rail.contains("[c]") && !rail.contains("[v]") && !rail.contains("[x]"),
        "{rail}"
    );
    let content = page(&rig).content.clone();
    for key in [Key::Char('c'), Key::Char('v'), Key::Enter, Key::Char('x')] {
        rig.key(key);
    }
    assert_eq!(page(&rig).content, content);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    rig.paint();
    rig.key(Key::Char('/'));
    assert!(page(&rig).search.editing);
    for c in "ri_010".chars() {
        rig.key(Key::Char(c));
    }
    rig.key(Key::Enter);
    let filtered = painted_all(&mut rig).join("\n");
    assert!(
        filtered.contains("ri_010") && filtered.contains("append-only"),
        "{filtered}"
    );
    assert!(!filtered.contains("ri_009"));
}

#[tokio::test]
async fn local_leader_log_opens_messages_not_a_sidebar_panel() {
    let mut rig = populated().await;
    rig.key(Key::Ctrl('x'));
    assert_eq!(rig.key(Key::Char('l')), InputAction::OpenMessages);
    assert!(
        rig.f.h.state.modal_stack.is_empty(),
        "the effect opens the frame, the chord only asks"
    );
    // The effect the event loop runs for the action, then one completed draw.
    attach_journal(&mut rig);
    transparency_bridge::open_messages(&rig.app, &mut rig.f.h.state, "origin").await;
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(top_title(&rig).as_deref(), Some(TITLE));
    assert_eq!(rig.f.h.state.mission_control.tab, McTab::Messages);
    assert!(
        rig.f.h.state.pending_log_visit.is_none(),
        "the completed draw presented the unfiltered visit"
    );
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 1);
}

#[tokio::test]
async fn messages_rows_equal_log_blocks_through_block_page() {
    use rustain::adapters::tui::handlers::team_command::log_blocks;
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    let report = rig
        .app
        .transparency
        .as_ref()
        .unwrap()
        .report()
        .await
        .unwrap();
    let blocks = log_blocks(&report.rows, false);
    assert_eq!(
        blocks.len(),
        report.rows.len() + 1,
        "one block per row and the claim"
    );
    // Plain ASCII blocks: the one-time escape is the identity, so the page is `block_page`.
    let (content, segments, turns) = thread_body::block_page(&blocks, true);
    assert_eq!(page(&rig).content, content);
    assert_eq!(page(&rig).segments, segments);
    assert_eq!(page(&rig).turns.len(), turns.len());
    assert!(page(&rig).turns.last().unwrap().keep, "the claim is kept");
    assert_eq!(page(&rig).turns.iter().filter(|turn| turn.keep).count(), 1);
    let search = InspectorSearch::default();
    let expected = thread_body::build(
        PageView {
            segments: &segments,
            turns: &turns,
            search: &search,
            mode: BodyMode::Rendered,
        },
        &rig.f.h.state.theme,
        100,
    );
    assert_eq!(
        thread_body::build(page(&rig).page(), &rig.f.h.state.theme, 100).lines,
        expected.lines
    );
    // The painted rows are the page's rows, in journal order (oldest first), claim last.
    let painted = painted_all(&mut rig);
    assert_eq!(painted.len(), page(&rig).fit().0);
    let text = painted.join("\n");
    let mut from = 0;
    for n in 0..25 {
        let at = text[from..]
            .find(&format!("ri_{n:03}"))
            .unwrap_or_else(|| panic!("row {n} out of order or missing: {text}"));
        from += at;
    }
    let claim = text[from..]
        .find("append-only")
        .expect("the closing claim follows the newest row");
    assert!(claim > 0);
}

#[tokio::test]
async fn the_filter_keeps_the_closing_claim_and_clears_with_one_escape() {
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    rig.paint();
    rig.key(Key::Char('/'));
    for c in "ri_010".chars() {
        rig.key(Key::Char(c));
    }
    rig.key(Key::Enter);
    let kept = painted_all(&mut rig).join("\n");
    assert!(kept.contains("ri_010"), "{kept}");
    assert!(
        kept.contains("append-only"),
        "the closing claim survives the filter: {kept}"
    );
    assert!(
        !kept.contains("ri_011") && !kept.contains("ri_009"),
        "{kept}"
    );
    assert_eq!(page(&rig).search.query.as_str(), "ri_010");
    // `Esc` clears the kept filter first and the frame stays; the second one closes.
    rig.key(Key::Esc);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert!(page(&rig).search.query.is_empty());
    let all = painted_all(&mut rig).join("\n");
    assert!(all.contains("ri_009") && all.contains("ri_011"), "{all}");
    rig.key(Key::Esc);
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert!(rig.f.h.state.mission_control.messages.is_none());
    // A query that matches nothing is refused and the view is unchanged.
    log(&mut rig, "log").await;
    rig.paint();
    rig.key(Key::Char('/'));
    for c in "zzzz-nothing".chars() {
        rig.key(Key::Char(c));
    }
    rig.key(Key::Enter);
    assert!(
        page(&rig).search.query.is_empty(),
        "no match: filter dropped"
    );
    assert!(painted_all(&mut rig).join("\n").contains("ri_000"));
}

#[tokio::test]
async fn g_jumps_to_the_oldest_row_and_capital_g_to_the_newest() {
    let ws = Workspace::with(&items(80)).await;
    let mut rig = Rig::plain(ws, 130, 24).await;
    log(&mut rig, "log").await;
    rig.paint();
    let (rows, viewport) = page(&rig).fit();
    assert!(
        rows > viewport,
        "the page scrolls: {rows} rows in {viewport}"
    );
    rig.key(Key::Char('G'));
    rig.paint();
    let newest = viewport_text(&rig);
    assert!(
        newest.contains("ri_079"),
        "G shows the newest row: {newest}"
    );
    assert!(newest.contains("append-only"), "{newest}");
    assert!(!newest.contains("ri_000"), "{newest}");
    rig.key(Key::Char('g'));
    rig.paint();
    let oldest = viewport_text(&rig);
    assert!(
        oldest.contains("ri_000"),
        "g shows the oldest row: {oldest}"
    );
    assert!(
        !oldest.contains("ri_079") && !oldest.contains("append-only"),
        "{oldest}"
    );
    // `j` / `k` move by a line and are state-only (the frame stays).
    rig.key(Key::Char('j'));
    rig.paint();
    assert_eq!(page(&rig).viewer.top, 1);
    rig.key(Key::Char('k'));
    rig.paint();
    assert_eq!(page(&rig).viewer.top, 0);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
}

#[tokio::test]
async fn e_exports_the_unfiltered_snapshot_the_view_was_read_from() {
    use rustain::domain::models::{Direction, PeerId, RejectReason, RoomEvent};
    use rustain::domain::services::transparency::render_export;
    let ws = Workspace::with(&items(3)).await;
    ws.room
        .record_event(RoomEvent::RemoteEnvelopeRejected {
            rail: None,
            peer: PeerId::from_public_key(&[9; 32]).unwrap(),
            reason: RejectReason::Policy {
                detail: "zz-hidden-by-the-filter-zz".into(),
            },
            direction: Direction::Inbound,
            task: None,
        })
        .await
        .unwrap();
    let mut rig = Rig::plain(ws, 130, 40).await;
    log(&mut rig, "log --filter=kind=item-received").await;
    rig.paint();
    let filtered = painted_all(&mut rig).join("\n");
    assert!(filtered.contains("ri_000"), "{filtered}");
    assert!(
        !filtered.contains("zz-hidden-by-the-filter-zz"),
        "the view is filtered: {filtered}"
    );
    assert!(rail_line(&rig).contains("Export"), "{}", rail_line(&rig));
    assert_eq!(rig.key(Key::Char('e')), InputAction::ExportTransparency);
    // The snapshot `e` exports is the page's whole report: the rows the view hides included.
    let service = rig.app.transparency.clone().expect("the journal service");
    let fresh = service.report().await.unwrap();
    let report = page(&rig).report.clone().expect("the whole journal report");
    assert_eq!(render_export(&report.rows), render_export(&fresh.rows));
    assert!(render_export(&report.rows).contains("zz-hidden-by-the-filter-zz"));
    // The effect the event loop runs for the action, then the notice it raised.
    let _ = rig.warnings();
    transparency_bridge::export_command(&rig.app, &mut rig.f.h.state, "origin").await;
    let notices = rig.warnings();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let message = &notices[0].1;
    assert!(
        message.contains(&format!("({} unfiltered rows)", fresh.rows.len())),
        "{message}"
    );
    let path = message
        .split("written to ")
        .nth(1)
        .and_then(|rest| rest.split(" (").next())
        .unwrap_or_else(|| panic!("no path in {message}"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        render_export(&fresh.rows),
        "the bytes are the unfiltered snapshot"
    );
}

#[tokio::test]
async fn the_export_key_is_dead_without_a_painted_hint() {
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    // Never painted: the rail has not offered `e` yet.
    assert_eq!(rig.key(Key::Char('e')), InputAction::Consumed);
    rig.paint();
    assert_eq!(rig.key(Key::Char('e')), InputAction::ExportTransparency);
}

#[tokio::test]
async fn an_item_thread_still_opens_s_thread_and_the_log_is_not_it() {
    // Positive control: S-THREAD is untouched for received items.
    let mut rig = Rig::plain(consultation_workspace().await, 130, 40).await;
    rig.view("ri_c").await;
    assert!(rig.is_open());
    assert_eq!(rig.top(), Surface::ThreadInspector);
    rig.paint();
    assert!(
        inner_text(&rig).contains("Please review the plan"),
        "{}",
        inner_text(&rig)
    );
    assert!(rig.f.h.state.mission_control.messages.is_none());
    // The log over it is the hub's Messages tab, not another thread target.
    log(&mut rig, "log").await;
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(top_title(&rig).as_deref(), Some(TITLE));
}

fn inner_text(rig: &Rig) -> String {
    lines(rig).join("\n")
}

#[tokio::test]
async fn the_attached_empty_log_paints_its_row_and_offers_no_export() {
    use rustain::adapters::tui::handlers::team_command::log_blocks;
    use rustain::adapters::tui::keymap::Host;
    use rustain::adapters::tui::mission_control::open_messages;
    let mut rig = Rig::plain(Workspace::new().await, 80, 24).await;
    open_messages(&mut rig.f.h.state, false, &log_blocks(&[], false), None);
    draw_as(&mut rig, Host::Attached);
    assert_eq!(top_title(&rig).as_deref(), Some(TITLE));
    let painted = body_lines(&rig).join("\n");
    assert!(
        painted.contains("· no A2A interactions recorded"),
        "{painted}"
    );
    let tab_row = lines(&rig)[usize::from(frame_rect(&rig).y) + 1].clone();
    assert!(tab_row.contains("Messages"), "{tab_row}");
    assert!(
        !tab_row.contains("Room") && !tab_row.contains("Artifacts"),
        "the attached host delivers Messages only: {tab_row}"
    );
    let rail = rail_line(&rig);
    assert!(!rail.contains("Export"), "attached: no export hint: {rail}");
    assert_eq!(
        rig.key(Key::Char('e')),
        InputAction::Consumed,
        "the key is dead where its hint is not painted"
    );
}

#[tokio::test]
async fn a_local_empty_journal_paints_the_same_row() {
    let mut rig = Rig::plain(Workspace::new().await, 80, 24).await;
    log(&mut rig, "log").await;
    draw_without_commit(&mut rig);
    let painted = body_lines(&rig).join("\n");
    assert!(
        painted.contains("· no A2A interactions recorded"),
        "{painted}"
    );
}

#[tokio::test]
async fn local_stage_indicator_toast_and_dim_leave_the_messages_frame_whole() {
    use rustain::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
    for (width, height) in [(80, 24), (130, 30)] {
        let label = format!("{width}x{height}");
        let mut rig = Rig::plain(Workspace::with(&items(25)).await, width, height).await;
        log(&mut rig, "log").await;
        let visit = rig.f.h.state.pending_log_visit.expect("unfiltered visit");
        let layout = rig.f.layout();
        let rect = frame_rect(&rig);
        // The Stage indicator (a streaming turn in the base) never overwrites a log row.
        rig.f.tabs.active_tab_mut().streaming.is_streaming = true;
        rig.f.paint();
        let buffer = rig.f.buffer();
        assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌", "{label}");
        assert_eq!(
            buffer[(rect.right() - 1, rect.bottom() - 1)].symbol(),
            "┘",
            "{label}"
        );
        assert!(
            lines(&rig)[usize::from(rect.y)].contains(TITLE),
            "{label}: {:?}",
            lines(&rig)[usize::from(rect.y)]
        );
        assert!(viewport_text(&rig).contains("ri_000"), "{label}");
        assert_eq!(
            rig.f.h.state.log_awareness.presented.as_slice(),
            &[visit],
            "{label}: the draw with the indicator presented the visit"
        );
        // Exterior dim: the composer and the Stage region outside the frame are dimmed, the
        // status row is not.
        let dim = |at: (u16, u16)| rig.f.buffer()[at].modifier.contains(Modifier::DIM);
        assert!(
            dim((layout.input_area.x + 1, layout.input_area.y + 1)),
            "{label}: composer"
        );
        assert!(
            !dim((layout.status_bar.x + 1, layout.status_bar.y)),
            "{label}: status"
        );
        // A live toast is painted by the base before the frame: the frame covers the cells it
        // overlaps, and outside the frame the toast is dimmed, never the other way round.
        notifications::notify(
            &mut rig.f.h.state,
            NoticeSource::System,
            NoticeSeverity::Error,
            "K5-toast",
        );
        let mut scratch =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        scratch
            .draw(|frame| {
                rustain::adapters::tui::widgets::toast_layer::render(
                    frame,
                    layout.chat_pane,
                    &rig.f.h.state.notifications,
                    &rig.f.h.state.theme,
                    &rustain::adapters::tui::notification_log::LogFacts::default(),
                );
            })
            .unwrap();
        let toast = scratch.backend().buffer().clone();
        let overlap: Vec<(u16, u16)> = (rect.y..rect.bottom())
            .flat_map(|y| {
                let toast = &toast;
                (rect.x..rect.right())
                    .filter_map(move |x| (toast[(x, y)].symbol() != " ").then_some((x, y)))
            })
            .collect();
        assert!(
            !overlap.is_empty(),
            "{label}: painted toast cells intersect the frame"
        );
        draw_without_commit(&mut rig);
        let composed = rig.f.buffer().clone();
        assert!(
            overlap
                .iter()
                .any(|&(x, y)| composed[(x, y)].symbol() != toast[(x, y)].symbol()),
            "{label}: the frame covers toast cells"
        );
        if width >= 100 {
            assert!(
                (layout.chat_pane.y..layout.chat_pane.bottom()).any(|y| {
                    (layout.chat_pane.x..layout.chat_pane.right()).any(|x| {
                        (x < rect.x || x >= rect.right() || y < rect.y || y >= rect.bottom())
                            && toast[(x, y)].symbol() != " "
                            && composed[(x, y)].symbol() == toast[(x, y)].symbol()
                            && composed[(x, y)].modifier.contains(Modifier::DIM)
                    })
                }),
                "{label}: the toast outside the centered frame is genuinely dimmed"
            );
        }
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{label}");
        assert!(lines(&rig).join("\n").contains(TITLE), "{label}");
    }
}

// 7B-10: a Messages frame waiting for a read that failed says why; it never stays on "Reading…"
// (and an empty successful journal keeps its own row).
#[tokio::test]
async fn a_failed_read_is_painted_and_not_left_on_reading() {
    let mut rig = Rig::plain(Workspace::new().await, 80, 24).await;
    presentation::reducer::open_mission_control(&mut rig.f.h.state, McTab::Messages);
    rig.paint();
    let waiting = inner_text(&rig);
    assert!(waiting.contains("Reading the A2A log"), "{waiting}");
    rig.f.h.state.mission_control.messages_failed = Some("disk on fire".to_owned());
    rig.paint();
    let failed = inner_text(&rig);
    assert!(
        failed.contains("Could not read the A2A log: disk on fire"),
        "{failed}"
    );
    assert!(!failed.contains("Reading the A2A log"), "{failed}");
}

// The filter field is a text context: while it is open every printable types (`q j k G e /` are
// not commands there) and `Backspace` edits; the frame, the page and the export stay untouched.
#[tokio::test]
async fn typing_into_the_filter_runs_no_command() {
    let mut rig = populated().await;
    log(&mut rig, "log").await;
    rig.paint();
    assert_eq!(rig.key(Key::Char('/')), InputAction::Consumed);
    assert!(page(&rig).search.editing);
    let content = page(&rig).content.clone();
    for c in "qjkGe/".chars() {
        assert_ne!(
            rig.key(Key::Char(c)),
            InputAction::ExportTransparency,
            "`{c}` types, it never exports"
        );
    }
    assert_eq!(page(&rig).search.query.as_str(), "qjkGe/");
    rig.key(Key::Backspace);
    assert_eq!(page(&rig).search.query.as_str(), "qjkGe");
    assert!(page(&rig).search.editing);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(page(&rig).content, content);
    // `Esc` clears the field and leaves it; the frame stays.
    rig.key(Key::Esc);
    assert!(!page(&rig).search.editing);
    assert!(page(&rig).search.query.is_empty());
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
}
