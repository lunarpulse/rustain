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

/// One `#`/`.` per cell: `#` where the cell is reversed and bold (the step highlight).
fn write_mask(
    dir: &std::path::Path,
    name: &str,
    width: u16,
    height: u16,
    suffix: &str,
    f: &Fixture,
) {
    use ratatui::style::Modifier;
    let buffer = f.buffer();
    let mark = Modifier::REVERSED | Modifier::BOLD;
    let mask: Vec<String> = (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| {
                    if buffer[(x, y)].modifier.contains(mark) {
                        '#'
                    } else {
                        '.'
                    }
                })
                .collect()
        })
        .collect();
    std::fs::write(
        dir.join(format!("{name}-{width}x{height}{suffix}.rev")),
        mask.join("\n"),
    )
    .expect("write the highlight mask");
}

fn armed(width: u16, height: u16, monitor: bool, number: usize) -> Fixture {
    let mut f = plain(width, height);
    f.h.state.monitor = monitor;
    slash_tour(&mut f);
    for _ in 1..number {
        press(&mut f, Key::Char('n'));
    }
    press(&mut f, Key::Enter);
    f
}

/// Leg B (slice 14-B): the coach line and the highlight at the three viewports, the monitor
/// column on at 130x30, and the strip-yield case (a full composer at 60x16).
#[test]
#[ignore = "writes the desk-check frames named by RUSTAIN_TOUR_FRAMES"]
fn story21_14_smoke_frames_export_b() {
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
        for number in [1usize, 4, 5] {
            let f = armed(width, height, monitor, number);
            let name = format!("coach{number}");
            write(&dir, &name, width, height, suffix, &f);
            write_mask(&dir, &name, width, height, suffix, &f);
        }
    }
    let mut f = armed(60, 16, false, 4);
    let mut draft = String::new();
    while f.layout().mission_strip.is_some() {
        draft.push_str("a line of the draft\n");
        f.h.state.input_buffer = draft.clone();
    }
    f.paint();
    write(&dir, "yield4", 60, 16, "", &f);
    write_mask(&dir, "yield4", 60, 16, "", &f);
}

/// Leg C (slice 14-C): a real `/export` refusal (an absolute path) entered through the export
/// handler's public door paints an Error toast, an S-LOG row and the S-LOG Details at the three
/// viewports; the rendered help of the root, `daemon` and `team` commands is written for the
/// help predicate. The live leg runs the same `/export` against the binary.
#[tokio::test]
#[ignore = "writes the desk-check frames named by RUSTAIN_TOUR_FRAMES"]
async fn story21_14_smoke_frames_export_c() {
    use clap::CommandFactory;
    use rustain::adapters::cli::commands::Cli;
    use rustain::domain::models::SessionMeta;
    use rustain::domain::models::conversation::Conversation;
    use rustain::infrastructure::runtime::event_loop::apply_export_command;

    let dir = std::path::PathBuf::from(
        std::env::var("RUSTAIN_TOUR_FRAMES")
            .expect("RUSTAIN_TOUR_FRAMES names the output directory"),
    );
    std::fs::create_dir_all(&dir).expect("create the frames directory");
    let mut root = Cli::command();
    root.build();
    std::fs::write(
        dir.join("help-root.txt"),
        root.clone().render_long_help().to_string(),
    )
    .expect("write the root help");
    for name in ["daemon", "team"] {
        let mut sub = root
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("`{name}` is a command"))
            .clone();
        std::fs::write(
            dir.join(format!("help-{name}.txt")),
            sub.render_long_help().to_string(),
        )
        .expect("write the subcommand help");
    }
    let meta = SessionMeta {
        version: 1,
        title: "leg C".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        message_count: 0,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        extra: serde_json::Map::new(),
        plan_slug: None,
    };
    let workspace = tempfile::tempdir().expect("scratch workspace");
    let absolute = workspace.path().join("missing-dir").join("out.md");
    let absolute = absolute.to_string_lossy().into_owned();
    for (width, height, monitor) in [
        (60u16, 16u16, false),
        (80, 24, false),
        (130, 30, false),
        (130, 30, true),
    ] {
        let suffix = if monitor { "-monitor" } else { "" };
        let mut f = plain(width, height);
        f.h.state.monitor = monitor;
        apply_export_command(
            Some(&absolute),
            &Conversation::default(),
            &meta,
            workspace.path(),
            &mut f.h.state,
        )
        .await;
        f.paint();
        write(&dir, "errtoast", width, height, suffix, &f);
        press_all(&mut f, &[Key::Ctrl('x'), Key::Char('n')]);
        write(&dir, "errlog", width, height, suffix, &f);
        press(&mut f, Key::Enter);
        write(&dir, "errdetails", width, height, suffix, &f);
    }
}
