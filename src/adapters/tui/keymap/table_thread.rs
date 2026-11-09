//! S-THREAD rows (Story 21.8, A6): the frame's own paging and closing rows, the stack
//! chords every frame declares, and the writer's action rows. The writer rows exist
//! only in the attached host (`.attached()`); whether one can act on the opened item
//! now is `thread_inspector::applies`, the one rule the generated rail and the dispatch
//! share, so a key the rail does not paint is a consumed no-op (21-13 D12).
//!
//! `p` and `l` are the thread pager; `l` shadows the inherited `MarkdownBody` scroll-right.
//! The inherited `h`, `0`, `$` and `v` rows have nothing to act on at the root of a thread
//! and are never painted.

use super::model::{Binding, CommandId, Context, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadCmd {
    Earlier,
    Newer,
    Back,
    Respond,
    Discuss,
    Busy,
    Acknowledge,
    Accept,
    Share,
    Decline,
    /// `1`, `2`, `3`: the index into `RetroField::ALL`.
    LeaveOut(u8),
    Context,
}

const fn t(keys: &'static [Key], command: ThreadCmd, label: &'static str) -> Binding {
    bind(
        Context::ThreadInspector,
        keys,
        CommandId::Thread(command),
        label,
    )
}

const fn w(keys: &'static [Key], command: ThreadCmd, label: &'static str) -> Binding {
    t(keys, command, label).attached()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    t(&[Key::Char('p')], ThreadCmd::Earlier, "Earlier").does("Show the earlier part of the thread"),
    t(&[Key::Char('l')], ThreadCmd::Newer, "Newer").does("Show the later part of the thread"),
    t(&[Key::Esc], ThreadCmd::Back, "Close").does("Close the thread window"),
    t(&[Key::Ctrl('c')], ThreadCmd::Back, "").does("Close the thread window (back one frame)"),
    bind(Context::ThreadInspector, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").does("Open the menu above the thread window"),
    bind(Context::ThreadInspector, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").does("Open the command palette above the thread window"),
    bind(Context::ThreadInspector, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").does("Ask to quit without closing the thread window"),
    w(&[Key::Char('r')], ThreadCmd::Respond, "Respond").does("Reply to the opened item; the reply is typed in the composer"),
    w(&[Key::Char('d')], ThreadCmd::Discuss, "Discuss").does("Discuss the opened item (or discuss concerns about its story list) in a typed reply"),
    w(&[Key::Char('b')], ThreadCmd::Busy, "Busy").does("Tell the sender you are busy and defer the item for 30 minutes"),
    w(&[Key::Char('a')], ThreadCmd::Acknowledge, "Acknowledge").does("Acknowledge the item: a receipt only, not an answer"),
    w(&[Key::Char('y')], ThreadCmd::Accept, "Accept").does("Accept the story assigned to you in the opened item"),
    w(&[Key::Char('s')], ThreadCmd::Share, "Share").does("Share the status or retrospective preview the item asks for"),
    w(&[Key::Char('n')], ThreadCmd::Decline, "Decline").does("Decline the status or retrospective request"),
    w(&[Key::Char('1')], ThreadCmd::LeaveOut(0), "Leave out").does("Leave the first retrospective field out of what is shared, or put it back"),
    w(&[Key::Char('2')], ThreadCmd::LeaveOut(1), "Leave out").does("Leave the second retrospective field out of what is shared, or put it back"),
    w(&[Key::Char('3')], ThreadCmd::LeaveOut(2), "Leave out").does("Leave the third retrospective field out of what is shared, or put it back"),
    w(&[Key::Char('u')], ThreadCmd::Context, "Use as context").does("Use the opened item as peer context for this conversation, or stop using it"),
];
