//! K5/K6 (local halves, AC7/AC8): a LocalOnly thread frame never offers a writer row.
//! No rail key is painted for `r d b a y s n u 1-3`, pressing them changes nothing (the
//! composer is not prefilled, no notice or event is emitted, the journal gains no row,
//! context is not selected), and a plain scroll key proves the same frame is live.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::domain::models::FocusState;

use super::support::*;

const WRITER_LETTERS: [char; 11] = ['r', 'd', 'b', 'a', 'y', 's', 'n', 'u', '1', '2', '3'];

fn state_of(rig: &Rig) -> String {
    let s = &rig.f.h.state;
    format!(
        "{:?} {:?} {:?} {} {:?} {:?} {}",
        (&s.input_buffer, s.cursor_position, s.input_scroll_offset),
        s.focus,
        rig.inspector().viewer,
        rig.depth(),
        rig.inspector().context_selected,
        rig.inspector().item_state,
        s.notifications.records().count(),
    )
}

#[tokio::test]
async fn k6_a_local_only_frame_paints_no_writer_row_and_the_writer_letters_change_nothing() {
    for draft in ["", "draft text"] {
        let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
        rig.f.h.state.focus = FocusState::Input;
        rig.f.h.state.input_buffer = draft.into();
        rig.f.h.state.cursor_position = draft.len();
        rig.view(NEWEST).await;
        rig.paint();
        assert!(rig.is_open());
        let rows_before = rig.ws.journal_rows().await;

        // No writer row: not a rail key, not a label.
        let rail = compact(&footer_rows(&rig).join("\n"));
        for letter in WRITER_LETTERS {
            assert!(
                !rail.contains(&format!("[{letter}]")),
                "[{letter}] is painted on a LocalOnly rail: {rail}"
            );
        }
        for label in [
            "Respond",
            "Acknowledge",
            "Discuss",
            "Busy",
            "Usecontext",
            "Accept",
            "Leaveout",
        ] {
            assert!(
                !rail.contains(label),
                "{label} is painted on a LocalOnly rail: {rail}"
            );
        }
        for positive in ["localviewonly", "[p]Earlier", "Scroll", "Close"] {
            assert!(
                rail.contains(positive),
                "positive control: {positive} in {rail}"
            );
        }
        let body = body_rows(&rig);
        let header = header_rows(&rig);

        let before = state_of(&rig);
        for letter in WRITER_LETTERS {
            let action = rig.key(Key::Char(letter));
            assert!(
                matches!(action, InputAction::Consumed | InputAction::Ignored),
                "{letter}: {action:?}"
            );
            rig.paint();
            assert_eq!(
                state_of(&rig),
                before,
                "{letter} changed state (draft {draft:?})"
            );
            assert_eq!(
                rig.f.h.state.input_buffer, draft,
                "{letter} touched the composer (no /team reply prefill)"
            );
            assert!(rig.is_open(), "{letter} closed the frame");
        }
        assert_eq!(body_rows(&rig), body, "no body row moved");
        assert_eq!(
            header_rows(&rig),
            header,
            "no identity row changed (no peer-tainted mark)"
        );
        assert!(
            rig.events.try_recv().is_err(),
            "no frame, notice or event was emitted by a local view key"
        );
        assert_eq!(
            rig.ws.journal_rows().await,
            rows_before,
            "the journal gained no row"
        );

        // Positive control: the same frame answers a plain scroll key.
        rig.key(Key::Char('j'));
        rig.paint();
        assert_ne!(state_of(&rig), before);
        assert_eq!(rig.viewer_top(), 1);
    }
}
