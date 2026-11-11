//! S-MC rows (Story 21.7, A4): the hub's tab, focus and closing rows and the stack chords every
//! frame declares. The scroll keys (`j k Down Up Ctrl+D Ctrl+U PgDn PgUp g G`) are the RU-25
//! rows of `InspectorBody`, inherited through the context's parent. Whether a row can act now
//! (a second tab, a wide frame) is decided by the key executor and the generated rail from the
//! same painted facts, so the rail never paints a key that does nothing.
//!
//! Every row is local-only in this slice: the attached host reaches the hub with its Messages
//! tab (a later slice), which turns the shared rows into both-host rows.

use super::model::{Binding, CommandId, Context, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MissionCmd {
    PrevTab,
    NextTab,
    /// `Tab`: master ⇄ detail (list tabs on a frame wide enough for both).
    Focus,
    Back,
}

const fn m(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(
        Context::MissionControl,
        keys,
        CommandId::Mission(command),
        label,
    )
    .local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    m(&[Key::Left], MissionCmd::PrevTab, "Tab").does("Select the previous Mission Control tab (inert while only one tab exists)"),
    m(&[Key::Right], MissionCmd::NextTab, "Tab").does("Select the next Mission Control tab (inert while only one tab exists)"),
    m(&[Key::Tab], MissionCmd::Focus, "Detail").does("Move between the list and its detail preview (only where the frame shows both)"),
    m(&[Key::Esc], MissionCmd::Back, "Back").does("Close Mission Control (back one frame)"),
    m(&[Key::Ctrl('c')], MissionCmd::Back, "").does("Close Mission Control (back one frame) without cancelling work"),
    bind(Context::MissionControl, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu above Mission Control"),
    bind(Context::MissionControl, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above Mission Control"),
    bind(Context::MissionControl, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing Mission Control"),
];
