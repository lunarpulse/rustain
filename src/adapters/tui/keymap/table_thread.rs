//! S-THREAD rows (Story 21.8, A6): the frame's own paging, copy, search, block-viewport and
//! closing rows, the stack chords every frame declares, and the writer's action rows. The
//! writer rows exist only in the attached host (`.attached()`), except `f Follow up` of a local
//! sent thread (`.local()`, story `agents-thread`); whether one can act on the
//! opened item now is `thread_inspector::applies`, the one rule the generated rail and the
//! dispatch share, so a key the rail does not paint is a consumed no-op (21-13 D12).
//!
//! `p` and `l` are the thread pager; `l` shadows the inherited `MarkdownBody` scroll-right at
//! the root. Inside the block viewport (`BodyViewport`, whose chain excludes the thread rows)
//! `h l 0 $` move the wide blocks' offset and `l` never pages. The text field
//! (`ThreadReply`) is a caret text field of its own, the attached reply field or the local
//! follow-up field (its rows resolve on both hosts): `Enter` sends, `Esc` clears and leaves it.

use super::model::{Binding, CommandId, Context, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadCmd {
    Earlier,
    Newer,
    Back,
    /// `c`: copy the opened item's original body.
    Copy,
    /// `Tab` (attached): the other view of the exchange, item ⇄ response.
    Toggle,
    /// `o` (attached): open the node's queued draft decision on L3.
    OpenDecision,
    /// `Enter`: enter the block viewport (only painted where a wide block exists).
    Viewport,
    /// `Esc` in the block viewport.
    LeaveViewport,
    /// `Enter` in the reply field.
    SendReply,
    /// `Esc` in the reply field.
    ReplyBack,
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
    /// `x`: retract the opened sent item, or the newest auto-sent reply of the opened item.
    Retract,
    /// `f` (local sent frame): follow up on the frame's newest eligible turn in a field of this
    /// window.
    FollowUp,
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

const fn r(keys: &'static [Key], command: ThreadCmd, label: &'static str) -> Binding {
    bind(
        Context::ThreadReply,
        keys,
        CommandId::Thread(command),
        label,
    )
    .te()
}

const fn v(keys: &'static [Key], command: ThreadCmd, label: &'static str) -> Binding {
    bind(
        Context::BodyViewport,
        keys,
        CommandId::Thread(command),
        label,
    )
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    t(&[Key::Char('p')], ThreadCmd::Earlier, "Earlier").does("Show the earlier part of the thread"),
    t(&[Key::Char('l')], ThreadCmd::Newer, "Newer").does("Show the later part of the thread"),
    t(&[Key::Esc], ThreadCmd::Back, "Close").does("Close the thread window (first clears a kept filter)"),
    t(&[Key::Ctrl('c')], ThreadCmd::Back, "").does("Close the thread window (back one frame)"),
    t(&[Key::Char('c')], ThreadCmd::Copy, "Copy").copy().does("Copy the opened item's original body, never the escaped display"),
    bind(Context::ThreadInspector, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").does("Filter the thread's turns by a case-insensitive query"),
    t(&[Key::Enter], ThreadCmd::Viewport, "Block viewport").does("Scroll a block wider than the window sideways, where one exists"),
    w(&[Key::Tab], ThreadCmd::Toggle, "Response").does("Switch between the item and the response view of this exchange"),
    w(&[Key::Char('o')], ThreadCmd::OpenDecision, "Open decision").does("Open the queued draft decision of this response above the thread window"),
    bind(Context::ThreadInspector, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").does("Open the menu above the thread window"),
    bind(Context::ThreadInspector, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").does("Open the command palette above the thread window"),
    bind(Context::ThreadInspector, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").does("Ask to quit without closing the thread window"),
    w(&[Key::Char('r')], ThreadCmd::Respond, "Respond").does("Reply to the opened item in a reply field of this window"),
    w(&[Key::Char('d')], ThreadCmd::Discuss, "Discuss").does("Discuss the opened item (or discuss concerns about its story list) in a reply field of this window"),
    w(&[Key::Char('b')], ThreadCmd::Busy, "Busy").does("Tell the sender you are busy and defer the item for 30 minutes"),
    w(&[Key::Char('a')], ThreadCmd::Acknowledge, "Acknowledge").does("Acknowledge the item: a receipt only, not an answer"),
    w(&[Key::Char('y')], ThreadCmd::Accept, "Accept").does("Accept the story assigned to you in the opened item"),
    w(&[Key::Char('s')], ThreadCmd::Share, "Share").does("Share the status or retrospective preview the item asks for"),
    w(&[Key::Char('n')], ThreadCmd::Decline, "Decline").does("Decline the status or retrospective request"),
    w(&[Key::Char('1')], ThreadCmd::LeaveOut(0), "Leave out").does("Leave the first retrospective field out of what is shared, or put it back"),
    w(&[Key::Char('2')], ThreadCmd::LeaveOut(1), "Leave out").does("Leave the second retrospective field out of what is shared, or put it back"),
    w(&[Key::Char('3')], ThreadCmd::LeaveOut(2), "Leave out").does("Leave the third retrospective field out of what is shared, or put it back"),
    w(&[Key::Char('u')], ThreadCmd::Context, "Use as context").does("Use the opened item as peer context for this conversation, or stop using it"),
    t(&[Key::Char('x')], ThreadCmd::Retract, "Retract").does("Retract the item you sent, or your newest auto-sent reply to this item (asks to confirm first)"),
    t(&[Key::Char('f')], ThreadCmd::FollowUp, "Follow up").local().does("Follow up on the newest eligible turn of the sent thread in a field of this window (local host, writable tab; sends through /team followup)"),
    r(&[Key::Enter], ThreadCmd::SendReply, "Send").does("Send the reply through /team reply and close the thread window (attached), or the follow-up through /team followup and keep the window open for its receipt (local)"),
    r(&[Key::Esc], ThreadCmd::ReplyBack, "Clear").does("Clear the reply or follow-up and leave its field"),
    r(&[Key::Ctrl('c')], ThreadCmd::ReplyBack, "").does("Clear the reply or follow-up and leave its field"),
    bind(Context::ThreadReply, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").te().does("Open the menu above this reply or follow-up field"),
    bind(Context::ThreadReply, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").te().does("Open the command palette above this reply or follow-up field"),
    bind(Context::ThreadReply, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").te().does("Ask to quit without closing this reply or follow-up field"),
    v(&[Key::Esc], ThreadCmd::LeaveViewport, "Leave").does("Leave the block viewport, back to the thread"),
    v(&[Key::Ctrl('c')], ThreadCmd::LeaveViewport, "").does("Leave the block viewport, back to the thread"),
    bind(Context::BodyViewport, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").does("Open the menu above the block viewport"),
    bind(Context::BodyViewport, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").does("Open the command palette above the block viewport"),
    bind(Context::BodyViewport, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").does("Ask to quit without closing the block viewport"),
];
