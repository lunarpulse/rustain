//! K1 (local half, AC1/AC2): `/team view <id>` opens exactly one S-THREAD frame on top,
//! at the frame geometry, titled by the item's type, with the LocalOnly rail and no
//! mutation row; an unknown id warns and opens nothing; a second view retargets.

use ratatui::layout::Rect;
use rustain::adapters::tui::presentation::{self, Surface, geometry};
use rustain::domain::models::SemanticMessageType;

use super::support::*;

const MUTATION_ROWS: [&str; 4] = [
    "[a] Acknowledge",
    "[d] Discuss",
    "[u] Use as context",
    "[r] Respond",
];

#[tokio::test]
async fn k1_view_opens_one_frame_at_frame_geometry_with_the_local_rail_and_no_mutation_row() {
    // 90 columns take the full sheet, 130 the centred box: both geometries.
    for width in [90u16, 130] {
        let mut rig = Rig::plain(consultation_workspace().await, width, 30).await;
        assert!(
            rig.f.h.state.modal_stack.is_empty(),
            "no frame before the view"
        );
        rig.view("ri_c").await;
        rig.paint();

        let state = &rig.f.h.state;
        assert_eq!(state.modal_stack.len(), 1, "exactly one frame ({width})");
        let top = state.modal_stack.top().expect("the frame");
        assert_eq!(top.surface, Surface::ThreadInspector);
        assert_eq!(top.title(), "Thread · consultation");
        assert!(presentation::consistent(state));

        let area = Rect::new(0, 0, width, 30);
        let expected = geometry::frame_rect(geometry::modal_region(&rig.f.layout(), area));
        let rect = frame_rect(&rig.f);
        assert_eq!(
            rect, expected,
            "the frame sits at the shared frame rect ({width})"
        );
        assert_eq!(rig.f.buffer()[(rect.x, rect.y)].symbol(), "┌");
        assert_eq!(
            rig.f.buffer()[(rect.right() - 1, rect.bottom() - 1)].symbol(),
            "┘"
        );
        assert!(
            title_row(&rig.f).contains("Thread · consultation"),
            "the type token is the painted title: {:?}",
            title_row(&rig.f)
        );

        // The local rail is the LocalOnly statement; no writer row is painted anywhere.
        let painted = compact(&inner_rows(&rig.f).join("\n"));
        assert!(
            painted.contains(&compact(
                "local view only — replying, acknowledgement and item context require a daemon-attached writer"
            )),
            "positive control: the LocalOnly footer is painted: {painted}"
        );
        assert!(painted.contains("Close"), "the close row is painted");
        for row in MUTATION_ROWS {
            assert!(
                !painted.contains(&compact(row)),
                "LocalOnly never paints {row:?}"
            );
        }
        // Opening wrote no base focus.
        assert_eq!(
            rig.f.h.state.focus,
            rustain::domain::models::FocusState::Chat
        );
    }
}

#[tokio::test]
async fn k1_unknown_id_warns_and_opens_nothing_and_a_known_id_is_the_positive_control() {
    let mut rig = Rig::plain(consultation_workspace().await, 130, 30).await;
    rig.view("not-a-real-item").await;
    assert!(
        rig.f.h.state.modal_stack.is_empty(),
        "no frame for an unknown id"
    );
    assert!(rig.f.h.state.thread_inspector.is_none());
    let warnings = rig.warnings();
    let (conversation, message) = warnings
        .last()
        .expect("the refusal is a Warning SystemNotice on the event bus");
    assert_eq!(conversation.as_deref(), Some("recipient-view"));
    assert!(
        !message.trim().is_empty(),
        "the notice carries the refusal text"
    );

    // Positive control: a known id through the same door opens the frame (and emits nothing).
    rig.view("ri_c").await;
    assert!(rig.is_open());
    assert!(rig.warnings().is_empty(), "a known id emits no warning");

    // An unknown id never disturbs the window that is already open.
    let before = rig.inspector().content.clone();
    rig.view("also-not-real").await;
    assert!(rig.is_open());
    assert_eq!(rig.inspector().content, before);
    assert_eq!(rig.warnings().len(), 1, "the second refusal warned once");
}

#[tokio::test]
async fn k1_a_second_view_retargets_the_one_frame() {
    let workspace = Workspace::with(&[
        Item::new("ri_c", SemanticMessageType::Consultation, "consult body"),
        Item::new("ri_a", SemanticMessageType::ArchitectureUpdate, "arch body"),
    ])
    .await;
    let mut rig = Rig::plain(workspace, 130, 30).await;
    rig.view("ri_c").await;
    rig.paint();
    assert!(rig.is_open());
    assert_eq!(
        rig.f.h.state.modal_stack.top().unwrap().title(),
        "Thread · consultation"
    );
    assert!(
        inner_rows(&rig.f)
            .join("\n")
            .contains("item actions · ri_c")
    );

    rig.view("ri_a").await;
    rig.paint();
    assert_eq!(rig.depth(), 1, "retarget, never a second frame");
    assert!(rig.is_open());
    assert_eq!(
        rig.f.h.state.modal_stack.top().unwrap().title(),
        "Thread · architecture_update"
    );
    assert!(
        title_row(&rig.f).contains("Thread · architecture_update"),
        "the painted title follows the new item"
    );
    let rows = inner_rows(&rig.f).join("\n");
    assert!(rows.contains("item actions · ri_a"));
    assert!(
        !rows.contains("item actions · ri_c"),
        "the old item is gone"
    );
    assert!(compact(&rows).contains("archbody"));

    // Retargeting back proves it is the same single frame, not a growing stack.
    rig.view("ri_c").await;
    assert_eq!(rig.depth(), 1);
    assert_eq!(
        rig.f.h.state.modal_stack.top().unwrap().title(),
        "Thread · consultation"
    );
}
