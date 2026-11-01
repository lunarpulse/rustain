//! Rows of the Agents-panel inspector, the drill-down on one agent (Story 21.3 AC21).
//!
//! The inspector owns the chat pane while an agent is open (`drill_down_agent`), so its
//! keys never reach the Agents list rows. `agent_inspector_keys::on_key` resolves them
//! here and `widgets::agent_inspector` paints its rails from the labels. Local only.
//!
//! * `y` / `n` answer a pending kill confirmation and are inert otherwise (a real `Enter` never
//!   confirms a cascade kill: it keeps its old meaning, re-opening the entry).
//! * A read-only tab blocks every command except `Esc`; its `j`/`k` fall through to the
//!   Agents list (they move the selection, not the inspector).
//! * Bare `c` is the named PC-14 exception owned by 21-7.
//! * One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{Binding, CommandId, Context, Key, bind};

/// Commands of the Agents-panel inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[rustfmt::skip]
pub enum InspectorCmd {
    /// Pause a running agent, resume a suspended one.
    Pause,
    /// Ask to confirm killing the agent and its subtree.
    Kill,
    /// Kill the agent and its subtree (only while the confirmation is shown).
    ConfirmKill,
    /// Dismiss the kill confirmation (only while it is shown).
    CancelKill,
    /// Notice: change the model from the model selector.
    Model,
    /// Notice: updating tools is not available from the inspector yet.
    Tools,
    /// Open the agent's output in a read-only conversation tab.
    Conversation,
    ScrollDown,
    ScrollUp,
    /// Close the inspector, back to the Agents list.
    Close,
}

const fn i(keys: &'static [Key], cmd: InspectorCmd, label: &'static str) -> Binding {
    bind(
        Context::SidebarAgentInspector,
        keys,
        CommandId::Inspector(cmd),
        label,
    )
    .local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    i(&[Key::Char('p')], InspectorCmd::Pause, "Pause/Resume").does("Pause the open agent, or resume it when suspended"),
    i(&[Key::Char('x')], InspectorCmd::Kill, "Kill (cascade)").does("Kill the open agent and its subtree (asks to confirm)"),
    i(&[Key::Char('y')], InspectorCmd::ConfirmKill, "Confirm").does("Confirm the pending kill (only while the confirmation is shown)"),
    i(&[Key::Char('n')], InspectorCmd::CancelKill, "Cancel").does("Cancel the pending kill (only while the confirmation is shown)"),
    i(&[Key::Char('m')], InspectorCmd::Model, "Change model").does("Change the agent's model (a notice names the model selector)"),
    i(&[Key::Char('t')], InspectorCmd::Tools, "Update tools").does("Update the agent's tools (a notice: not available from the inspector yet)"),
    i(&[Key::Char('c')], InspectorCmd::Conversation, "Conversation tab").does("Open the agent's output in a read-only conversation tab"),
    i(&[Key::Char('j')], InspectorCmd::ScrollDown, "").does("Scroll the inspector down one line"),
    i(&[Key::Char('k')], InspectorCmd::ScrollUp, "").does("Scroll the inspector up one line"),
    i(&[Key::Esc], InspectorCmd::Close, "Back").does("Close the inspector and return to the Agents list"),
];
