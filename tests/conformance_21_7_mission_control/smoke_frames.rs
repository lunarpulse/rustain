//! Desk-check frames for the native smoke file (`tests_tui/test_story_21_7_mission_control.py`,
//! leg A): the production host paints the hub at 60x16, 80x24 and 130x30 with a seeded room
//! journal, and the frames are written as plain text for the Python predicates to be proven
//! against offline (every predicate passes on these frames and fails on torn controls).
//!
//! `#[ignore]`d: it writes files. Run it with `RUSTAIN_MISSION_FRAMES=<dir>`:
//! `cargo test --test tui conformance_21_7_mission_control::smoke_frames -- --ignored`.
use super::support::*;
use rustain::adapters::tui::keymap::Key;

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
}
