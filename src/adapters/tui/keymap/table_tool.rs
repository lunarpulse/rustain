//! Tool frame commands; every row is local-only.

use super::model::{Binding, CommandId, Context, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCmd {
    PrevTab,
    NextTab,
    PrevCall,
    NextCall,
    NextMatch,
    PrevMatch,
    Wrap,
    Copy,
    Back,
}

const fn t(keys: &'static [Key], command: ToolCmd, label: &'static str) -> Binding {
    bind(
        Context::ToolInspector,
        keys,
        CommandId::Tool(command),
        label,
    )
    .local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    t(&[Key::Left], ToolCmd::PrevTab, "Tab").hint().does("Select the previous available tool tab"),
    t(&[Key::Right], ToolCmd::NextTab, "Tab").hint().does("Select the next available tool tab"),
    t(&[Key::Char('[')], ToolCmd::PrevCall, "Previous").hint().does("Inspect the previous tool call in this turn"),
    t(&[Key::Char(']')], ToolCmd::NextCall, "Next").hint().does("Inspect the next tool call in this turn"),
    bind(Context::ToolInspector, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").hint().local().does("Search the active tool tab"),
    t(&[Key::Char('n')], ToolCmd::NextMatch, "Next match").hint().does("Select the next search match, wrapping at the end"),
    t(&[Key::Char('N')], ToolCmd::PrevMatch, "Previous match").hint().does("Select the previous search match, wrapping at the start"),
    t(&[Key::Char('w')], ToolCmd::Wrap, "Wrap").hint().does("Toggle soft wrapping, keeping the logical line at the top"),
    t(&[Key::Char('c')], ToolCmd::Copy, "Copy").copy().hint().does("Copy the active tool tab's original source"),
    t(&[Key::Esc], ToolCmd::Back, "Back").hint().does("Back from the tool inspector"),
    t(&[Key::Ctrl('c')], ToolCmd::Back, "").does("Back from the tool inspector without cancelling work"),
    bind(Context::ToolInspector, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu without closing the tool inspector"),
    bind(Context::ToolInspector, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the tool inspector"),
    bind(Context::ToolInspector, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the tool inspector"),
];
