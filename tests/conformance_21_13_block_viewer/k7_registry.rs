//! K7 (AC8): applicability: the rail never paints a key that does nothing.
use rustain::adapters::tui::{app::InputAction, keymap::Key};

use super::support::{block, code, open, rail_row, single};

#[test]
fn horizontal_rows_apply_only_when_something_is_wider_than_the_viewport() {
    let (mut f, id) = single(80, 24, "```\nshort\n```\n");
    open(&mut f, &id);
    assert!(!block(&f).wide());
    assert!(!rail_row(&f).contains("Columns"), "{}", rail_row(&f));
    for key in [
        Key::Char('l'),
        Key::Char('$'),
        Key::Char('h'),
        Key::Char('0'),
    ] {
        assert_eq!(f.press(key), InputAction::Consumed);
        assert_eq!(block(&f).viewer.h_off, 0, "{key:?} is a silent no-op");
    }
    // Positive control: a wide block paints the row and the key acts.
    let (mut f, id) = single(80, 24, &code(12));
    open(&mut f, &id);
    assert!(block(&f).wide());
    assert!(rail_row(&f).contains("Columns") || rail_row(&f).contains("[h/l]"));
    f.press(Key::Char('l'));
    assert_eq!(block(&f).viewer.h_off, 8);
}
