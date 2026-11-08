//! DecisionCompose (Story 21.6 A12, RU-24, RU-29): the text field a decision frame
//! owns (Deny + feedback, the question answer). Editing runs through the registry rows
//! of `Context::DecisionCompose` (readline keys, caret motion, Backspace) on the shared
//! `TextField`; it never reads or writes the composer (`state.input_buffer`).

use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::presentation::text_field::TextField;

/// What one key did to a field the frame owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldKey {
    /// `Enter`: submit the text through the kind's existing effect.
    Send,
    /// `Esc` / `Ctrl+C`: leave the field, the request stays undecided.
    Back,
    /// `Ctrl+D` in the question's field: skip the question.
    Skip,
    /// `Ctrl+Q`: ask to quit over the field.
    Quit,
    /// The field changed or its caret moved.
    Edited,
    /// A key the field does not use: consumed, never leaked below.
    Swallowed,
}

/// Apply `key` to `field` as the registry says (a printable key without a row types),
/// within `max` bytes. `context` is the field's own context: `DecisionCompose`, or the
/// question's (which adds `Ctrl+D` for the frame, never for the field).
pub(super) fn apply(
    field: &mut TextField,
    context: Context,
    key: Key,
    host: Host,
    max: usize,
) -> FieldKey {
    match keymap::resolve(context, host, &[key]) {
        Resolved::Hit(row) => match row.command {
            CommandId::Card(CardCmd::Send) => FieldKey::Send,
            CommandId::Card(CardCmd::Back) => FieldKey::Back,
            CommandId::Card(CardCmd::Skip) => FieldKey::Skip,
            CommandId::Overlay(OverlayCmd::QuitRequest) => FieldKey::Quit,
            CommandId::Field(command) => {
                field.edit(command);
                FieldKey::Edited
            }
            _ => FieldKey::Swallowed,
        },
        Resolved::Miss => match key {
            Key::Char(c) if !c.is_control() && room_for(field, c.len_utf8(), max) => {
                field.insert_char(c);
                FieldKey::Edited
            }
            _ => FieldKey::Swallowed,
        },
        Resolved::Pending => FieldKey::Swallowed,
    }
}

/// Paste into the field: line breaks and other controls become spaces (the field is
/// one line), and the text is cut at the field's byte bound `max`. True when it changed.
pub(crate) fn paste(field: &mut TextField, text: &str, max: usize) -> bool {
    let mut room = max.saturating_sub(field.as_str().len());
    let mut clean = String::new();
    for c in text.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if c.len_utf8() > room {
            break;
        }
        room -= c.len_utf8();
        clean.push(c);
    }
    field.insert(&clean)
}

fn room_for(field: &TextField, extra: usize, max: usize) -> bool {
    field.as_str().len() + extra <= max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_a_paste_is_one_line_and_cut_at_the_bound() {
        let mut field = TextField::default();
        assert!(paste(&mut field, "ab\ncd\u{1b}[31m", 6));
        assert_eq!(field.as_str(), "ab cd ");
        assert!(!paste(&mut field, "more", 6), "no room left");
    }

    #[test]
    fn story21_6_typing_stops_at_the_bound_and_enter_sends() {
        let mut field = TextField::default();
        let typed: Vec<FieldKey> = ['a', 'b', 'c']
            .into_iter()
            .map(|c| {
                apply(
                    &mut field,
                    Context::DecisionCompose,
                    Key::Char(c),
                    Host::Local,
                    2,
                )
            })
            .collect();
        assert_eq!(
            typed,
            [FieldKey::Edited, FieldKey::Edited, FieldKey::Swallowed]
        );
        assert_eq!(
            apply(
                &mut field,
                Context::Question,
                Key::Ctrl('d'),
                Host::Local,
                2
            ),
            FieldKey::Skip
        );
        assert_eq!(field.as_str(), "ab");
        assert_eq!(
            apply(&mut field, Context::Question, Key::Enter, Host::Local, 2),
            FieldKey::Send,
            "the question's field resolves Enter through DecisionCompose"
        );
        assert_eq!(
            apply(&mut field, Context::Question, Key::Esc, Host::Local, 2),
            FieldKey::Back
        );
        assert_eq!(
            apply(
                &mut field,
                Context::Question,
                Key::Ctrl('q'),
                Host::Local,
                2
            ),
            FieldKey::Quit
        );
    }
}
