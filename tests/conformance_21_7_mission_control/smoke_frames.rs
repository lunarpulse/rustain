//! Desk-check frames for the native smoke file (`tests_tui/test_story_21_7_mission_control.py`,
//! legs A and B): the production host paints the hub at 60x16, 80x24 and 130x30 with a seeded
//! room journal (leg A: the Room tab), a seeded A2A log page (leg B: the Messages tab) and one
//! artifact window (leg B: `Artifact · <name>`), and the frames are written as plain text for
//! the Python predicates to be proven against offline (every predicate passes on these frames
//! and fails on torn controls).
//!
//! `#[ignore]`d: it writes files. Run it with `RUSTAIN_MISSION_FRAMES=<dir>`:
//! `cargo test --test tui conformance_21_7_mission_control::smoke_frames -- --ignored`.
use super::support::*;
use rustain::adapters::tui::artifact_window::{ArtifactBody, ArtifactWindowState};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::reducer;

/// The seeded A2A rows the Messages frame paints (the closing claim last, as `log_blocks`
/// builds it).
fn seeded_log_blocks() -> Vec<String> {
    vec![
        "2023-11-14 22:13:20Z ↓ inbound ✗ refused smoke-00".to_owned(),
        "2023-11-14 22:13:21Z ↓ inbound ✗ refused smoke-01".to_owned(),
        "— append-only, crash-safe, structurally replayable (not payload- or provenance-authenticated)"
            .to_owned(),
    ]
}

const ARTIFACT_BODY: &[u8] = b"proof artifact body line\n";

#[tokio::test]
#[ignore = "writes the desk-check frames named by RUSTAIN_MISSION_FRAMES"]
async fn story21_7_smoke_frames_export() {
    let dir = std::path::PathBuf::from(
        std::env::var("RUSTAIN_MISSION_FRAMES")
            .expect("RUSTAIN_MISSION_FRAMES names the output directory"),
    );
    std::fs::create_dir_all(&dir).expect("create the frames directory");
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        let before = lines(&rig).join("\n");
        open_room(&mut rig).await;
        let open = lines(&rig).join("\n");
        rig.f.press(Key::Esc);
        rig.f.paint();
        let after = lines(&rig).join("\n");
        std::fs::write(
            dir.join(format!("stage-before-{width}x{height}.txt")),
            before,
        )
        .expect("write");
        std::fs::write(dir.join(format!("room-open-{width}x{height}.txt")), open).expect("write");
        std::fs::write(dir.join(format!("stage-after-{width}x{height}.txt")), after)
            .expect("write");
    }
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rustain::adapters::tui::mission_control::open_messages(
            &mut rig.f.h.state,
            false,
            &seeded_log_blocks(),
            None,
        );
        rig.f.paint();
        std::fs::write(
            dir.join(format!("messages-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");

        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        let id = "a".repeat(64);
        let header = "evidence aaaaaa\n  producer  proof-node\n".to_owned();
        reducer::open_artifact_window(
            &mut rig.f.h.state,
            ArtifactWindowState::new(id, header, ArtifactBody::Source(ARTIFACT_BODY.to_vec())),
            "Artifact · evidence aaaaaa".to_owned(),
        );
        rig.f.paint();
        std::fs::write(
            dir.join(format!("artifact-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
    }
}
