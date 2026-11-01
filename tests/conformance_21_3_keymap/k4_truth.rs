//! K4 (decoder leg, AC12): a Ctrl/Alt-modified character never becomes a bare
//! `KeyPress`, so it can never type its letter or fire the letter's command
//! (Ctrl+J scrolling Chat, Ctrl+Y approving a plan, Alt+C typing `c`).

use rustain::adapters::tui::keymap::Key;
use rustain::domain::events::{DomainInputEvent, DomainKey};

use super::support::{decode, fresh, is_inert, press};

#[test]
fn ctrl_and_alt_letters_never_type_or_command() {
    for c in 'a'..='z' {
        for key in [Key::Ctrl(c), Key::Alt(c)] {
            let event = decode(key).unwrap_or_else(|| panic!("{key:?} must decode to something"));
            assert!(
                !matches!(event, DomainInputEvent::KeyPress(_)),
                "{key:?} folded into the bare letter: {event:?}"
            );
        }
    }
    // The named leaks of PC-4: Ctrl+J (raw 0x0a), Ctrl+Y, Ctrl+A, Ctrl+W, Alt+C/L/U.
    assert!(matches!(
        decode(Key::Ctrl('j')),
        Some(DomainInputEvent::SpecialKey(DomainKey::UnboundCtrl('j')))
    ));
    assert!(matches!(
        decode(Key::Ctrl('y')),
        Some(DomainInputEvent::SpecialKey(DomainKey::UnboundCtrl('y')))
    ));
    assert!(matches!(
        decode(Key::Ctrl('a')),
        Some(DomainInputEvent::SpecialKey(DomainKey::CtrlA))
    ));
    assert!(matches!(
        decode(Key::Ctrl('w')),
        Some(DomainInputEvent::SpecialKey(DomainKey::CtrlW))
    ));
    // Positive control: a plain letter and a shifted letter still decode as characters.
    assert!(matches!(
        decode(Key::Char('j')),
        Some(DomainInputEvent::KeyPress('j'))
    ));
    assert!(matches!(
        decode(Key::Char('J')),
        Some(DomainInputEvent::KeyPress('J'))
    ));
}

/// The policy for combined modifiers (AC20b): a character held with more than one
/// modifier is its own key (bits 1 Ctrl, 2 Alt, 4 Shift), never the plain Ctrl or
/// Alt key of the same letter: `Ctrl+Shift+C` is not `Ctrl+C`.
#[test]
fn combined_modifier_characters_decode_to_their_own_key() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers as M};
    use rustain::adapters::tui::app::convert_crossterm_event;
    use rustain::domain::models::MouseConfig;
    let decode_mods = |c: char, mods: M| {
        convert_crossterm_event(
            &Event::Key(KeyEvent::new(KeyCode::Char(c), mods)),
            &MouseConfig::default(),
        )
    };
    for c in ['c', 'x', 'q', 'j', 'y', 'm', 'C', 'X'] {
        let lower = c.to_ascii_lowercase();
        for (mods, bits) in [
            (M::CONTROL | M::ALT, 3),
            (M::CONTROL | M::SHIFT, 5),
            (M::CONTROL | M::ALT | M::SHIFT, 7),
            (M::ALT | M::SHIFT, 6),
        ] {
            assert!(
                matches!(
                    decode_mods(c, mods),
                    Some(DomainInputEvent::SpecialKey(DomainKey::UnboundCombo(k, b)))
                        if k == lower && b == bits
                ),
                "{mods:?}+{c}"
            );
        }
    }
    // Positive controls: the same keys alone keep their own meaning.
    assert!(matches!(
        decode_mods('c', M::CONTROL),
        Some(DomainInputEvent::SpecialKey(DomainKey::CtrlC))
    ));
    assert!(matches!(
        decode_mods('C', M::SHIFT),
        Some(DomainInputEvent::KeyPress('C'))
    ));
}

#[test]
fn alt_c_l_u_outside_the_login_overlay_are_inert() {
    for c in ['c', 'l', 'u'] {
        let mut state = fresh();
        state.input_buffer = "x".into();
        state.cursor_position = 1;
        let action = press(&mut state, Key::Alt(c));
        assert!(is_inert(&action), "Alt+{c}: {action:?}");
        assert_eq!(state.input_buffer, "x", "Alt+{c} must not type `{c}`");
    }
}
