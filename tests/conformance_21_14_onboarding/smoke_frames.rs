//! Desk-check frames for the native smoke file (`tests_tui/test_story_21_14_onboarding.py`, leg
//! A): the production host paints the tour at 60x16, 80x24 and 130x30 (130x30 also with the
//! monitor column on), and the frames are written as plain text for the Python predicates to be
//! proven against offline (every predicate passes on these frames and fails on torn controls).
//!
//! `#[ignore]`d: it writes files. Run it with `RUSTAIN_TOUR_FRAMES=<dir>`:
//! `cargo test --test tui conformance_21_14_onboarding::smoke_frames -- --ignored`.
use super::support::*;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::tour::TourStep;

fn write(dir: &std::path::Path, name: &str, width: u16, height: u16, suffix: &str, f: &Fixture) {
    std::fs::write(
        dir.join(format!("{name}-{width}x{height}{suffix}.txt")),
        rows(f).join("\n"),
    )
    .expect("write the frame");
}

#[test]
#[ignore = "writes the desk-check frames named by RUSTAIN_TOUR_FRAMES"]
fn story21_14_smoke_frames_export() {
    let dir = std::path::PathBuf::from(
        std::env::var("RUSTAIN_TOUR_FRAMES")
            .expect("RUSTAIN_TOUR_FRAMES names the output directory"),
    );
    std::fs::create_dir_all(&dir).expect("create the frames directory");
    for (width, height, monitor) in [
        (60u16, 16u16, false),
        (80, 24, false),
        (130, 30, false),
        (130, 30, true),
    ] {
        let suffix = if monitor { "-monitor" } else { "" };
        let mut f = plain(width, height);
        f.h.state.monitor = monitor;
        f.paint();
        write(&dir, "idle", width, height, suffix, &f);

        let config = Config::new();
        launch_with_offer(&mut f, &config);
        card_opens(&mut f);
        write(&dir, "offer", width, height, suffix, &f);
        press(&mut f, Key::Enter);
        write(&dir, "step1", width, height, suffix, &f);
        for _ in 1..4 {
            press(&mut f, Key::Char('n'));
        }
        write(&dir, "step4", width, height, suffix, &f);
        press(&mut f, Key::Enter);
        write(&dir, "armed4", width, height, suffix, &f);
        press_all(&mut f, &[Key::Ctrl('x'), Key::Char('s')]);
        write(&dir, "hub", width, height, suffix, &f);
        press(&mut f, Key::Esc);
        f.clock
            .advance(rustain::adapters::tui::decision_queue::TYPING_GRACE);
        tick(&mut f);
        assert_eq!(f.h.state.tour.as_ref().unwrap().step, TourStep::Help);
        write(&dir, "step5", width, height, suffix, &f);
        press(&mut f, Key::Enter);
        press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
        write(&dir, "help", width, height, suffix, &f);
        press(&mut f, Key::Esc);
        write(&dir, "complete", width, height, suffix, &f);
    }
}
