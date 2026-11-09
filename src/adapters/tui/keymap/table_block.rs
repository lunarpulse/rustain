//! S-BLOCK (Story 21.13): horizontal block-local scrolling shared by every
//! Markdown body window, and the commands of one inspected code block or table.
//! The Markdown body rows are both-host rows (Story 21.8: the attached host reaches them
//! through the thread frame); the block-viewer rows are local-only (D4). Whether a row can
//! change anything now is the surface's `applies` rule, so the rail never paints a key that
//! does nothing.

use super::model::{Binding, CommandId, Context, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;
use crate::adapters::tui::presentation::viewer::ViewerCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockCmd {
    NextMatch,
    PrevMatch,
    Copy,
    Back,
    Numbers,
    Wrap,
    PrevBlock,
    NextBlock,
}

/// Commands shared by every Markdown body window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyCmd {
    ToggleMode,
}

const fn m(keys: &'static [Key], command: ViewerCmd, label: &'static str) -> Binding {
    bind(
        Context::MarkdownBody,
        keys,
        CommandId::Viewer(command),
        label,
    )
}

const fn b(keys: &'static [Key], command: BlockCmd, label: &'static str) -> Binding {
    bind(Context::BlockViewer, keys, CommandId::Block(command), label).local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    m(&[Key::Char('h')], ViewerCmd::ScrollLeft, "Left").hint().does("Scroll wide rows left by one tab stop"),
    m(&[Key::Char('l')], ViewerCmd::ScrollRight, "Right").hint().does("Scroll wide rows right by one tab stop"),
    m(&[Key::Char('0')], ViewerCmd::LeftEdge, "Start").hint().does("Scroll wide rows to their first column"),
    m(&[Key::Char('$')], ViewerCmd::RightEdge, "End").hint().does("Scroll wide rows to their last column"),
    bind(Context::MarkdownBody, &[Key::Char('v')], CommandId::Body(BodyCmd::ToggleMode), "View").hint().does("Switch the body between rendered markdown and its source"),
    bind(Context::BlockViewer, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").hint().local().does("Search the displayed rows of the block"),
    b(&[Key::Char('n')], BlockCmd::NextMatch, "Next match").hint().does("Select the next search match, wrapping at the end"),
    b(&[Key::Char('N')], BlockCmd::PrevMatch, "Previous match").hint().does("Select the previous search match, wrapping at the start"),
    b(&[Key::Char('c')], BlockCmd::Copy, "Copy").copy().hint().does("Copy the block's exact source, never its borders, markers or line numbers"),
    b(&[Key::Char('#')], BlockCmd::Numbers, "Numbers").hint().does("Toggle source line numbers for code, or for a table shown as source"),
    b(&[Key::Char('w')], BlockCmd::Wrap, "Wrap").hint().does("Toggle wrapping of a code block at the viewport width"),
    b(&[Key::Char('[')], BlockCmd::PrevBlock, "Previous").hint().does("Inspect the previous code block or table of this turn"),
    b(&[Key::Char(']')], BlockCmd::NextBlock, "Next").hint().does("Inspect the next code block or table of this turn"),
    b(&[Key::Esc], BlockCmd::Back, "Back").hint().does("Back from the block viewer"),
    b(&[Key::Ctrl('c')], BlockCmd::Back, "").does("Back from the block viewer without cancelling work"),
    bind(Context::BlockViewer, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu without closing the block viewer"),
    bind(Context::BlockViewer, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the block viewer"),
    bind(Context::BlockViewer, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the block viewer"),
];
