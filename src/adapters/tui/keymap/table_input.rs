//! Input rows (Story 21.3 AC5, KG-2): the composer, `FocusState::Input` with no
//! popup, reverse search, palette or leader open.
//!
//! Slice 3-A describes TODAY's hand-written arms of `app::handle_char`,
//! `app::handle_special_key` and `attach_keys::handle_attached_key` exactly; the
//! registry does not dispatch them yet. Printable characters are typing (no row,
//! the K4 sweep asserts each inserts exactly itself) except the two documented
//! popup triggers `/` and `@`. `Esc` and `Enter` are the `TextEntry` keys a text
//! field inherits (A4); every other row is declared here. `Ctrl+X` and `Ctrl+Q`
//! come from `Global`. Unbound keys (`Ctrl+A/W/K/U/H`, `Ctrl/Alt` letters) have no
//! row: they are inert (AC12).

use super::model::{AvailCtx, Availability, Binding, CommandId, Context, HelpCat, Key, bind};

/// Commands of the Input context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputCmd {
    /// `Esc`: leave the composer for Chat; unsent text is kept, never sends.
    ReturnToChat,
    /// `Enter`: send the composer, or insert a newline in multi-line mode.
    SubmitOrNewline,
    /// `Ctrl+Enter`: send the composer (an empty composer does nothing).
    Send,
    /// `Shift+Enter` / `Alt+Enter`: insert a newline at the cursor.
    InsertNewline,
    /// `Alt+M`: toggle multi-line mode (`/ml` is the typed route).
    ToggleMultiline,
    /// `Alt+V`: paste an image (or text) from the system clipboard. Local only.
    PasteClipboard,
    /// `Ctrl+P`: open the command palette.
    OpenPalette,
    /// `Ctrl+R`: open reverse search over the input history. Local only.
    ReverseSearch,
    /// `Ctrl+F`: search the conversation (needs an empty composer). Local only.
    OpenSearch,
    /// `Ctrl+T`: open a new tab. Local only.
    NewTab,
    /// `Ctrl+C`: stop active work; when idle, ask to quit.
    CancelOrQuit,
    /// `Ctrl+E`: move to the end of the current logical line.
    LineEnd,
    /// `Backspace`: delete the grapheme before the cursor.
    DeleteBack,
    /// `Delete`: delete the grapheme at the cursor. Local only.
    DeleteForward,
    /// `Left`: move the cursor one grapheme left. Local only.
    CursorLeft,
    /// `Right`: move the cursor one grapheme right. Local only.
    CursorRight,
    /// `Home`: move to the start of the current visual row. Local only.
    CursorHome,
    /// `End`: move to the end of the current visual row. Local only.
    CursorEnd,
    /// `Up`: previous visual row, else the previous history entry. Local only.
    UpOrHistoryPrev,
    /// `Down`: next visual row, else the next history entry. Local only.
    DownOrHistoryNext,
    /// `Shift+Tab`: cycle the permission mode. Local only.
    CycleMode,
    /// `/` on an empty composer: type it and open the slash-command popup.
    OpenSlashPopup,
    /// `@`: type it and open the file/agent mention popup. Local only.
    OpenMentionPopup,
    /// `PageUp`: scroll the attached transcript one page up. Attached only.
    TranscriptPageUp,
    /// `PageDown`: scroll the attached transcript one page down. Attached only.
    TranscriptPageDown,
    /// `Ctrl+A`: move to the start of the current logical line. Local only.
    LineStart,
    /// `Ctrl+W`: delete the word before the cursor.
    DeleteWordBack,
    /// `Ctrl+U`: delete from the start of the logical line to the cursor.
    KillToLineStart,
    /// `Ctrl+K`: delete from the cursor to the end of the logical line. Local only.
    KillToLineEnd,
    /// `Ctrl+A/E/K` in the attached composer: it has no cursor, so nothing moves.
    NoCursor,
}

/// Commands of the shared append-only text fields (A4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldCmd {
    /// `Ctrl+W`: delete the last word of the field.
    DeleteWordBack,
    /// `Ctrl+U`: clear the field.
    Clear,
    /// `Ctrl+H`: delete the last character of the field (legacy Backspace).
    DeleteBack,
    /// `Ctrl+A/E/K`: no cursor, so no effect.
    NoCursor,
}

fn no_cursor(_: &AvailCtx) -> Availability {
    Availability::Unavailable("no cursor in this field")
}

const fn i(keys: &'static [Key], cmd: InputCmd, label: &'static str) -> Binding {
    bind(Context::Input, keys, CommandId::Input(cmd), label)
}

const fn f(keys: &'static [Key], cmd: FieldCmd) -> Binding {
    bind(Context::FieldEdit, keys, CommandId::Field(cmd), "")
}

const IN: HelpCat = HelpCat::Input;
const CMD: HelpCat = HelpCat::Commands;

#[rustfmt::skip]
const INPUT: &[Binding] = &[
    i(&[Key::Enter], InputCmd::SubmitOrNewline, "Send").te().help(IN, "Enter", "Send message"),
    i(&[Key::ShiftEnter], InputCmd::InsertNewline, "Newline").alt("Enter in multi-line mode (/ml)").help(IN, "Shift+Enter", "New line (multi-line mode)"),
    i(&[Key::AltEnter], InputCmd::InsertNewline, "Newline").alt("Enter in multi-line mode (/ml)").hint().help(IN, "Alt+Enter", "Insert newline (alternative when Shift+Enter is intercepted)"),
    i(&[Key::Ctrl('e')], InputCmd::LineEnd, "").local().help(IN, "Ctrl+E", "Move to the end of the current line"),
    i(&[Key::Ctrl('a')], InputCmd::LineStart, "").local().help(IN, "Ctrl+A", "Move to the start of the current line"),
    i(&[Key::Ctrl('w')], InputCmd::DeleteWordBack, "").help(IN, "Ctrl+W", "Delete the word before the cursor"),
    i(&[Key::Ctrl('u')], InputCmd::KillToLineStart, "").help(IN, "Ctrl+U (input)", "Delete to the start of the current line"),
    i(&[Key::Ctrl('k')], InputCmd::KillToLineEnd, "").local().help(IN, "Ctrl+K", "Delete to the end of the current line"),
    i(&[Key::Ctrl('h')], InputCmd::DeleteBack, "").does("Delete the character before the cursor, like Backspace (legacy terminals send it for Backspace)"),
    i(&[Key::Ctrl('a')], InputCmd::NoCursor, "").attached().avail(no_cursor).does("Move to the start of the line: the attached composer has no cursor, so nothing moves"),
    i(&[Key::Ctrl('e')], InputCmd::NoCursor, "").attached().avail(no_cursor).does("Move to the end of the line: the attached composer has no cursor, so nothing moves"),
    i(&[Key::Ctrl('k')], InputCmd::NoCursor, "").attached().avail(no_cursor).does("Delete to the end of the line: the attached composer has no cursor, so nothing is deleted"),
    i(&[Key::Alt('m')], InputCmd::ToggleMultiline, "Multi-line").slash("/ml").alt("/ml").hint().help(IN, "Alt+M", "Toggle multi-line mode"),
    i(&[Key::CtrlEnter], InputCmd::Send, "Send").alt("Ctrl+X Enter").help(IN, "Ctrl+Enter", "Send (where the terminal distinguishes it from Enter)"),
    i(&[Key::Esc], InputCmd::ReturnToChat, "Chat").te().help(IN, "Esc", "Close popup / return to Chat — never sends; unsent text is kept"),
    i(&[Key::Ctrl('r')], InputCmd::ReverseSearch, "History search").local().help(IN, "Ctrl+R", "Reverse search history"),
    i(&[Key::Up], InputCmd::UpOrHistoryPrev, "").local().help(IN, "Up / Down", "Cycle input history (when input empty)"),
    i(&[Key::Down], InputCmd::DownOrHistoryNext, "").local().does("Next visual row, else the next input-history entry"),
    i(&[Key::ShiftTab], InputCmd::CycleMode, "Mode").local().help(IN, "Shift+Tab in input", "Cycle permission mode (normal, auto-edit, plan; Yolo only via /mode yolo)"),
    i(&[Key::Char('/')], InputCmd::OpenSlashPopup, "Commands").help(CMD, "/", "Slash command autocomplete"),
    i(&[Key::Char('@')], InputCmd::OpenMentionPopup, "Mention").local().help(CMD, "@", "File / agent mention"),
    i(&[Key::Ctrl('p')], InputCmd::OpenPalette, "Commands").hint().help(CMD, "Ctrl+P", "Command palette"),
    i(&[Key::Ctrl('f')], InputCmd::OpenSearch, "Search").local().help(HelpCat::Search, "Ctrl+F", "Search within conversation (Typing mode)"),
    i(&[Key::Ctrl('c')], InputCmd::CancelOrQuit, "Stop").help(HelpCat::General, "Ctrl+C", "Stop active work; when idle, quit (confirms first if work or a draft exists)"),
    i(&[Key::Ctrl('t')], InputCmd::NewTab, "New tab").local().does("Open a new tab"),
    i(&[Key::Alt('v')], InputCmd::PasteClipboard, "Paste image").local().alt("palette: paste image from clipboard").does("Paste an image or text from the system clipboard"),
    i(&[Key::Backspace], InputCmd::DeleteBack, "").does("Delete the character before the cursor"),
    i(&[Key::Delete], InputCmd::DeleteForward, "").local().does("Delete the character at the cursor"),
    i(&[Key::Left], InputCmd::CursorLeft, "").local().does("Move the cursor one character left"),
    i(&[Key::Right], InputCmd::CursorRight, "").local().does("Move the cursor one character right"),
    i(&[Key::Home], InputCmd::CursorHome, "").local().does("Move to the start of the current row"),
    i(&[Key::End], InputCmd::CursorEnd, "").local().does("Move to the end of the current row"),
    i(&[Key::PageUp], InputCmd::TranscriptPageUp, "").attached().does("Scroll the transcript one page up"),
    i(&[Key::PageDown], InputCmd::TranscriptPageDown, "").attached().does("Scroll the transcript one page down"),
];

/// Append-only text fields (permission feedback, question answer, palette filter,
/// search, cross-search, reverse search): the cursor sits at the end (A4).
#[rustfmt::skip]
pub const FIELD: &[Binding] = &[
    f(&[Key::Ctrl('w')], FieldCmd::DeleteWordBack).does("Delete the last word of the field"),
    f(&[Key::Ctrl('u')], FieldCmd::Clear).does("Clear the field"),
    f(&[Key::Ctrl('h')], FieldCmd::DeleteBack).does("Delete the last character of the field, like Backspace"),
    f(&[Key::Ctrl('a')], FieldCmd::NoCursor).avail(no_cursor).does("Move to the start of the field: it has no cursor, so nothing moves"),
    f(&[Key::Ctrl('e')], FieldCmd::NoCursor).avail(no_cursor).does("Move to the end of the field: it has no cursor, so nothing moves"),
    f(&[Key::Ctrl('k')], FieldCmd::NoCursor).avail(no_cursor).does("Delete to the end of the field: it has no cursor, so nothing is deleted"),
];

pub const ROWS: &[Binding] = INPUT;
