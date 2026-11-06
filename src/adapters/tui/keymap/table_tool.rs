//! Tool frame commands; every row is local-only.

use super::model::{Binding, CommandId, Context, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCmd {
    PrevTab,
    NextTab,
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
    t(&[Key::Esc], ToolCmd::Back, "Back").hint().does("Back from the tool inspector"),
    t(&[Key::Ctrl('c')], ToolCmd::Back, "").does("Back from the tool inspector without cancelling work"),
    bind(Context::ToolInspector, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu without closing the tool inspector"),
    bind(Context::ToolInspector, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the tool inspector"),
    bind(Context::ToolInspector, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the tool inspector"),
];
