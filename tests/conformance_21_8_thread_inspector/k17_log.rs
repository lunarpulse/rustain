//! K17 / AC20: a log visit is acknowledged only after its frame paints.
use super::support::*;
use rustain::adapters::tui::{keymap::Key, presentation::Surface};
use rustain::domain::models::SemanticMessageType;
use rustain::infrastructure::runtime::transparency_bridge;

async fn log(rig: &mut Rig, args: &str) {
    use rustain::infrastructure::{
        subagent::node_journal::WorkspaceJournalReader, transparency::TransparencyService,
    };
    if rig.app.transparency.is_none() {
        let workspace = rig.ws.path().to_path_buf();
        rig.app.transparency = Some(std::sync::Arc::new(TransparencyService::new(
            std::sync::Arc::new(WorkspaceJournalReader::open_workspace(&workspace)),
            workspace,
        )));
    }
    transparency_bridge::team_command(&mut rig.f.h.state, "origin", Some(args), &rig.app).await;
}
fn draw_without_commit(rig: &mut Rig) {
    use rustain::adapters::tui::{
        keymap::Host,
        notification_log::LogFacts,
        presentation::{self, Shell},
    };
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
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
}
async fn populated() -> Rig {
    let items: Vec<_> = (0..25)
        .map(|n| {
            Item::new(
                &format!("ri_{n:03}"),
                SemanticMessageType::Consultation,
                &format!("log marker {n:03}"),
            )
        })
        .collect();
    Rig::plain(Workspace::with(&items).await, 130, 40).await
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
        let service = rig.app.transparency.as_ref().unwrap();
        let before = render_export(&service.report().await.unwrap().rows);
        assert!(before.contains('\u{202e}') && before.contains('\u{2066}'));
        let lines =
            rustain::adapters::tui::thread_body::build(rig.inspector(), &rig.f.h.state.theme, 150)
                .lines;
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
        let painted = painted_body(&mut rig).join("");
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
    assert_eq!(rig.top(), Surface::ThreadInspector);
    let visit = rig.f.h.state.pending_log_visit.expect("unfiltered visit");
    assert!(rig.f.h.state.log_awareness.presented.is_empty());
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.pending_log_visit, Some(visit));
    assert!(rig.f.h.state.log_awareness.presented.is_empty());
    draw_without_commit(&mut rig);
    assert!(title_row(&rig.f).contains("A2A log"));
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.as_slice(), &[visit]);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 1);
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
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(
        rig.f.h.state.log_awareness.presented.as_slice(),
        &[json, newer]
    );
    log(&mut rig, "log").await;
    rig.key(Key::Esc);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 2);
    log(&mut rig, "log").await;
    rig.key(Key::Ctrl('p'));
    draw_without_commit(&mut rig);
    rig.f.h.state.log_visits_presented();
    assert_eq!(rig.f.h.state.log_awareness.presented.len(), 2);
    rig.key(Key::Esc);
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
    let all = painted_body(&mut rig).join("\n");
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
    assert!(rig.viewer_top() > 0);
    assert!(body_rows(&rig).join("\n").contains("append-only"));
    let rail = footer_rows(&rig).join("\n");
    assert!(
        !rail.contains("[c]") && !rail.contains("[v]") && !rail.contains("[x]"),
        "{rail}"
    );
    let content = rig.inspector().content.clone();
    for key in [Key::Char('c'), Key::Char('v'), Key::Enter, Key::Char('x')] {
        rig.key(key);
    }
    assert_eq!(rig.inspector().content, content);
    assert!(rig.is_open());
    rig.key(Key::Char('/'));
    for c in "ri_010".chars() {
        rig.key(Key::Char(c));
    }
    rig.key(Key::Enter);
    let filtered = painted_body(&mut rig).join("\n");
    assert!(
        filtered.contains("ri_010") && filtered.contains("append-only"),
        "{filtered}"
    );
    assert!(!filtered.contains("ri_009"));
}

#[tokio::test]
async fn local_leader_log_retains_sidebar_effect() {
    use rustain::adapters::tui::app::InputAction;
    use rustain::domain::models::visual::PanelType;
    let mut rig = populated().await;
    rig.key(Key::Ctrl('x'));
    assert!(matches!(
        rig.key(Key::Char('l')),
        InputAction::OpenPanel(PanelType::TransparencyLog)
    ));
    assert!(rig.f.h.state.modal_stack.is_empty());
}
