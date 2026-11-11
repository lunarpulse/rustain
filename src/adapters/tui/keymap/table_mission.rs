//! S-MC rows (Story 21.7, A4): the hub's tab, focus and closing rows and the stack chords every
//! frame declares. The scroll keys (`j k Down Up Ctrl+D Ctrl+U PgDn PgUp g G`) are the RU-25
//! rows of `InspectorBody`, inherited through the context's parent. Whether a row can act now
//! (a second tab, a wide frame, a selected row, the Messages tab) is decided by the key
//! executor and the generated rail from the same painted facts, so the rail never paints a key
//! that does nothing.
//!
//! The shared rows are both-host rows: the attached host reaches the hub with its Messages tab
//! only (A20), where `Left`/`Right` are inert and unpainted. The export and the open-a-window
//! rows exist only in the local host.

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
    /// `e` (Messages, local): export the unfiltered snapshot the view was read from.
    Export,
    /// `Enter` (a list tab, local): open the selected row's window (Agents, Tasks, Artifacts).
    Open,
    /// `c` (artifact window, local): copy the artifact's original source bytes.
    CopySource,
    /// `c` (agent window, local): copy the agent transcript's original text.
    CopyTranscript,
    /// `c` (task window, local): copy the task's result (or its error / skip reason).
    CopyResult,
    /// `p` (Agents and Tasks tabs, agent and task windows, local): pause a running agent or
    /// task, resume a suspended agent or a paused task.
    Pause,
    /// `x` (Agents tab, agent window, local): ask to kill the agent and its subtree; on the
    /// Tasks tab: ask to cancel the plan.
    Kill,
    /// `s` (Tasks tab, task window, local): skip the selected task (a decision when other tasks
    /// depend on it).
    Skip,
    /// `r` (task window, local): retry a failed task.
    Retry,
    /// `e` (task window, local): edit a failed task and queue it again.
    Edit,
    /// `c` (a list tab, local): copy the selected row's text, or its detail in the detail pane.
    CopyRow,
    /// Reorder (Tasks tab, local): move the task being reordered down, up, commit, cancel.
    ReorderDown,
    ReorderUp,
    ReorderCommit,
    ReorderCancel,
}

const fn m(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(
        Context::MissionControl,
        keys,
        CommandId::Mission(command),
        label,
    )
}

const fn w(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(
        Context::ArtifactWindow,
        keys,
        CommandId::Mission(command),
        label,
    )
    .local()
}

const fn g(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(
        Context::AgentWindow,
        keys,
        CommandId::Mission(command),
        label,
    )
    .local()
}

const fn ad(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(Context::Adapters, keys, CommandId::Mission(command), label).local()
}

const fn t(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(
        Context::TaskWindow,
        keys,
        CommandId::Mission(command),
        label,
    )
    .local()
}

const fn ro(keys: &'static [Key], command: MissionCmd, label: &'static str) -> Binding {
    bind(
        Context::MissionReorder,
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
    m(&[Key::Enter], MissionCmd::Open, "Open").local().does("Open the selected row's window (Agents, Tasks, Artifacts)"),
    m(&[Key::Char('e')], MissionCmd::Export, "Export").local().does("Export the unfiltered A2A log snapshot the Messages view was read from"),
    m(&[Key::Char('p')], MissionCmd::Pause, "Pause/Resume").local().does("Pause the selected running agent or task, or resume a suspended agent or a paused task (Agents, Tasks)"),
    m(&[Key::Char('x')], MissionCmd::Kill, "Kill").local().does("Ask to kill the selected agent and its subtree (Agents), or to cancel the plan (Tasks); a decision confirms it"),
    m(&[Key::Char('s')], MissionCmd::Skip, "Skip").local().does("Skip the selected task; a decision confirms it when other tasks depend on it (Tasks)"),
    m(&[Key::Char('c')], MissionCmd::CopyRow, "Copy").local().copy().does("Copy the selected row's original text, or its detail text while the detail pane has focus (refused above 8 MiB; list tabs only)"),
    bind(Context::MissionControl, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").does("Filter a list tab's rows, or the Messages rows, by a case-insensitive query (the closing claim always stays)"),
    m(&[Key::Esc], MissionCmd::Back, "Back").does("Clear a kept Messages filter, else close Mission Control (back one frame)"),
    m(&[Key::Ctrl('c')], MissionCmd::Back, "").does("Clear a kept Messages filter, else close Mission Control (back one frame) without cancelling work"),
    bind(Context::MissionControl, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").does("Open the Ctrl+X menu above Mission Control"),
    bind(Context::MissionControl, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").does("Open the command palette above Mission Control"),
    bind(Context::MissionControl, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").does("Ask to quit without closing Mission Control"),
    // ── the artifact window (local) ──
    w(&[Key::Char('c')], MissionCmd::CopySource, "Copy").copy().does("Copy the artifact's original source bytes (refused above 8 MiB), never the sanitized display"),
    bind(Context::ArtifactWindow, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").local().does("Show only the body lines that contain a query"),
    w(&[Key::Esc], MissionCmd::Back, "Back").does("Clear a kept filter, else back to the frame beneath"),
    w(&[Key::Ctrl('c')], MissionCmd::Back, "").does("Back to the frame beneath without cancelling work"),
    bind(Context::ArtifactWindow, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu above the artifact window"),
    bind(Context::ArtifactWindow, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the artifact window"),
    bind(Context::ArtifactWindow, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the artifact window"),
    // ── the agent window (local) ──
    g(&[Key::Char('p')], MissionCmd::Pause, "Pause/Resume").does("Pause the agent, or resume it when suspended"),
    g(&[Key::Char('x')], MissionCmd::Kill, "Kill").does("Ask to kill the agent and its subtree; a decision confirms it"),
    g(&[Key::Char('c')], MissionCmd::CopyTranscript, "Copy").copy().does("Copy the agent transcript's original text (refused above 8 MiB), never the sanitized display"),
    bind(Context::AgentWindow, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").local().does("Show only the transcript lines that contain a query"),
    g(&[Key::Esc], MissionCmd::Back, "Back").does("Clear a kept filter, else back to the frame beneath"),
    g(&[Key::Ctrl('c')], MissionCmd::Back, "").does("Back to the frame beneath without cancelling work"),
    bind(Context::AgentWindow, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu above the agent window"),
    bind(Context::AgentWindow, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the agent window"),
    bind(Context::AgentWindow, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the agent window"),
    // ── the task window (local) ──
    t(&[Key::Char('p')], MissionCmd::Pause, "Pause/Resume").does("Pause the task, or resume it when paused"),
    t(&[Key::Char('s')], MissionCmd::Skip, "Skip").does("Skip the task; a decision confirms it when other tasks depend on it"),
    t(&[Key::Char('r')], MissionCmd::Retry, "Retry").does("Retry the failed task"),
    t(&[Key::Char('e')], MissionCmd::Edit, "Edit").does("Edit the failed task and queue it again"),
    t(&[Key::Char('c')], MissionCmd::CopyResult, "Copy").copy().does("Copy the task's result, error or skip reason (refused above 8 MiB), never the sanitized display"),
    bind(Context::TaskWindow, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").local().does("Show only the body lines that contain a query"),
    t(&[Key::Esc], MissionCmd::Back, "Back").does("Clear a kept filter, else back to the frame beneath"),
    t(&[Key::Ctrl('c')], MissionCmd::Back, "").does("Back to the frame beneath without cancelling work"),
    bind(Context::TaskWindow, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu above the task window"),
    bind(Context::TaskWindow, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the task window"),
    bind(Context::TaskWindow, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the task window"),
    // ── the Tasks tab while a task is being reordered (local) ──
    ro(&[Key::Char('j')], MissionCmd::ReorderDown, "Move down").does("Move the task being reordered down"),
    ro(&[Key::Down], MissionCmd::ReorderDown, "Move down").does("Move the task being reordered down"),
    ro(&[Key::Char('k')], MissionCmd::ReorderUp, "Move up").does("Move the task being reordered up"),
    ro(&[Key::Up], MissionCmd::ReorderUp, "Move up").does("Move the task being reordered up"),
    ro(&[Key::Enter], MissionCmd::ReorderCommit, "Commit").does("Commit the new task order"),
    ro(&[Key::Esc], MissionCmd::ReorderCancel, "Cancel").does("Cancel the reorder and restore the original order"),
    ro(&[Key::Ctrl('c')], MissionCmd::ReorderCancel, "").does("Cancel the reorder and restore the original order without cancelling work"),
    bind(Context::MissionReorder, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without ending the reorder"),
    // ── the Adapters window (local) ──
    ad(&[Key::Esc], MissionCmd::Back, "Back").does("Back to the frame beneath"),
    ad(&[Key::Ctrl('c')], MissionCmd::Back, "").does("Back to the frame beneath without cancelling work"),
    bind(Context::Adapters, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu above the Adapters window"),
    bind(Context::Adapters, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the Adapters window"),
    bind(Context::Adapters, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the Adapters window"),
];
