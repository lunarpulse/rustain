//! DecisionCompose (Story 21.6 A12, RU-24, RU-29): the text field a decision frame
//! owns (Deny + feedback). Editing runs through the registry rows of
//! `Context::DecisionCompose` (readline keys, caret motion, Backspace) on the
//! shared `TextField`; it never reads or writes the composer (`state.input_buffer`).

use super::permission::FEEDBACK_MAX_BYTES;
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
    /// `Ctrl+Q`: ask to quit over the field.
    Quit,
    /// The field changed or its caret moved.
    Edited,
    /// A key the field does not use: consumed, never leaked below.
    Swallowed,
}

/// Apply `key` to `field` as the registry says (a printable key without a row types).
pub(super) fn apply(field: &mut TextField, key: Key, host: Host) -> FieldKey {
    match keymap::resolve(Context::DecisionCompose, host, &[key]) {
        Resolved::Hit(row) => match row.command {
            CommandId::Card(CardCmd::Send) => FieldKey::Send,
            CommandId::Card(CardCmd::Back) => FieldKey::Back,
            CommandId::Overlay(OverlayCmd::QuitRequest) => FieldKey::Quit,
            CommandId::Field(command) => {
                field.edit(command);
                FieldKey::Edited
            }
            _ => FieldKey::Swallowed,
        },
        Resolved::Miss => match key {
            Key::Char(c) if !c.is_control() && room_for(field, c.len_utf8()) => {
                field.insert_char(c);
                FieldKey::Edited
            }
            _ => FieldKey::Swallowed,
        },
        Resolved::Pending => FieldKey::Swallowed,
    }
}

/// Paste into the field: line breaks and other controls become spaces (the field is
/// one line), and the text is cut at the field's byte bound. True when it changed.
pub(crate) fn paste(field: &mut TextField, text: &str) -> bool {
    let mut room = FEEDBACK_MAX_BYTES.saturating_sub(field.as_str().len());
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

fn room_for(field: &TextField, extra: usize) -> bool {
    field.as_str().len() + extra <= FEEDBACK_MAX_BYTES
}
