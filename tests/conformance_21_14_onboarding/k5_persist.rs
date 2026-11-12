//! K5 — persistence and privacy of the "done" mark (AC5, D10, D11).
use super::support::*;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::infrastructure::tui_state_file::{self, start_session_at};

const SEEDED: &str = "session_count = 4\nunknown_key = \"kept\"\n";

fn table(config: &Config) -> toml::Table {
    config.text().parse().expect("the state file is valid TOML")
}

/// Finish the tour from the first-launch offer the way the person does, by `how`.
fn finish(config: &Config, how: &[Key]) -> Fixture {
    let mut f = plain(80, 24);
    launch_with_offer(&mut f, config);
    card_opens(&mut f);
    press_all(&mut f, how);
    f
}

#[test]
fn marking_done_preserves_session_count_and_unknown_keys() {
    let config = Config::new();
    std::fs::write(&config.file, SEEDED).unwrap();
    let f = finish(&config, &[Key::Esc]);
    assert!(f.h.state.tour.is_none());
    let table = table(&config);
    assert_eq!(table["session_count"].as_integer(), Some(4));
    assert_eq!(table["unknown_key"].as_str(), Some("kept"));
    assert_eq!(table["tour_done"].as_bool(), Some(true));
}

#[test]
fn leaving_from_any_card_marks_the_tour_done() {
    for (label, keys) in [
        ("offer, Esc", vec![Key::Esc]),
        ("offer, Ctrl+C", vec![Key::Ctrl('c')]),
        ("step 1, Esc", vec![Key::Enter, Key::Esc]),
        (
            "step 3, Ctrl+C",
            vec![Key::Enter, Key::Char('n'), Key::Char('n'), Key::Ctrl('c')],
        ),
        (
            "past step 5",
            vec![
                Key::Enter,
                Key::Char('n'),
                Key::Char('n'),
                Key::Char('n'),
                Key::Char('n'),
                Key::Char('n'),
            ],
        ),
    ] {
        let config = Config::new();
        let f = finish(&config, &keys);
        assert!(f.h.state.tour.is_none(), "{label}");
        assert!(config.done(), "{label}: the leave path writes tour_done");
    }
}

#[test]
fn the_file_holds_only_the_two_keys_and_is_owner_only() {
    let config = Config::new();
    let session = start_session_at(&config.file);
    assert_eq!((session.session_count, session.tour_done), (1, false));
    finish(&config, &[Key::Esc]);
    let table = table(&config);
    let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["session_count", "tour_done"]);
    // The counter keeps counting through the same module.
    let next = start_session_at(&config.file);
    assert_eq!((next.session_count, next.tour_done), (2, true));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config.file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn a_planted_link_is_never_followed() {
    use std::os::unix::fs::symlink;
    let config = Config::new();
    let victim = config.dir.path().join("victim.txt");
    std::fs::write(&victim, "precious").unwrap();
    // Every predictable sibling name a naive writer might use.
    for name in [
        "state.toml.tmp",
        "state.toml.new",
        ".state.toml.tmp",
        "state.toml~",
        "state.tmp",
        "state.toml.bak",
    ] {
        symlink(&victim, config.dir.path().join(name)).unwrap();
    }
    std::fs::write(&config.file, SEEDED).unwrap();
    finish(&config, &[Key::Esc]);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    assert!(config.done());
    // A link planted AT the state file is replaced by a regular file, not written through.
    let config = Config::new();
    let victim = config.dir.path().join("victim.txt");
    std::fs::write(&victim, "precious").unwrap();
    symlink(&victim, &config.file).unwrap();
    finish(&config, &[Key::Esc]);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    assert!(
        !std::fs::symlink_metadata(&config.file)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(config.done());
}

#[test]
fn a_garbage_file_reads_as_defaults_and_is_replaced_by_a_valid_one() {
    let config = Config::new();
    std::fs::write(&config.file, "not toml @@@").unwrap();
    assert_eq!(tui_state_file::load(&config.file).session_count, 0);
    finish(&config, &[Key::Esc]);
    assert!(config.done());
    config
        .text()
        .parse::<toml::Table>()
        .expect("valid TOML now");
}

#[test]
fn an_unwritable_location_warns_and_the_session_continues() {
    let config = Config::new();
    let mut f = plain(80, 24);
    let missing = config.dir.path().join("missing").join("state.toml");
    rustain::adapters::tui::tour::install_at_startup(&mut f.h.state, true, Ok(missing), false);
    card_opens(&mut f);
    press(&mut f, Key::Esc);
    assert!(f.h.state.tour.is_none() && f.h.state.modal_stack.is_empty());
    let warning =
        f.h.state
            .notifications
            .records()
            .find(|record| record.severity == NoticeSeverity::Warning)
            .expect("the failed save is announced");
    assert!(
        warning.message.starts_with(
            "Could not save the tour as finished. It will be offered again next time.\n"
        ),
        "{}",
        warning.message
    );
    // The session goes on: the composer still types.
    press(&mut f, Key::Char('x'));
    assert_eq!(f.h.state.input_buffer, "x");
}

/// 14-A-02: the config directory could not be resolved at startup (the production `None` path
/// used to skip the writer and its Warning): finishing still says it could not save.
#[test]
fn an_unresolved_config_directory_warns_when_the_tour_ends() {
    for key in [Key::Esc, Key::Ctrl('c')] {
        let mut f = plain(80, 24);
        rustain::adapters::tui::tour::install_at_startup(
            &mut f.h.state,
            true,
            Err("Could not determine config directory".to_owned()),
            false,
        );
        card_opens(&mut f);
        press(&mut f, key);
        assert!(f.h.state.tour.is_none() && f.h.state.modal_stack.is_empty());
        let warning =
            f.h.state
                .notifications
                .records()
                .find(|record| record.severity == NoticeSeverity::Warning)
                .expect("an unresolved config directory is announced, never silent");
        assert_eq!(
            warning.message,
            "Could not save the tour as finished. It will be offered again next time.\nCould not determine config directory"
        );
    }
    // Finishing past step 5 reports it too.
    let mut f = plain(80, 24);
    rustain::adapters::tui::tour::install_at_startup(
        &mut f.h.state,
        false,
        Err("no dir".into()),
        false,
    );
    slash_tour(&mut f);
    for _ in 0..5 {
        press(&mut f, Key::Char('n'));
    }
    assert!(
        f.h.state
            .notifications
            .records()
            .any(|record| record.severity == NoticeSeverity::Warning)
    );
}

/// 14-A-01: two processes writing the state file at once (a startup count and the tour's finish)
/// lose neither the done mark nor the other keys.
#[test]
fn concurrent_writers_never_erase_the_done_mark() {
    for round in 0..60 {
        let config = Config::new();
        std::fs::write(&config.file, "unknown_key = \"kept\"\n").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let startup = {
            let (file, barrier) = (config.file.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                start_session_at(&file)
            })
        };
        let mut f = plain(80, 24);
        launch_with_offer(&mut f, &config);
        card_opens(&mut f);
        barrier.wait();
        press(&mut f, Key::Esc);
        assert_eq!(startup.join().unwrap().session_count, 1, "round {round}");
        let table = table(&config);
        assert_eq!(
            table["tour_done"].as_bool(),
            Some(true),
            "round {round}: {table:?}"
        );
        assert_eq!(
            table["session_count"].as_integer(),
            Some(1),
            "round {round}"
        );
        assert_eq!(table["unknown_key"].as_str(), Some("kept"), "round {round}");
    }
}

#[test]
fn quitting_and_replaying_never_clear_the_mark() {
    let config = Config::new();
    finish(&config, &[Key::Esc]);
    assert!(config.done());
    let before = config.text();
    // `/tour` replays the tour; finishing it again keeps the mark.
    let mut f = plain(80, 24);
    rustain::adapters::tui::tour::install_at_startup(
        &mut f.h.state,
        false,
        Ok(config.file.clone()),
        true,
    );
    slash_tour(&mut f);
    press(&mut f, Key::Esc);
    assert!(config.done());
    assert_eq!(table(&config), before.parse::<toml::Table>().unwrap());
}
