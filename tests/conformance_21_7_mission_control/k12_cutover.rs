//! K12 (final, AC25 / AC24) of slice 7-F: the RU-4 chords `Ctrl+X s/t/l/r/e/a/u/h/w` do exactly
//! what the registry says in both hosts at 60/80/119/120/130 columns, the which-key bar paints a
//! row `(n/a)` exactly when the chord names a remedy instead of running, and the generated
//! reference says the same about the monitor row. The cutover scanner clause `mission-21-7-f`
//! itself lives in `conformance_21_4_modal_stack/k6_gate.rs`; the retired config keys are pinned
//! by `tests/cli_config.rs`.
use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::keymap::render_docs::markdown_reference;
use rustain::adapters::tui::keymap::table::{
    MONITOR_NARROW, attached_mission_remedy, attached_panel_remedy,
};
use rustain::adapters::tui::keymap::{AvailCtx, Host, Key, LeaderStep, leader_step};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::which_key_bar;

const CHORDS: [(char, &str); 9] = [
    ('s', "[S] Agents"),
    ('t', "[T] Tasks"),
    ('l', "[L] A2A log"),
    ('r', "[R] Room"),
    ('e', "[E] Artifacts"),
    ('a', "[A] Adapters"),
    ('u', "[U] Usage"),
    ('h', "[H] History"),
    ('w', "[W] Monitor"),
];

fn bar(width: u16, avail: &AvailCtx) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 12)).expect("terminal");
    terminal
        .draw(|frame| {
            let _ = which_key_bar::render(frame, frame.area(), &Theme::dark(), avail, None);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..12)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the chord runs in this host at this width (the truth table of A16/A20/AC25).
fn runs(host: Host, key: char, width: u16) -> bool {
    match (host, key) {
        (Host::Local, 'w') => width >= 120,
        (Host::Local, _) => true,
        // The attached hub shows Messages only: `l` runs, `u` and the rest name a remedy.
        (Host::Attached, 'l') => true,
        (Host::Attached, _) => false,
    }
}

fn remedy(host: Host, key: char) -> &'static str {
    match (host, key) {
        (Host::Local, 'w') => MONITOR_NARROW,
        (Host::Attached, 'u') => attached_panel_remedy(),
        _ => attached_mission_remedy(),
    }
}

#[test]
fn every_ru4_chord_runs_or_names_its_remedy_and_the_which_key_bar_paints_the_same_truth() {
    for host in [Host::Local, Host::Attached] {
        for width in [60u16, 80, 119, 120, 130] {
            let avail = match host {
                Host::Local => AvailCtx::local(width),
                Host::Attached => AvailCtx::attached(width, false),
            };
            let painted = bar(width, &avail);
            for (key, label) in CHORDS {
                let at = format!("{host:?} {width} `{key}`");
                match leader_step(host, Key::Char(key), &avail, "Input") {
                    LeaderStep::Run(_) => assert!(runs(host, key, width), "{at}: ran"),
                    LeaderStep::Unavailable(_, text) => {
                        assert!(!runs(host, key, width), "{at}: refused");
                        assert_eq!(text, remedy(host, key), "{at}: the one registry remedy");
                    }
                    other => panic!("{at}: a registered chord must run or refuse: {other:?}"),
                }
                // 60 columns truncate every label to a stub: the bar's truth is read from 80 up.
                if width >= 80 {
                    assert!(
                        painted.contains(label),
                        "{at}: the row is painted:\n{painted}"
                    );
                    assert_eq!(
                        painted.contains(&format!("{label} (n/a)")),
                        !runs(host, key, width),
                        "{at}: `(n/a)` exactly when the chord names a remedy:\n{painted}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_generated_reference_states_the_monitor_row_the_registry_enforces() {
    let docs = markdown_reference();
    let row = docs
        .lines()
        .find(|line| line.contains("`Ctrl+X, W`"))
        .expect("the monitor row is in the generated reference");
    for needle in [
        "Toggle the read-only monitor (wide terminals)",
        "needs a terminal of 120+ columns",
        "not available in an attached session",
    ] {
        assert!(row.contains(needle), "`{needle}` in {row}");
    }
    for retired in ["Toggle Monitor mode", "Density", "Dashboard"] {
        assert!(!docs.contains(retired), "`{retired}` is retired: {docs}");
    }
}
