//! Attached-host key tests moved out of `attach_keys.rs` so that file never grows net
//! (A-15 line ratchet). They drive the same `Rig` the other attached key tests use.

use super::attach_loop::attach_keys::KeyFlow;
use super::attach_loop::attach_keys::tests::Rig;
use crate::adapters::tui::notifications::NoticeSeverity;

/// With an empty attached queue the chord answers without sending a frame.
#[tokio::test]
async fn story21_1_attached_ctrl_x_q_answers_that_no_decision_waits_in_this_client() {
    let mut rig = Rig::new(false);
    assert_eq!(rig.ctrl('x').await, KeyFlow::Continue);
    assert_eq!(rig.ch('q').await, KeyFlow::Continue);
    let notice = rig.state.notifications.visible().next().expect("a toast");
    assert_eq!(notice.message, "No decisions waiting");
    assert_eq!(notice.severity, NoticeSeverity::Info);
    assert!(rig.state.decisions.is_empty());
    assert!(rig.frames().is_empty(), "never sent to the daemon");
}

/// Visit's opener and reader commands are local-only. Exercise the real attached
/// Messages frame without constructing a Visit payload or acquiring local custody.
#[cfg(feature = "session-visit")]
#[tokio::test]
async fn session_visit_attached_has_no_local_bindings_or_open_effect() {
    use super::attach_loop::attach_keys::{route_event, tests::salient};
    use crate::adapters::tui::keymap::resolve::{Resolved, resolve, rows_of};
    use crate::adapters::tui::keymap::table_tool::ToolCmd;
    use crate::adapters::tui::keymap::{CommandId, Context, Host, Key};
    use crate::adapters::tui::presentation::{self, RouteEffect, Surface};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    assert!(
        matches!(
            resolve(Context::ToolInspector, Host::Local, &[Key::Char('o')]),
            Resolved::Hit(row) if row.command == CommandId::Tool(ToolCmd::VisitSource)
        ),
        "positive control: the local source key really opens Visit"
    );
    assert!(
        matches!(
            resolve(Context::ToolInspector, Host::Attached, &[Key::Char('o')]),
            Resolved::Miss
        ),
        "attached cannot construct the local Visit source action"
    );
    assert!(rows_of(Context::SessionVisit, Host::Local).next().is_some());
    assert!(
        rows_of(Context::SessionVisit, Host::Attached)
            .next()
            .is_none(),
        "attached has no Visit reader commands"
    );

    for read_only in [false, true] {
        let mut rig = Rig::new(read_only);
        rig.type_text("retained draft").await;
        rig.ctrl('x').await;
        rig.ch('l').await;
        assert!(presentation::is_open(&rig.state, Surface::MissionControl));
        let before = salient(&rig);
        let frames = rig.state.modal_stack.iter().cloned().collect::<Vec<_>>();
        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE);

        assert_eq!(
            route_event(&mut rig.state, &Event::Key(key)),
            Some(RouteEffect::Consumed),
            "the attached source key has no host action"
        );
        assert_eq!(rig.raw_key(key).await, KeyFlow::Continue);
        assert_eq!(salient(&rig), before, "no composer or reader side effect");
        assert_eq!(
            rig.state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames,
            "the real attached frame remains the owner"
        );
        assert!(rig.state.session_visit.is_none());
        assert!(!presentation::is_open(&rig.state, Surface::SessionVisit));
        assert!(
            rig.frames().is_empty(),
            "no Visit request or other frame is sent to the daemon"
        );
    }
}
