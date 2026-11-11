//! K8 / AC14 / A21 (21-7 slice 7-C): the Adapters window. The rows are the port and MCP health the
//! sidebar panel painted (the same builder, one snapshot taken when the window opens), in a
//! frame of their own at every width; read-only; the chord and the palette open the same
//! window; the attached host refuses with the one A20 remedy. Keys travel as crossterm events
//! through the local host's decoder; assertions read painted cells and the one frame on the
//! stack. No sleeps.
use super::support::*;
use rustain::adapters::tui::adapters_window;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::table::attached_mission_remedy;
use rustain::adapters::tui::keymap::{AvailCtx, Host, Key, LeaderStep, leader_step};
use rustain::adapters::tui::presentation::Surface;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::adapter_status_panel::{build, lines as adapter_lines};
use rustain::domain::models::{
    AdapterRef, HealthLevel, HealthSummary, McpHealthRow, PortDimension,
};
use rustain::infrastructure::runtime::agent_core::AgentCore;
use rustain::infrastructure::runtime::mission_bridge;
use std::collections::BTreeMap;

async fn rig(width: u16, height: u16) -> Rig {
    Rig::plain(Workspace::new().await, width, height).await
}

/// `Ctrl+X a` through the local front door, the effect the loop runs, a draw.
async fn open_window(rig: &mut Rig) {
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('a')), InputAction::OpenAdapters);
    mission_bridge::open_adapters(&mut rig.f.h.state, &AgentCore::test_noop());
    rig.paint();
}

fn echo_server() -> McpHealthRow {
    McpHealthRow {
        server_name: "echo".to_owned(),
        transport: "stdio".to_owned(),
        level: HealthLevel::Healthy,
        metric: "7 tools".to_owned(),
    }
}

fn healthy(port: PortDimension) -> HealthSummary {
    if port == PortDimension::Tools {
        HealthSummary::healthy("composite".to_owned())
    } else {
        HealthSummary::unknown()
    }
}

#[tokio::test]
async fn the_chord_opens_the_adapters_window_at_every_width_with_the_seven_ports() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut rig = rig(width, height).await;
        let composer = composer_rows(&rig);
        open_window(&mut rig).await;
        assert_eq!(
            top_surface(&rig),
            Some(Surface::Adapters),
            "{width}x{height}"
        );
        assert_eq!(top_title(&rig).as_deref(), Some("Adapters"));
        let all = lines(&rig).join("\n");
        assert!(all.contains("persona"), "{width}x{height}: {all}");
        assert!(
            !all.contains("120 columns"),
            "no sidebar-width floor: {all}"
        );
        assert_eq!(
            composer_rows(&rig),
            composer,
            "the window writes no composer cell"
        );
        // The way out is always painted and works.
        assert!(rail_line(&rig).contains("Back"), "{}", rail_line(&rig));
        assert_eq!(rig.key(Key::Esc), InputAction::Consumed);
        assert_eq!(top_surface(&rig), None);
    }
}

#[tokio::test]
async fn the_rows_are_the_builders_lines_for_a_fake_mcp_server_at_every_width() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut rig = rig(width, height).await;
        let overrides = BTreeMap::from([(
            PortDimension::Scheduler,
            AdapterRef {
                adapter: "cron".to_owned(),
                _config: None,
            },
        )]);
        let rows = build(
            healthy,
            vec![echo_server()],
            Some("Registry: 9 capabilities (7 MCP, 2 builtin, 0 skill)".to_owned()),
            &overrides,
        );
        adapters_window::open(&mut rig.f.h.state, rows.clone());
        rig.paint();
        let rect = frame_rect(&rig);
        let inner = rect.width - 2;
        let expected: Vec<String> = adapter_lines(&rows, inner, &Theme::dark())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let painted: Vec<String> = lines(&rig)
            [usize::from(rect.y + 1)..usize::from(rect.bottom() - 1)]
            .iter()
            .map(|row| {
                row.chars()
                    .skip(usize::from(rect.x + 1))
                    .take(usize::from(inner))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        // Every painted body row is the builder's line at the frame's inner width (the first
        // rows when the frame scrolls); the MCP row names its server, transport and tools.
        let viewport = painted.len() - 1;
        for (index, want) in expected.iter().take(viewport).enumerate() {
            assert_eq!(
                painted[index],
                want.trim_end(),
                "{width}x{height} row {index}"
            );
        }
        let all = painted.join("\n");
        assert!(
            all.contains("tools") && all.contains(": composite"),
            "{all}"
        );
        if expected.len() <= viewport {
            assert!(all.contains("└─ ") && all.contains("echo stdio"), "{all}");
            assert!(all.contains("7 tools"), "{all}");
            assert!(
                all.contains("scheduler") && all.contains(": cron [override]"),
                "{all}"
            );
        }
    }
}

#[tokio::test]
async fn a_frame_too_short_for_every_row_scrolls_by_value_and_clamps() {
    let mut rig = rig(60, 16).await;
    let rows = build(healthy, vec![echo_server()], None, &BTreeMap::new());
    adapters_window::open(&mut rig.f.h.state, rows);
    rig.paint();
    assert!(rail_line(&rig).contains("Scroll"), "{}", rail_line(&rig));
    assert!(!lines(&rig).join("\n").contains("7 ports loaded"));
    // `G` reaches the last row (the NoOp footer) and `g` the first; a further move is clamped.
    rig.key(Key::Char('G'));
    rig.paint();
    assert!(lines(&rig).join("\n").contains("7 ports loaded"));
    rig.key(Key::Char('j'));
    rig.paint();
    assert!(
        lines(&rig).join("\n").contains("7 ports loaded"),
        "clamped at the end"
    );
    rig.key(Key::Char('g'));
    rig.paint();
    assert!(lines(&rig).join("\n").contains("persona"));
}

#[tokio::test]
async fn keys_are_the_registrys_unbound_printables_never_reach_the_composer_or_close_it() {
    let mut rig = rig(80, 24).await;
    open_window(&mut rig).await;
    let composer = composer_rows(&rig);
    let draft = rig.f.h.state.input_buffer.clone();
    for key in [
        Key::Char('x'),
        Key::Char('q'),
        Key::Char('m'),
        Key::Char('c'),
    ] {
        assert_eq!(rig.key(key), InputAction::Consumed, "{key:?}");
        assert_eq!(top_surface(&rig), Some(Surface::Adapters), "{key:?}");
        assert_eq!(
            rig.f.h.state.input_buffer, draft,
            "{key:?} never reaches the composer"
        );
    }
    assert_eq!(composer_rows(&rig), composer);
    // The stack chords push over it and return exactly.
    rig.key(Key::Ctrl('x'));
    rig.key(Key::Char('?'));
    assert_eq!(top_surface(&rig), Some(Surface::Help));
    rig.key(Key::Esc);
    assert_eq!(top_surface(&rig), Some(Surface::Adapters));
    // A plain Release or an Alt variant never acts (RU-29).
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    assert_eq!(
        raw(
            &mut rig,
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release
        ),
        None
    );
    assert_eq!(
        raw(
            &mut rig,
            KeyCode::Char('x'),
            KeyModifiers::ALT,
            KeyEventKind::Press
        ),
        None
    );
    assert_eq!(top_surface(&rig), Some(Surface::Adapters));
}

#[test]
fn the_attached_host_refuses_the_adapters_chord_with_the_one_a20_remedy() {
    for width in [60, 80, 130] {
        let avail = AvailCtx::attached(width, false);
        match leader_step(Host::Attached, Key::Char('a'), &avail, "Input") {
            LeaderStep::Unavailable(_, remedy) => {
                assert_eq!(remedy, attached_mission_remedy(), "{width}");
            }
            other => panic!("attached `a` must be refused: {other:?}"),
        }
        // Control: the same chord runs in the local host at the same width.
        let local = AvailCtx::local(width);
        assert!(matches!(
            leader_step(Host::Local, Key::Char('a'), &local, "Input"),
            LeaderStep::Run(_)
        ));
    }
}

// 7C-02 (A9): adapter and MCP labels are untrusted scalars: each is one safe line before any
// span is measured or built (pre-cell rows), ordinary Unicode survives.
#[test]
fn adapter_and_mcp_labels_are_sanitized_scalars_before_any_span_exists() {
    let evil = McpHealthRow {
        server_name: "ev\nil\u{1b}]0;x\u{7}\u{202e}srv\u{200b}".to_owned(),
        transport: "std\rio".to_owned(),
        level: HealthLevel::Healthy,
        metric: "7\u{1b}[31m tools\u{2066}".to_owned(),
    };
    let ok = McpHealthRow {
        server_name: "café-日本".to_owned(),
        transport: "stdio".to_owned(),
        level: HealthLevel::Healthy,
        metric: "3 tools".to_owned(),
    };
    let rows = build(
        healthy,
        vec![evil, ok],
        Some("summary\u{1b}[0m\nTokens".to_owned()),
        &BTreeMap::new(),
    );
    let painted: Vec<String> = adapter_lines(&rows, 80, &Theme::dark())
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let joined = painted.join("\n");
    assert!(
        !joined.chars().any(|c| {
            (c.is_control() && c != '\n')
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)
        }),
        "{joined:?}"
    );
    assert!(painted.iter().all(|row| !row.contains('\n')), "{painted:?}");
    assert!(
        joined.contains("café-日本"),
        "ordinary Unicode survives: {joined}"
    );
    assert!(joined.contains("ev il"), "{joined}");
}
