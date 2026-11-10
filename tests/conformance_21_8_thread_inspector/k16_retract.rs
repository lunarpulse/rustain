//! K16 / AC19: retract is a writer-only effect, never inferred from a badge.
use super::support::*;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::{app::InputAction, keymap::Key, state::ThreadAccess};
use rustain::infrastructure::runtime::{thread_open, transparency_bridge};

fn sent(rig: &mut Rig, access: ThreadAccess, body: Option<&str>) {
    let generation = thread_open::open_sent(&mut rig.f.h.state, access, "ada", "opened", "origin");
    if let Some(body) = body {
        assert!(thread_open::fill_sent(
            &mut rig.f.h.state,
            "origin",
            generation,
            body
        ));
    }
}
fn rail(rig: &mut Rig) -> String {
    rig.paint();
    footer_rows(rig).join("\n")
}

#[tokio::test]
async fn sent_retract_requires_owned_unretracted_completed_writer_read() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 40).await;
    let mine = "thread · ada · opened\n  you       opened\n      body";
    sent(&mut rig, ThreadAccess::Writer, Some(mine));
    assert!(rail(&mut rig).contains("[x] Retract"));
    let before = rig.inspector().content.clone();
    let blocks = rig.f.h.state.feedback_blocks.len();
    assert!(
        matches!(rig.key(Key::Char('x')), InputAction::ThreadRetract { peer, item } if peer == "ada" && item == "opened")
    );
    assert_eq!(rig.inspector().content, before);
    assert_eq!(rig.f.h.state.feedback_blocks.len(), blocks);
    for event in [
        raw(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ),
        raw(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Repeat),
        raw(KeyCode::Char('x'), KeyModifiers::ALT, KeyEventKind::Press),
        raw(KeyCode::Char('x'), KeyModifiers::SHIFT, KeyEventKind::Press),
        raw(KeyCode::Char('X'), KeyModifiers::SHIFT, KeyEventKind::Press),
    ] {
        assert!(!matches!(
            rig.send(event),
            InputAction::ThreadRetract { .. }
        ));
        assert_eq!(rig.inspector().content, before);
    }
    for (access, body) in [
        (ThreadAccess::Writer, None),
        (
            ThreadAccess::Writer,
            Some("thread · ada · opened\n  ada       opened\n      body"),
        ),
        (
            ThreadAccess::Writer,
            Some("thread · ada · opened\n  you       opened [retracted 12:34]\n      body"),
        ),
        (ThreadAccess::ReadOnly, Some(mine)),
        (ThreadAccess::LocalOnly, Some(mine)),
    ] {
        sent(&mut rig, access, body);
        assert!(!rail(&mut rig).contains("[x] Retract"));
        assert!(!matches!(
            rig.key(Key::Char('x')),
            InputAction::ThreadRetract { .. }
        ));
    }
    let generation = thread_open::open_sent(
        &mut rig.f.h.state,
        ThreadAccess::Writer,
        "bea",
        "other-item",
        "origin",
    );
    assert!(thread_open::fill_sent(
        &mut rig.f.h.state,
        "origin",
        generation,
        "thread · bea · other-item\n  you       other-item\n      another body",
    ));
    assert!(rail(&mut rig).contains("[x] Retract"));
    assert!(
        matches!(
            rig.key(Key::Char('x')),
            InputAction::ThreadRetract { peer, item } if peer == "bea" && item == "other-item"
        ),
        "the production key route carries the retargeted peer and item"
    );
}

#[tokio::test]
async fn local_read_only_fences_sent_retract_rail_key_and_effect() {
    use rustain::adapters::tui::notifications::NoticeSeverity;

    let mut rig = Rig::plain(Workspace::new().await, 130, 40).await;
    sent(
        &mut rig,
        ThreadAccess::Writer,
        Some("  you  opened\n      body"),
    );
    rig.f.tabs.active_tab_mut().read_only = false;
    assert!(rail(&mut rig).contains("[x] Retract"));
    assert!(matches!(
        rig.key(Key::Char('x')),
        InputAction::ThreadRetract { peer, item } if peer == "ada" && item == "opened"
    ));
    rig.f.tabs.active_tab_mut().read_only = true;
    assert!(!rail(&mut rig).contains("[x] Retract"));
    assert!(!matches!(
        rig.key(Key::Char('x')),
        InputAction::ThreadRetract { .. }
    ));
    let before = rig.ws.journal_rows().await;
    transparency_bridge::retract_sent(
        &mut rig.f.h.state,
        "origin",
        "ada",
        "opened",
        true,
        &rig.app,
    )
    .await;
    let notice = rig
        .f
        .h
        .state
        .notifications
        .visible()
        .last()
        .expect("read-only warning");
    assert_eq!(notice.severity, NoticeSeverity::Warning);
    assert_eq!(notice.message, "Tab is read-only (subagent output view)");
    assert_eq!(rig.f.h.state.decisions.items().count(), 0);
    assert!(!rig.f.h.state.feedback_blocks.contains_key("team-retract"));
    assert_eq!(rig.ws.journal_rows().await, before);
}

#[cfg(not(feature = "a2a"))]
#[tokio::test]
async fn local_sent_retract_key_runs_real_unavailable_effect_without_mutation() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 40).await;
    sent(
        &mut rig,
        ThreadAccess::Writer,
        Some("  you  opened\n      body"),
    );
    assert!(rail(&mut rig).contains("[x] Retract"));
    let before = rig.ws.journal_rows().await;
    let InputAction::ThreadRetract { peer, item } = rig.key(Key::Char('x')) else {
        panic!("plain x must request a sent-item retract");
    };
    assert_eq!((&*peer, &*item), ("ada", "opened"));
    transparency_bridge::retract_sent(&mut rig.f.h.state, "origin", &peer, &item, false, &rig.app)
        .await;
    let message = &rig.f.h.state.feedback_blocks["team-retract"].message;
    assert!(message.contains("needs the `a2a` feature"), "{message}");
    assert!(message.contains("Nothing was marked"), "{message}");
    assert_eq!(rig.f.h.state.decisions.items().count(), 0);
    assert!(rig.events.try_recv().is_err());
    assert_eq!(rig.ws.journal_rows().await, before);
}

#[tokio::test]
async fn badge_without_a_retractable_row_offers_no_retract() {
    let ws = Workspace::with(&[Item::new(
        "ri_c",
        rustain::domain::models::SemanticMessageType::Consultation,
        "[auto-sent] not authority",
    )])
    .await;
    let mut rig = Rig::plain(ws, 130, 40).await;
    rig.view("ri_c").await;
    assert!(painted_body(&mut rig).join("\n").contains("[auto-sent]"));
    assert!(!rail(&mut rig).contains("[x] Retract"));
    assert!(!matches!(
        rig.key(Key::Char('x')),
        InputAction::ThreadRetract { .. }
    ));
    // A badge on a SENT thread that is not this host's own item is no fact either.
    sent(
        &mut rig,
        ThreadAccess::Writer,
        Some(
            "thread · ada · opened\n  another sender  opened  [auto-sent]\n      [auto-sent] body",
        ),
    );
    assert!(rig.inspector().content.contains("[auto-sent]"));
    assert!(!rail(&mut rig).contains("[x] Retract"));
    assert!(!matches!(
        rig.key(Key::Char('x')),
        InputAction::ThreadRetract { .. }
    ));
    sent(
        &mut rig,
        ThreadAccess::Writer,
        Some("thread · ada · opened\n  you       opened\n      body"),
    );
    assert!(rail(&mut rig).contains("[x] Retract"));
}
