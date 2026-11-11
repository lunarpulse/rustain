//! S-MC, the Mission Control hub (Story 21.7). One L2 frame, `Surface::MissionControl`, whose
//! payload is the session-global [`MissionControlState`]: the tab, the focus, the scroll
//! positions and a selection that is a row **id**, never an index (A3). Tab bodies read the
//! live state when they paint, so a conversation-tab switch never closes or rebinds the hub.
//!
//! Slices 7-A and 7-B deliver the frame, the key rows, the shared list builder and three tabs:
//! Messages (21-8's A2A log view, re-homed: the block page lives in [`messages::LogPage`]),
//! Room and Artifacts (a list tab built from the durable fold). The other tabs (Agents, Tasks)
//! are added by the later slices through the same builder. A list tab that a test registers
//! (`register_probe`) is how the shared list builder (selection by id, the "no longer
//! available" placeholder, master/detail) is proven independently of a real tab.
//!
//! Presented, not held (A14): the Room poll runs only while the last *completed* draw painted
//! the Room body on a Mission Control frame that is still on top. The paint is read-only (the
//! compositor shares the state), so the facts it learned wait in `room_paint` until the host
//! applies them after the draw ([`TuiState::mission_draw_completed`]). The Messages visit
//! commits by the same rule (A13).

pub mod agents;
pub mod artifacts;
pub mod keys;
pub mod messages;
pub mod tabs;
pub mod tasks;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::adapters::tui::keymap::Host;
use crate::adapters::tui::presentation::inspector_search::InspectorSearch;
use crate::adapters::tui::presentation::{Surface, viewer::Viewer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::room_panel::RoomPaint;
pub use messages::LogPage;
pub use tabs::{McRow, Resolved, Selection};

/// The tabs of the hub, in the fixed order Agents, Tasks, Messages, Room, Artifacts; a tab
/// that no slice has delivered yet does not exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum McTab {
    /// The subagents of this session (`Ctrl+X s`), a list tab: `Enter` opens the agent window.
    Agents,
    /// The active conversation's resolved plan (`Ctrl+X t`), a list tab: `Enter` opens the task
    /// window.
    Tasks,
    /// The A2A log (`Ctrl+X l`, `/team log`): 21-8's log view.
    Messages,
    /// The durable room replay (`Ctrl+X r`, `/room`).
    #[default]
    Room,
    /// The durable artifact fold, a list tab (`Ctrl+X e`, `/artifacts`).
    Artifacts,
    /// A list tab whose rows a caller supplies ([`MissionControlState::register_probe`]).
    /// Only tests register one; it is never delivered to an operator.
    #[doc(hidden)]
    Probe,
}

impl McTab {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            McTab::Agents => "Agents",
            McTab::Tasks => "Tasks",
            McTab::Messages => "Messages",
            McTab::Room => "Room",
            McTab::Artifacts => "Artifacts",
            McTab::Probe => "Probe",
        }
    }

    /// The frame title, `Mission Control · <Tab>`.
    #[must_use]
    pub fn title(self) -> String {
        format!("Mission Control · {}", self.label())
    }

    /// A list tab keeps a selection and shows a master list (and, wide, a detail preview).
    #[must_use]
    pub const fn is_list(self) -> bool {
        matches!(
            self,
            McTab::Agents | McTab::Tasks | McTab::Artifacts | McTab::Probe
        )
    }
}

/// Which pane of a list tab the movement keys act on (a pane, not a focus owner: the hub is
/// one frame and owns no base focus).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McPane {
    #[default]
    Master,
    Detail,
}

/// The tabs an operator can reach now (fixed order): the local host's four, the attached
/// host's Messages only (A20), plus a registered test probe.
#[derive(Debug, Clone, Copy)]
pub struct Delivered {
    tabs: [McTab; 6],
    len: usize,
}

impl Delivered {
    #[must_use]
    pub fn as_slice(&self) -> &[McTab] {
        &self.tabs[..self.len]
    }
}

/// What the last paint measured; the keys clamp against it (height == paint: one fit for the
/// rows painted and the rows a key can reach).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fit {
    /// Rows of the painted master list (placeholder included) and the rows it could show.
    pub list_rows: usize,
    pub list_viewport: usize,
    /// First painted master row.
    pub list_top: usize,
    /// Rows of the detail preview and the rows it could show.
    pub detail_rows: usize,
    pub detail_viewport: usize,
    /// The frame was wide enough for master and detail side by side.
    pub wide: bool,
    /// The body had more rows than it could show.
    pub scrollable: bool,
    /// The rail painted `/ Search` whole (Messages): the key is live only where its hint is.
    pub search: bool,
    /// The rail painted `e Export` whole (Messages, local host, a report to export).
    pub export: bool,
    /// The rail painted `Enter Open` whole (a list tab with a selected row).
    pub open: bool,
    /// The rail painted `p Pause/Resume` whole (an agent or task that can be paused or resumed).
    pub pause: bool,
    /// The rail painted `x Kill` / `x Cancel plan` whole (an agent that can be killed, a plan
    /// that can be cancelled).
    pub kill: bool,
    /// The rail painted `s Skip` whole (a task that can be skipped).
    pub skip: bool,
    /// The rail painted `c Copy` whole (a list tab with a selected row, local host).
    pub copy: bool,
}

/// One list tab's rows and the words around them.
#[derive(Debug, Default)]
pub struct McList {
    /// Every row the last read produced; `visible` is its filtered view while a filter is kept.
    all: Vec<McRow>,
    visible: Option<Vec<McRow>>,
    /// What the list says when it has no rows (a read failure, a zero state).
    pub empty: Option<String>,
    /// One muted line above the rows (a zero-state headline, a hazard count).
    pub note: Option<String>,
    /// Bumped by every replacement or refilter; caches key on it.
    pub rev: u64,
}

impl McList {
    /// The rows the list paints and the keys act on (the filtered view while a filter is kept).
    #[must_use]
    pub fn rows(&self) -> &[McRow] {
        self.visible.as_deref().unwrap_or(&self.all)
    }

    /// Every row the last read produced, before any filter (the monitor shows these).
    #[must_use]
    pub fn all(&self) -> &[McRow] {
        &self.all
    }

    /// The list has rows before any filter (a filter that matches nothing leaves it empty).
    #[must_use]
    pub fn has_rows(&self) -> bool {
        !self.all.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct MissionControlState {
    pub tab: McTab,
    pub pane: McPane,
    /// Scroll position of the detail preview (list tabs, detail pane).
    pub detail: Viewer,
    /// The Room tab's cursor row: pinned to the first painted node, never highlighted.
    pub room_cursor: usize,
    /// The Messages tab's page: set by `/team log` and `Ctrl+X l`, dropped when the frame
    /// closes (the next visit reads the journal again).
    pub messages: Option<LogPage>,
    /// Why the read behind a Messages frame failed (painted instead of "Reading…").
    pub messages_failed: Option<String>,
    /// The filter of the current list tab (`/`); kept per frame, cleared when the tab changes
    /// or the frame closes.
    pub list_search: InspectorSearch,
    selections: HashMap<McTab, Selection>,
    lists: HashMap<McTab, McList>,
    /// What the Tasks tab was last built from (the conversation, the plan, the task statuses).
    pub tasks: Option<tasks::TasksSource>,
    /// Fingerprint of what the Tasks rows were last built from, and how many times they were
    /// built (the witness that an unchanged repaint rebuilds nothing).
    pub tasks_fp: Option<u64>,
    pub tasks_builds: u32,
    fit: Cell<Fit>,
    room_paint: RefCell<Option<RoomPaint>>,
    room_presented: bool,
    /// The Room body was painted by the monitor column in the last completed draw (A14).
    monitor_room_paint: RefCell<Option<RoomPaint>>,
    monitor_room_presented: bool,
    /// The hub was opened at least once: the monitor follows its tab only after that (before
    /// it, the monitor shows Tasks).
    visited: bool,
    detail_cache: RefCell<Option<DetailCache>>,
    detail_builds: Cell<u32>,
    next_rev: u64,
}

impl MissionControlState {
    /// The delivered tabs for `host`.
    #[must_use]
    pub fn delivered(&self, host: Host) -> Delivered {
        let mut tabs = [McTab::Messages; 6];
        let mut len = 1;
        if host == Host::Local {
            tabs[0] = McTab::Agents;
            tabs[1] = McTab::Tasks;
            tabs[2] = McTab::Messages;
            tabs[3] = McTab::Room;
            tabs[4] = McTab::Artifacts;
            len = 5;
            if self.lists.contains_key(&McTab::Probe) {
                tabs[5] = McTab::Probe;
                len = 6;
            }
        }
        Delivered { tabs, len }
    }

    /// The painted frame title: the tab's, with the Messages view's `· filtered` mark.
    #[must_use]
    pub fn title(&self) -> String {
        match (self.tab, &self.messages) {
            (McTab::Messages, Some(page)) => page.title(),
            (tab, _) => tab.title(),
        }
    }

    /// Register (or refresh) the probe list tab.
    pub fn register_probe(&mut self, rows: Vec<McRow>) {
        self.set_list(McTab::Probe, rows, None, None);
    }

    /// Replace the rows of a list tab only when they (or the words around them) differ from
    /// what it holds: an unchanged rebuild bumps no revision, so the detail cache and the
    /// repaint facts keyed on it stay valid. Returns whether anything changed.
    pub fn set_list_if_changed(
        &mut self,
        tab: McTab,
        rows: Vec<McRow>,
        empty: Option<String>,
        note: Option<String>,
    ) -> bool {
        if let Some(list) = self.lists.get(&tab)
            && list.all == rows
            && list.empty == empty
            && list.note == note
        {
            return false;
        }
        self.set_list(tab, rows, empty, note);
        true
    }

    /// Drop the selection of `tab` (its rows now name something else).
    pub fn forget_selection(&mut self, tab: McTab) {
        self.selections.remove(&tab);
    }

    /// Replace the rows of a list tab. The selection is reconciled against the rows it
    /// pointed into *before* they are replaced, so a removed row leaves its position and
    /// label behind for the placeholder.
    pub fn set_list(
        &mut self,
        tab: McTab,
        rows: Vec<McRow>,
        empty: Option<String>,
        note: Option<String>,
    ) {
        if let Some(old) = self.lists.get(&tab)
            && let Some(selection) = self.selections.get_mut(&tab)
            && let Some(index) = selection
                .id
                .as_ref()
                .and_then(|id| old.rows().iter().position(|row| row.id == *id))
        {
            selection.at = index;
            selection.label = old.rows()[index].text.clone();
        }
        self.next_rev += 1;
        self.lists.insert(
            tab,
            McList {
                all: rows,
                visible: None,
                empty,
                note,
                rev: self.next_rev,
            },
        );
        if tab == self.tab && !self.list_search.query.as_str().is_empty() {
            self.refilter();
        }
    }

    /// The list of the current tab (none for a body tab or before its first read).
    #[must_use]
    pub fn list(&self) -> Option<&McList> {
        self.lists.get(&self.tab)
    }

    /// The visible rows of the current list tab (none for a body tab).
    #[must_use]
    pub fn rows(&self) -> &[McRow] {
        self.list().map_or(&[], McList::rows)
    }

    /// The list a tab holds (none before its first read), whatever the current tab is.
    #[must_use]
    pub fn list_for(&self, tab: McTab) -> Option<&McList> {
        self.lists.get(&tab)
    }

    /// The tab the monitor column shows (A16): the last tab the hub was on, Tasks while the
    /// hub was never opened.
    #[must_use]
    pub fn monitor_tab(&self) -> McTab {
        if self.visited { self.tab } else { McTab::Tasks }
    }

    /// The current tab's selection (empty until the operator or a refresh set one).
    #[must_use]
    pub fn selection(&self) -> &Selection {
        static NONE: Selection = Selection {
            id: None,
            at: 0,
            label: String::new(),
        };
        self.selections.get(&self.tab).unwrap_or(&NONE)
    }

    /// How the current selection resolves against the live rows now.
    #[must_use]
    pub fn resolved(&self) -> Resolved {
        tabs::resolve(self.rows(), self.selection())
    }

    /// The id of the selected row of the current list tab, if it is still there.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        match self.resolved() {
            Resolved::Found(index) => self.rows().get(index).map(|row| row.id.as_str()),
            _ => None,
        }
    }

    /// The selected row of the current list tab, if it is still there.
    #[must_use]
    pub fn selected_row(&self) -> Option<&McRow> {
        match self.resolved() {
            Resolved::Found(index) => self.rows().get(index),
            _ => None,
        }
    }

    /// The last painted measurements (the keys clamp against them).
    #[must_use]
    pub fn fit(&self) -> Fit {
        self.fit.get()
    }

    pub fn set_fit(&self, fit: Fit) {
        self.fit.set(fit);
    }

    /// Retarget in place: the tab changes, the pane positions start over and a list filter
    /// never follows the operator to another tab.
    pub(crate) fn switch_to(&mut self, tab: McTab) {
        self.visited = true;
        if self.tab != tab {
            self.list_search.back();
            self.refilter();
            self.tab = tab;
            self.pane = McPane::Master;
            self.detail = Viewer::default();
            self.fit.set(Fit::default());
        }
    }

    /// The frame closed: transient positions and the paint facts go, and so does the Messages
    /// page (a closed visit is never committed; the next one reads the journal again) and a
    /// list filter; the tab and the per-tab selections stay (the monitor of a later slice
    /// follows the last tab).
    pub(crate) fn closed(&mut self) {
        self.pane = McPane::Master;
        self.detail = Viewer::default();
        self.fit.set(Fit::default());
        self.room_paint.borrow_mut().take();
        self.room_presented = false;
        self.messages = None;
        self.messages_failed = None;
        self.list_search.back();
        self.refilter();
    }

    /// The paint of the Room body records what it learned; the host applies it after the draw.
    pub fn record_room_paint(&self, paint: RoomPaint) {
        *self.room_paint.borrow_mut() = Some(paint);
    }

    /// The monitor column's paint of the Room body records what it learned like the hub's.
    pub fn record_monitor_room_paint(&self, paint: RoomPaint) {
        *self.monitor_room_paint.borrow_mut() = Some(paint);
    }

    /// Consume the Messages paint mark (see [`LogPage::take_painted`]).
    pub(crate) fn take_messages_painted(&self) -> bool {
        self.messages.as_ref().is_some_and(LogPage::take_painted)
    }

    /// Move the selection of the current list tab by a viewer command (master focus).
    pub(crate) fn move_selection(
        &mut self,
        command: crate::adapters::tui::presentation::viewer::ViewerCmd,
    ) {
        use crate::adapters::tui::presentation::viewer::ViewerCmd::{
            HalfDown, HalfUp, LineDown, LineUp, PageDown, PageUp, Tail, Top,
        };
        let fit = self.fit.get();
        let tab = self.tab;
        let Some(list) = self.lists.get(&tab) else {
            return;
        };
        let rows: &[McRow] = list.rows();
        let selection = self.selections.entry(tab).or_default();
        let resolved = tabs::resolve(rows, selection);
        let len = tabs::view_len(rows.len(), resolved);
        if len == 0 {
            return;
        }
        let pos = tabs::view_pos(resolved);
        let viewport = fit.list_viewport.max(1);
        let to = match command {
            LineDown => pos.saturating_add(1),
            LineUp => pos.saturating_sub(1),
            HalfDown => pos.saturating_add((viewport / 2).max(1)),
            HalfUp => pos.saturating_sub((viewport / 2).max(1)),
            PageDown => pos.saturating_add(viewport.saturating_sub(1).max(1)),
            PageUp => pos.saturating_sub(viewport.saturating_sub(1).max(1)),
            Top => 0,
            Tail => len - 1,
            _ => return,
        }
        .min(len - 1);
        if let Some(index) = tabs::target(rows.len(), resolved, to) {
            tabs::select(selection, rows, index);
            self.detail = Viewer::default();
        }
    }

    /// The filter field of the current tab: Messages' turn filter, a list tab's row filter.
    #[must_use]
    pub fn search(&self) -> Option<&InspectorSearch> {
        match self.tab {
            McTab::Messages => self.messages.as_ref().map(|page| &page.search),
            tab if tab.is_list() => Some(&self.list_search),
            _ => None,
        }
    }

    pub fn search_mut(&mut self) -> Option<&mut InspectorSearch> {
        match self.tab {
            McTab::Messages => self.messages.as_mut().map(|page| &mut page.search),
            tab if tab.is_list() => Some(&mut self.list_search),
            _ => None,
        }
    }

    /// A filter is kept (a non-empty query) on the current tab.
    #[must_use]
    pub fn filter_kept(&self) -> bool {
        self.search()
            .is_some_and(|search| !search.query.as_str().is_empty())
    }

    /// Re-derive the visible rows of the current list tab from its filter. A selected row the
    /// filter hides is not "gone": the selection is dropped, never shown as a placeholder.
    pub(crate) fn refilter(&mut self) {
        let tab = self.tab;
        let Some(list) = self.lists.get_mut(&tab) else {
            return;
        };
        let search = &self.list_search;
        list.visible = (!search.query.as_str().is_empty()).then(|| {
            list.all
                .iter()
                .filter(|row| search.matches(&row.text))
                .cloned()
                .collect()
        });
        self.next_rev += 1;
        list.rev = self.next_rev;
        let hidden = self.selections.get(&tab).is_some_and(|selection| {
            selection
                .id
                .as_ref()
                .is_some_and(|id| !list.rows().iter().any(|row| row.id == *id))
        });
        if hidden {
            self.selections.remove(&tab);
        }
        self.detail = Viewer::default();
    }
}

/// Open (or retarget) the one Mission Control frame on Messages with a freshly read log page:
/// one filterable turn per row, the closing claim kept under every filter. A kept filter of
/// the page being replaced carries over (21-8's reopen rule). `report` is the whole journal
/// the view was read from: what `e` exports.
pub fn open_messages(
    state: &mut TuiState,
    filtered: bool,
    blocks: &[String],
    report: Option<crate::domain::services::transparency::TransparencyReport>,
) {
    let carried = state.mission_control.messages.take().map(|old| old.search);
    let mut page = LogPage::new(filtered, blocks, report);
    if let Some(search) = carried {
        page.search = search;
    }
    state.mission_control.messages_failed = None;
    state.mission_control.messages = Some(page);
    crate::adapters::tui::presentation::reducer::open_mission_control(state, McTab::Messages);
}

/// The filter field was edited or cleared: the rows it filters depend on it.
pub(crate) fn search_changed(state: &mut TuiState) {
    if state.mission_control.tab == McTab::Messages {
        if let Some(page) = state.mission_control.messages.as_mut() {
            page.invalidate();
        }
    } else {
        state.mission_control.refilter();
    }
    state.needs_redraw = true;
}

/// `Enter` in the filter field keeps the filter; a query that matches nothing says so and
/// leaves the view unchanged.
pub(crate) fn accept_search(state: &mut TuiState) {
    let mission = &mut state.mission_control;
    let Some(search) = mission.search_mut() else {
        return;
    };
    search.accept();
    let query = search.query.as_str().to_owned();
    let matched = if mission.tab == McTab::Messages {
        mission.messages.as_ref().is_none_or(LogPage::has_match)
    } else {
        mission.refilter();
        query.is_empty() || !mission.rows().is_empty()
    };
    if !matched {
        if let Some(search) = mission.search_mut() {
            search.back();
        }
        crate::adapters::tui::notifications::info(
            state,
            crate::adapters::tui::notifications::NoticeSource::Search,
            format!("Search: no match for \"{query}\""),
        );
    }
    search_changed(state);
}

/// `Esc` / `Ctrl+C` in the filter field clears it and leaves the field.
pub(crate) fn clear_search(state: &mut TuiState) {
    if let Some(search) = state.mission_control.search_mut() {
        search.back();
    }
    search_changed(state);
}

/// Mission Control is the top frame of the stack.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::MissionControl)
}

/// The filter field of the current tab is open (a text context of the frame).
#[must_use]
pub fn search_editing(state: &TuiState) -> bool {
    state
        .mission_control
        .search()
        .is_some_and(|search| search.editing)
}

/// A task of the Tasks tab is being reordered **here**: the frame resolves its keys in
/// `Context::MissionReorder` until the reorder is committed or cancelled. A reorder armed for
/// another conversation's plan (a decision switched tabs) never captures this tab's keys; the
/// host restores that order (`task_panel_bridge::settle_reorder_origin`) before the next key.
#[must_use]
pub fn reordering(state: &TuiState) -> bool {
    state.mission_control.tab == McTab::Tasks
        && state
            .task_panel_state
            .reorder
            .as_ref()
            .is_some_and(|reorder| {
                state
                    .mission_control
                    .tasks
                    .as_ref()
                    .is_some_and(|source| source.conversation_id == reorder.conversation_id)
            })
}

/// A surface that shows a plan's tasks is open (the hub on the Tasks tab, the task window, or
/// the monitor column on Tasks): a task event repaints, the running-task tick keeps the
/// elapsed times moving, and the host's draw rebuilds the rows from the active conversation.
#[must_use]
pub fn tasks_surface_open(state: &TuiState) -> bool {
    state.task_window.is_some()
        || (state.mission_control.tab == McTab::Tasks
            && state
                .modal_stack
                .all_frames()
                .any(|frame| frame.surface == Surface::MissionControl))
        || (monitor_shown(state) && state.mission_control.monitor_tab() == McTab::Tasks)
}

/// The monitor body is on screen (7E-02): the retained layout paints it where the layout
/// gives it a rectangle — the sidebar column (Monitor density, a visible column, at least
/// 120 columns) or Dashboard's panel (any width a frame fits) — until 7-F deletes both.
#[must_use]
pub fn monitor_shown(state: &TuiState) -> bool {
    !crate::adapters::tui::presentation::too_small(state)
        && (state.density_mode == crate::domain::models::visual::DensityMode::Dashboard
            || (state.sidebar_visible
                && state.terminal_width >= crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH))
}

/// Nothing paints over the monitor: no frame on the stack (frames span the terminal's full
/// width and paint after the column) and no leader menu (7E-01). A covered monitor is not a
/// presentation, so the Room under it is not polled.
fn monitor_unobscured(state: &TuiState) -> bool {
    state.modal_stack.is_empty() && state.pending_leader.is_none()
}

/// The Room is presented: the LAST COMPLETED DRAW painted its body with at least one row while
/// Mission Control was the top visible frame (no leader menu pending over it), and nothing has
/// changed what is on top since (`now` still has the hub on top, on the Room tab, no menu, in a
/// terminal big enough to paint frames). A Room held under another frame or a menu, on another
/// tab, below the floor, after an interrupted draw or after a drawn cover closed is not
/// presented and must not be polled (A14). The monitor counts when it is painted and nothing
/// covers it: the last completed draw painted the Room body in it, no frame is open and no
/// leader menu is pending, and the column is still shown on the Room tab.
#[must_use]
pub fn room_projection_presented(state: &TuiState) -> bool {
    let hub = state.mission_control.room_presented
        && is_top(state)
        && state.pending_leader.is_none()
        && state.mission_control.tab == McTab::Room
        && !crate::adapters::tui::presentation::too_small(state);
    let monitor = state.mission_control.monitor_room_presented
        && monitor_unobscured(state)
        && monitor_shown(state)
        && state.mission_control.monitor_tab() == McTab::Room;
    hub || monitor
}

impl TuiState {
    /// Before a draw: the paint facts of a draw that never completed are void and nothing is
    /// presented until a draw completes (`restore_unflushed_log_visits` calls this); the
    /// Messages visit it was about to present returns to pending.
    pub(crate) fn mission_draw_started(&mut self) {
        self.mission_control.room_paint.borrow_mut().take();
        self.mission_control.room_presented = false;
        self.mission_control.monitor_room_paint.borrow_mut().take();
        self.mission_control.monitor_room_presented = false;
        if let Some(page) = &self.mission_control.messages {
            page.clear_painted();
        }
    }

    /// After a **successful** draw: apply what the Room body painted. The viewport and the
    /// anchors always follow the paint; the acknowledgement of the folded rows only counts for
    /// a body on top of the stack (a covered Room has not been seen).
    pub fn mission_draw_completed(&mut self) {
        let hub = self.mission_control.room_paint.borrow_mut().take();
        let monitor = self.mission_control.monitor_room_paint.borrow_mut().take();
        self.mission_control.monitor_room_presented =
            monitor.as_ref().is_some_and(|paint| paint.rows >= 1) && monitor_unobscured(self);
        let Some(paint) = hub else {
            // Only the monitor painted the Room: its viewport and anchors follow, nothing is
            // acknowledged (a column glanced at is not the folded rows read).
            if let Some(paint) = monitor {
                let mut cursor = self.mission_control.room_cursor;
                self.room_panel.apply_paint(&paint, &mut cursor, false);
                self.mission_control.room_cursor = cursor;
            }
            return;
        };
        let visible = is_top(self)
            && self.pending_leader.is_none()
            && self.mission_control.tab == McTab::Room;
        let mut cursor = self.mission_control.room_cursor;
        self.room_panel.apply_paint(&paint, &mut cursor, visible);
        self.mission_control.room_cursor = cursor;
        self.mission_control.room_presented = visible && paint.rows >= 1;
    }
}

/// Source lines of a detail preview that are laid out (a bound on the work of one build).
const DETAIL_MAX_LINES: usize = 2_000;

/// The sanitized, wrapped rows of one selected row's detail preview at one width, valid while
/// the row, the width and the content revision it was built for stay the same.
#[derive(Debug)]
pub struct DetailCache {
    tab: McTab,
    id: String,
    width: usize,
    revision: u64,
    rows: std::sync::Arc<Vec<String>>,
}

impl MissionControlState {
    /// The detail preview of the selected row wrapped to `width`, cached by (tab, selected id,
    /// width, content revision): an unchanged repaint reuses the rows and clones nothing but
    /// the handle; a changed selection, width or refresh rebuilds them once.
    pub fn detail_rows(&self, width: usize) -> std::sync::Arc<Vec<String>> {
        let (id, text) = match self.resolved() {
            Resolved::Found(index) => self
                .rows()
                .get(index)
                .map_or(("", ""), |row| (row.id.as_str(), row.detail.as_str())),
            _ => ("", ""),
        };
        let revision = self.list().map_or(0, |list| list.rev);
        if let Some(cache) = self.detail_cache.borrow().as_ref()
            && cache.tab == self.tab
            && cache.id == id
            && cache.width == width
            && cache.revision == revision
        {
            return cache.rows.clone();
        }
        let safe = crate::domain::services::peer_text::sanitize_tool_text_block(text);
        let mut out = Vec::new();
        for line in safe.lines().take(DETAIL_MAX_LINES) {
            if line.is_empty() {
                out.push(String::new());
            } else {
                out.extend(
                    crate::adapters::tui::text_geometry::wrap_hard(line, width.max(1))
                        .into_iter()
                        .map(str::to_owned),
                );
            }
        }
        let rows = std::sync::Arc::new(out);
        self.detail_builds.set(self.detail_builds.get() + 1);
        *self.detail_cache.borrow_mut() = Some(DetailCache {
            tab: self.tab,
            id: id.to_owned(),
            width,
            revision,
            rows: rows.clone(),
        });
        rows
    }

    /// How many times a detail preview was laid out (the cache's witness).
    #[must_use]
    pub fn detail_builds(&self) -> u32 {
        self.detail_builds.get()
    }
}
