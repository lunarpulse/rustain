//! K3 / M3: hostile tool bytes stop at the display boundary, not at an enum.
use super::support::*;
use ratatui::{Terminal, backend::TestBackend, layout::Rect, widgets::Paragraph};
use rustain::adapters::tui::{
    keymap::Key,
    markdown::{self, RenderOptions},
};
use rustain::domain::models::{InvocationStatus as S, Turn, WriteDiffState as D};
use serde_json::json;

fn assert_no_terminal_controls(f: &Fixture) {
    for cell in f.buffer().content() {
        for c in cell.symbol().chars() {
            assert!(
                !c.is_control()
                    && !matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'\u{061c}'|'\u{200e}'|'\u{200f}'),
                "forbidden terminal/display character {c:?} reached an actual cell {:?}",
                cell.symbol()
            );
        }
    }
}

fn literal_cells(f: &Fixture, expected: &str) {
    let body = f.body_rect();
    let lines = markdown::render(
        expected,
        usize::from(body.width),
        &f.h.state.theme,
        &RenderOptions::literal(),
    )
    .lines;
    let mut terminal = Terminal::new(TestBackend::new(body.width, body.height)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new(lines.clone()),
                Rect::new(0, 0, body.width, body.height),
            )
        })
        .unwrap();
    for y in 0..body.height {
        for x in 0..body.width {
            assert_eq!(
                f.buffer()[(body.x + x, body.y + y)].symbol(),
                terminal.backend().buffer()[(x, y)].symbol(),
                "shared Literal cell width/CRLF/tab regression at ({x},{y})"
            );
        }
    }
}

#[test]
fn no_control_or_bidi_character_reaches_a_cell() {
    let hostile = "\x1b[31mred\x1b[0m\tx\u{202e}evil\x07\r\n\x1b]0;forged-title\x07\x1b]8;;https://invalid.example\x07link\x1b]8;;\x07\n\u{009b}31mC1\u{009b}0m\x00\x08\u{2066}safe\u{2069}";
    let clean = "red\txevil\r\nlink\nC1safe";
    let (mut f, _) = Fixture::single(
        80,
        24,
        "Edit",
        json!({"old_string":hostile,"new_string":"replacement\x1b[31m\u{202e}"}),
        S::Success,
        Some((hostile, D::NotAWrite)),
    );
    f.open();
    assert_no_terminal_controls(&f);
    assert!(f.body_row(0).contains("red"));
    assert!(f.body_row(0).contains("xevil"));
    assert!(
        !f.body_row(0).contains("redxevil"),
        "the tab must expand, not disappear"
    );
    literal_cells(&f, clean);
    let body = (0..f.body_rect().height)
        .map(|n| f.body_row(n))
        .collect::<Vec<_>>()
        .join("\n");
    for payload in ["31m", "forged-title", "]8;;", "https://invalid.example"] {
        assert!(
            !body.contains(payload),
            "dropping a zero-width ESC cell must not leave its control payload: {body:?}"
        );
    }
    for _ in 0..3 {
        f.press(Key::Right);
        f.paint();
        assert_no_terminal_controls(&f);
    }
    assert!(
        f.body_row(0).contains("- red"),
        "Diff also passes the display boundary"
    );
}

#[test]
fn literal_plain_unicode_crlf_tabs_and_combining_clusters_keep_their_cells() {
    let wide = "界".repeat(39);
    for text in [
        "plain **not bold** [not a link](https://invalid.example)\nsecond line",
        "漢字\t🙂 e\u{301} 👩\u{200d}💻\r\nCRLF second\r\nthird\tend",
        wide.as_str(),
    ] {
        let (mut f, _) = Fixture::single(
            80,
            24,
            "Bash",
            json!({"command":"unicode"}),
            S::Success,
            Some((text, D::NotAWrite)),
        );
        f.open();
        assert_no_terminal_controls(&f);
        literal_cells(&f, text);
        assert!(
            f.buffer().content().iter().any(|cell| cell.symbol() != " "),
            "positive paint control"
        );
    }
}

#[test]
fn two_running_calls_of_the_same_tool_show_only_their_own_tail() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"first"}),
        S::Running,
        None,
    );
    let first_part = part_for(&turn, &first);
    prose(&mut turn, &rows(24));
    let second = call(
        &mut turn,
        "Bash",
        json!({"command":"second"}),
        S::Running,
        None,
    );
    let second_part = part_for(&turn, &second);
    prose(&mut turn, &rows(24));
    let unknown = call(
        &mut turn,
        "Bash",
        json!({"command":"unknown wire id"}),
        S::Running,
        None,
    );
    let mut f = Fixture::new(80, 24, Turn::new("unused".into(), 0));
    f.tabs.active_tab_mut().conversation.turns.clear();
    let reducer = &mut f.tabs.active_tab_mut().reducer;
    reducer.open_turn = Some(turn);
    reducer
        .pending_invocations
        .insert("wire-A".into(), first_part);
    reducer
        .pending_invocations
        .insert("wire-B".into(), second_part);
    reducer.set_tail("wire-A", "A1-identity-tail".into());
    reducer.set_tail("wire-B", "B1-identity-tail".into());
    for (id, own, other) in [
        (&first, "A1-identity-tail", "B1-identity-tail"),
        (&second, "B1-identity-tail", "A1-identity-tail"),
    ] {
        f.focus_call(id);
        f.open();
        let body = (0..f.body_rect().height)
            .map(|n| f.body_row(n))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains(own), "{body}");
        assert!(
            !body.contains(other),
            "same tool name is not an identity: {body}"
        );
        assert!(body.contains("Live tail"));
        f.press(Key::Esc);
        f.paint();
    }
    f.focus_call(&unknown);
    f.open();
    assert!(f.body_row(0).contains("No output yet — running ("));
    let body = (0..f.body_rect().height)
        .map(|n| f.body_row(n))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.contains("A1-identity-tail") && !body.contains("B1-identity-tail"));
}
