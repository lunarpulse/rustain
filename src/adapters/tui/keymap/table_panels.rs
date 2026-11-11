//! Rows of the task drill-down, the wave/reorder Chat sub-states, the sidebar
//! panels beyond History and the wave overlay (Story 21.3 AC21, KG-2).
//!
//! They describe TODAY's hand-written arms of `app::handle_char` /
//! `handle_special_key` exactly (3-B2 moves the dispatch under the resolver and
//! must keep every by-value witness green). All are local-only surfaces. Arms
//! that no real terminal can reach (the synthetic `'\n'`, `'\r'`, `'\u{1b}'` and
//! arrow-glyph characters of `handle_char`) are not rows.
//!
//! * `ChatDrill` is exclusive: it inherits only Chat's non-character keys.
//! * Sidebar panels share the `Sidebar` rows (`j`, `k`, `q`, `Esc`, `Tab`, …).
//! * One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{Binding, CommandId, Context, Key, bind};

/// Commands of the drill-down, Chat sub-states, panels and the wave overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[rustfmt::skip]
pub enum PanelCmd {
    // Task drill-down (focus Chat).
    DrillCopy, DrillRetry, DrillSkip, DrillEdit, DrillPause, DrillScrollDown, DrillScrollUp,
    DrillTop, DrillBottom, DrillToggleExpand, DrillClose,
    // Chat sub-states.
    WaveBack, WaveOpenOverlay, WaveOpenDiverge, WaveCloseDiverge, WaveOpenFromChat, ReorderCancel,
    // Sidebar(Tasks), incl. reorder mode.
    TasksPrev, TasksNext, TasksCancelPlan, TasksPause, TasksSkip, TasksDrill,
    ReorderDown, ReorderUp, ReorderCommit,
    // Sidebar(Agents).
    AgentPauseToggle,
    // Wave overlay.
    WaveNext, WavePrev, WaveDrill, WaveClose, WaveRerun, WaveDiverge, WaveDivergeClose,
}

const fn d(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::ChatDrill, keys, CommandId::Panel(cmd), "").local()
}
const fn cw(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::ChatWave, keys, CommandId::Panel(cmd), "").local()
}
const fn cr(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::ChatReorder, keys, CommandId::Panel(cmd), "").local()
}
const fn t(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::SidebarTasks, keys, CommandId::Panel(cmd), "").local()
}
const fn r(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::TasksReorder, keys, CommandId::Panel(cmd), "").local()
}
const fn a(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::SidebarAgents, keys, CommandId::Panel(cmd), "").local()
}
const fn w(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::WaveOverlay, keys, CommandId::Panel(cmd), "").local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    // ── task drill-down (Chat focus, a task is drilled) ──
    d(&[Key::Char('c')], PanelCmd::DrillCopy).copy().does("Copy the drilled task's result"),
    d(&[Key::Char('r')], PanelCmd::DrillRetry).does("Retry the drilled task (the event loop checks its status)"),
    d(&[Key::Char('s')], PanelCmd::DrillSkip).does("Skip the drilled task (the event loop checks its status)"),
    d(&[Key::Char('e')], PanelCmd::DrillEdit).does("Edit the drilled task (the event loop checks its status)"),
    d(&[Key::Char('p')], PanelCmd::DrillPause).does("Pause the drilled task (the event loop checks its status)"),
    d(&[Key::Char('j')], PanelCmd::DrillScrollDown).does("Scroll the task result down one line"),
    d(&[Key::Down], PanelCmd::DrillScrollDown).does("Scroll the task result down one line"),
    d(&[Key::Char('k')], PanelCmd::DrillScrollUp).does("Scroll the task result up one line"),
    d(&[Key::Up], PanelCmd::DrillScrollUp).does("Scroll the task result up one line"),
    d(&[Key::Char('g')], PanelCmd::DrillTop).does("Scroll the task result to the top"),
    d(&[Key::Char('G')], PanelCmd::DrillBottom).does("Scroll the task result to the bottom"),
    d(&[Key::Enter], PanelCmd::DrillToggleExpand).does("Toggle the task result between half and full height"),
    d(&[Key::Esc], PanelCmd::DrillClose).does("Leave the drill-down and return to the Tasks panel"),
    // ── Chat sub-states ──
    cw(&[Key::Esc], PanelCmd::WaveBack).does("Close the divergence view, else dismiss the retained wave"),
    cw(&[Key::Enter], PanelCmd::WaveOpenOverlay).does("Open the retained wave overlay"),
    cw(&[Key::Char('d')], PanelCmd::WaveOpenDiverge).does("Open the divergence view of the retained wave"),
    cw(&[Key::Char('s')], PanelCmd::WaveCloseDiverge).does("Close the divergence view of the retained wave"),
    cw(&[Key::Char('r')], PanelCmd::WaveOpenFromChat).does("Open the retained wave overlay"),
    cr(&[Key::Esc], PanelCmd::ReorderCancel).does("Cancel the task reorder (the event loop restores the original order)"),
    // ── Sidebar(Tasks) ──
    t(&[Key::Up], PanelCmd::TasksPrev).does("Select the previous task"),
    t(&[Key::Down], PanelCmd::TasksNext).does("Select the next task"),
    t(&[Key::Char('j')], PanelCmd::TasksNext).does("Select the next task"),
    t(&[Key::Char('k')], PanelCmd::TasksPrev).does("Select the previous task"),
    t(&[Key::Char('x')], PanelCmd::TasksCancelPlan).does("Cancel the whole plan (asks to confirm)"),
    t(&[Key::Char('p')], PanelCmd::TasksPause).does("Pause the selected task"),
    t(&[Key::Char('s')], PanelCmd::TasksSkip).does("Skip the selected task"),
    t(&[Key::Enter], PanelCmd::TasksDrill).does("Open the selected task's result (the drill-down)"),
    // ── Sidebar(Tasks) in reorder mode ──
    r(&[Key::Char('j')], PanelCmd::ReorderDown).does("Move the selected task down"),
    r(&[Key::Char('k')], PanelCmd::ReorderUp).does("Move the selected task up"),
    r(&[Key::Enter], PanelCmd::ReorderCommit).does("Commit the new task order"),
    // ── Sidebar(Agents), list only (the inspector arrives with its extraction) ──
    a(&[Key::Char('p')], PanelCmd::AgentPauseToggle).does("Pause or resume the selected agent"),
    // ── Wave overlay ──
    w(&[Key::Char('j')], PanelCmd::WaveNext).does("Select the next spoke"),
    w(&[Key::Down], PanelCmd::WaveNext).does("Select the next spoke"),
    w(&[Key::Char('k')], PanelCmd::WavePrev).does("Select the previous spoke"),
    w(&[Key::Up], PanelCmd::WavePrev).does("Select the previous spoke"),
    w(&[Key::Enter], PanelCmd::WaveDrill).does("Open the selected spoke"),
    w(&[Key::Esc], PanelCmd::WaveClose).does("Close the wave overlay"),
    w(&[Key::Ctrl('c')], PanelCmd::WaveClose).does("Close the wave overlay (back one frame)"),
    w(&[Key::Char('r')], PanelCmd::WaveRerun).does("Re-run the selected spoke"),
    w(&[Key::Char('d')], PanelCmd::WaveDiverge).does("Open the divergence view"),
    w(&[Key::Char('s')], PanelCmd::WaveDivergeClose).does("Close the divergence view"),
];
