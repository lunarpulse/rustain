//! Story C-12 AC4 (FR68, NFR60) attached client: `Ctrl+X a` through the real attached key front
//! door opens the Adapters window on the channel link state the attach event stream delivered
//! (never a local core), the window repaints live, and a later update replaces a stale value.
use super::*;
use crate::adapters::tui::presentation::Surface;
use crate::adapters::tui::widgets::adapter_status_panel::{
    ATTACHED_NO_LINK_NOTE, ATTACHED_SCOPE_NOTE,
};
use crate::domain::models::ChannelLinkState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One `ChannelLinkChanged` event through the attach client's event application.
fn deliver(rig: &mut attach_keys::tests::Rig, state: ChannelLinkState) {
    let clock = SystemClock::default();
    let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
    let mut mode = PermissionMode::Normal;
    let raw = RawEvent {
        conversation_id: None,
        timestamp_ms: 0,
        kind: RawEventKind::ChannelLinkChanged {
            adapter: "slack".into(),
            state,
        },
    };
    let redraw = apply_client_event(
        raw,
        &mut rig.domain_tab.conversation,
        &mut rig.domain_tab.streaming,
        &mut reducer,
        &mut rig.state,
        &mut mode,
        &clock,
    );
    assert!(redraw, "a link update repaints");
}

async fn press(rig: &mut attach_keys::tests::Rig, code: KeyCode, modifiers: KeyModifiers) {
    let flow = rig.raw_key(KeyEvent::new(code, modifiers)).await;
    assert_eq!(flow, attach_keys::KeyFlow::Continue, "{code:?}");
}

async fn open_adapters(rig: &mut attach_keys::tests::Rig) {
    press(rig, KeyCode::Char('x'), KeyModifiers::CONTROL).await;
    press(rig, KeyCode::Char('a'), KeyModifiers::NONE).await;
    assert_eq!(
        rig.state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::Adapters),
        "attached `Ctrl+X a` opens the Adapters window"
    );
}

/// The painted screen, one line per terminal row.
fn screen(rig: &mut attach_keys::tests::Rig) -> String {
    let terminal = rig.paint();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn channels_row(screen: &str) -> &str {
    screen
        .lines()
        .find(|line| line.contains("channels") && line.contains("slack"))
        .unwrap_or_else(|| panic!("a channels row for slack:\n{screen}"))
}

#[tokio::test]
async fn attached_adapters_key_shows_the_daemon_link_and_replaces_a_stale_value() {
    for read_only in [false, true] {
        let mut rig = attach_keys::tests::Rig::new(read_only);

        // Nothing published (terminal or Telegram daemon): the honest note, no channels row.
        open_adapters(&mut rig).await;
        let empty = screen(&mut rig);
        assert!(empty.contains(ATTACHED_NO_LINK_NOTE), "{empty}");
        assert!(empty.contains(ATTACHED_SCOPE_NOTE), "{empty}");
        assert!(!empty.contains("slack"), "{empty}");
        // State-only: Esc goes back, nothing is sent.
        press(&mut rig, KeyCode::Esc, KeyModifiers::NONE).await;
        assert!(
            rig.state.modal_stack.top().is_none(),
            "Esc closes the window"
        );

        // The daemon says connecting: the row says exactly that, never a hopeful `connected`.
        deliver(&mut rig, ChannelLinkState::Connecting);
        open_adapters(&mut rig).await;
        let starting = screen(&mut rig);
        let row = channels_row(&starting);
        assert!(row.contains("connecting"), "{row}");
        assert!(
            !row.contains("connected"),
            "starting is not connected: {row}"
        );
        assert!(!starting.contains(ATTACHED_NO_LINK_NOTE), "{starting}");

        // A live update repaints the open window.
        deliver(&mut rig, ChannelLinkState::Connected);
        let connected = screen(&mut rig);
        assert!(
            channels_row(&connected).contains("connected"),
            "{connected}"
        );

        // Loss, then the retry budget spent: the stale `connected` is replaced.
        deliver(&mut rig, ChannelLinkState::Reconnecting);
        let reconnecting = screen(&mut rig);
        assert!(
            channels_row(&reconnecting).contains("reconnecting"),
            "{reconnecting}"
        );
        deliver(&mut rig, ChannelLinkState::Offline);
        let offline = screen(&mut rig);
        let row = channels_row(&offline);
        assert!(row.contains("offline"), "{row}");
        assert!(!row.contains("connected"), "no stale state: {row}");
        assert!(
            offline.contains("restart the daemon to reconnect"),
            "{offline}"
        );

        // Reopening reads the latest daemon value, not the first one.
        press(&mut rig, KeyCode::Esc, KeyModifiers::NONE).await;
        open_adapters(&mut rig).await;
        let reopened = screen(&mut rig);
        assert!(channels_row(&reopened).contains("offline"), "{reopened}");
        assert_eq!(
            rig.state.attached_channel_link,
            Some(("slack".to_owned(), ChannelLinkState::Offline))
        );
        assert!(rig.frames().is_empty(), "the window never sends a frame");
    }
}
