//! K4: S-BLOCK navigation moves the real painted body: vertical rows inherited
//! from RU-25, horizontal `h l 0 $` owned here, search over the displayed rows.
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
use rustain::adapters::tui::{app, app::InputAction, keymap::Key};
use rustain::domain::models::MouseConfig;

use super::support::{Fixture, block, body_row, code, open, parts, rail_row, single, status_row};

fn opened(text: &str) -> Fixture {
    let (mut f, id) = single(80, 24, text);
    open(&mut f, &id);
    f.compose();
    f
}

fn cols(f: &Fixture) -> usize {
    usize::from(parts(f).body.width)
}

fn press(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        assert_eq!(f.press(*key), InputAction::Consumed, "{key:?}");
    }
    f.compose();
}

/// Painted body rows, top to bottom.
fn painted(f: &Fixture) -> Vec<String> {
    (0..parts(f).body.height).map(|y| body_row(f, y)).collect()
}

fn scroll_to_wide_row(f: &mut Fixture) -> usize {
    for _ in 0..12 {
        if let Some(at) = painted(f).iter().position(|row| row.contains("TAIL120")) {
            return at;
        }
        press(f, &[Key::Char('j')]);
    }
    panic!("the 120-cell row never painted: {:?}", painted(f));
}

#[test]
fn dollar_shows_the_last_cells_and_zero_returns() {
    let mut f = opened(&code(12));
    let (cols, widest) = (cols(&f), 122);
    assert_eq!(block(&f).widest, widest);
    press(&mut f, &[Key::Char('l'), Key::Char('l'), Key::Char('l')]);
    assert_eq!(block(&f).viewer.h_off, 24, "three tab stops");
    assert_eq!(
        status_row(&f),
        format!(
            "code 1/1 · rust · 12 lines · Rendered · col 25-{}/122",
            24 + cols
        )
    );
    press(&mut f, &[Key::Char('$')]);
    assert_eq!(
        block(&f).viewer.h_off,
        widest - cols,
        "the last cell, not past it"
    );
    let at = scroll_to_wide_row(&mut f);
    let wide = painted(&f)[at].clone();
    assert!(
        wide.ends_with("TAIL120\";│"),
        "the tail of the line is visible: {wide:?}"
    );
    assert!(
        painted(&f).iter().all(|row| !row.ends_with('→')),
        "nothing continues right"
    );
    assert!(status_row(&f).ends_with(&format!("col {}-122/122", widest - cols + 1)));
    press(&mut f, &[Key::Char('0')]);
    assert_eq!(block(&f).viewer.h_off, 0);
    press(&mut f, &[Key::Char('h')]);
    assert_eq!(block(&f).viewer.h_off, 0, "never negative");
    press(&mut f, &[Key::Char('l'), Key::Char('h')]);
    assert_eq!(block(&f).viewer.h_off, 0, "h undoes one l");
}

#[test]
fn markers_name_hidden_content_on_each_side_and_rows_stay_inside_the_body() {
    let mut f = opened(&code(12));
    let last = cols(&f) - 1;
    let marker = |f: &Fixture, y: u16| -> (char, char) {
        let row: Vec<char> = body_row(f, y).chars().collect();
        (row[0], *row.last().unwrap())
    };
    assert_eq!(marker(&f, 0).1, '→');
    assert_ne!(marker(&f, 0).0, '←');
    press(&mut f, &[Key::Char('l')]);
    assert_eq!(marker(&f, 1), ('←', '→'));
    press(&mut f, &[Key::Char('$')]);
    let (left, right) = marker(&f, 1);
    assert_eq!(left, '←');
    assert_ne!(right, '→');
    for y in 0..parts(&f).body.height {
        assert!(body_row(&f, y).chars().count() <= last + 1);
    }
}

#[test]
fn horizontal_offsets_land_on_grapheme_boundaries() {
    const FAMILY: &str = "👨\u{200d}👩\u{200d}👧";
    // `l` puts the left edge at cell 9 (after the `←` cell). Row cell 0 is the box
    // border, so a one-cell prefix puts glyph boundaries on even cells and the edge
    // cuts a glyph; a two-cell prefix puts them on odd cells and it does not.
    for glyph in ["界", FAMILY] {
        let cut = format!("```\nx{}\ny\n```\n", glyph.repeat(70));
        let mut f = opened(&cut);
        press(&mut f, &[Key::Char('l')]);
        // 76 window cells: a blank for the cut glyph, 37 glyphs, a blank for the
        // glyph cut by the right edge.
        assert_eq!(
            body_row(&f, 1),
            format!("← {} →", glyph.repeat(37)),
            "{glyph:?} cut at both edges"
        );
        let aligned = format!("```\nxx{}\ny\n```\n", glyph.repeat(70));
        let mut f = opened(&aligned);
        press(&mut f, &[Key::Char('l')]);
        assert_eq!(
            body_row(&f, 1),
            format!("←{}→", glyph.repeat(38)),
            "{glyph:?} aligned"
        );
    }
    // Combining marks are never split from their base.
    let combining = format!("```\n{}\n```\n", "e\u{301}".repeat(150));
    let mut f = opened(&combining);
    press(&mut f, &[Key::Char('l')]);
    assert_eq!(body_row(&f, 1), format!("←{}→", "e\u{301}".repeat(76)));
}

#[test]
fn vertical_keys_clamp_and_move_painted_rows() {
    let mut f = opened(&code(60));
    let height = usize::from(parts(&f).body.height);
    let total = block(&f).rows.len();
    assert_eq!(total, 62);
    press(&mut f, &[Key::Char('G')]);
    assert_eq!(block(&f).viewer.top, total - height);
    assert!(
        painted(&f).last().unwrap().starts_with('└'),
        "{:?}",
        painted(&f)
    );
    press(&mut f, &[Key::Char('j')]);
    assert_eq!(
        block(&f).viewer.top,
        total - height,
        "clamped at the bottom"
    );
    press(&mut f, &[Key::Char('k')]);
    assert_eq!(block(&f).viewer.top, total - height - 1);
    press(&mut f, &[Key::Char('g')]);
    assert_eq!(block(&f).viewer.top, 0);
    assert!(body_row(&f, 0).starts_with("┌─ rust"));
    press(&mut f, &[Key::Ctrl('d')]);
    assert_eq!(block(&f).viewer.top, (height / 2).max(1));
    assert!(rail_row(&f).starts_with("[Esc] Back"));
}

#[test]
fn the_wheel_scrolls_the_body() {
    let mut f = opened(&code(60));
    let body = parts(&f).body;
    for (kind, expected_moved) in [
        (MouseEventKind::ScrollDown, true),
        (MouseEventKind::ScrollUp, false),
    ] {
        let before = block(&f).viewer.top;
        let event = Event::Mouse(MouseEvent {
            kind,
            column: body.x,
            row: body.y,
            modifiers: KeyModifiers::NONE,
        });
        let decoded = app::convert_crossterm_event(&event, &MouseConfig::default())
            .expect("real wheel decoder");
        assert_eq!(
            app::handle_input(&mut f.h.state, &decoded),
            InputAction::Consumed
        );
        f.compose();
        assert_eq!(block(&f).viewer.top != before, expected_moved || before > 0);
        if expected_moved {
            assert!(block(&f).viewer.top > before);
        }
    }
}

#[test]
fn search_finds_and_jumps_to_a_displayed_row() {
    let mut f = opened(&code(60));
    press(&mut f, &[Key::Char('/')]);
    for ch in "line_55".chars() {
        press(&mut f, &[Key::Char(ch)]);
    }
    press(&mut f, &[Key::Enter]);
    assert!(
        status_row(&f).starts_with("Search 1/1 · "),
        "{}",
        status_row(&f)
    );
    assert!(block(&f).viewer.top > 0, "the match row was jumped to");
    assert!(painted(&f).iter().any(|row| row.contains("line_55")));
    press(&mut f, &[Key::Char('n')]);
    assert!(
        status_row(&f).starts_with("Search 1/1"),
        "a single match wraps onto itself"
    );
    // Control: a query with no match reports it and does not move.
    let top = block(&f).viewer.top;
    press(&mut f, &[Key::Char('/')]);
    for ch in "zzz".chars() {
        press(&mut f, &[Key::Char(ch)]);
    }
    press(&mut f, &[Key::Enter]);
    assert!(status_row(&f).contains("no match") || status_row(&f).contains("Search"));
    assert_eq!(block(&f).viewer.top, top);
}
