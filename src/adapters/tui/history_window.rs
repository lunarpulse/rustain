//! The History window (Story 21.7 A21, AC20): the saved conversations in a frame of their own
//! at every width. `j k` and the RU-25 scroll keys move the selection, `Enter` opens the
//! selected conversation, `d` asks to delete it (a decision), `/` searches across the
//! conversations, `Esc` goes back.
//!
//! The rows are a **snapshot of the in-memory session index** ([`HistoryWindowState::sync`]):
//! it is read again only when the index changed ([`SessionIndex::revision`], which every
//! mutation site of the event loop moves, the delete-all clear included) or when the minute the
//! ages were written for passed. There is no storage read behind the window and none at draw.
//!
//! The selection is a conversation **id**, never an index (A3): after a refresh the id is looked
//! up, and a conversation that left the list shows a muted `<title> is no longer available` row
//! at its old position instead of being rebound to its neighbour. Every action carries that id,
//! so `d` and `Enter` act on the conversation that was selected even after the list changed
//! underneath.

use std::cell::Cell;

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::mission_control::tabs::{self, McRow, Selection};
use crate::adapters::tui::presentation::viewer::ViewerCmd;
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::domain::services::peer_text::sanitize_display_line;
use crate::domain::services::session_index::{SessionIndex, SessionSummary};

/// The title a conversation without one is listed under.
pub const UNTITLED: &str = "(Untitled)";

/// What the last paint measured: the rows and the viewport the keys clamp against, and which
/// keyed hints the rail painted whole (a key whose hint was not painted is inert).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fit {
    /// Rows of the painted list (the "no longer available" placeholder included).
    pub rows: usize,
    pub viewport: usize,
    /// First painted row.
    pub top: usize,
    pub open: bool,
    pub delete: bool,
    pub search: bool,
}

#[derive(Debug)]
pub struct HistoryWindowState {
    sessions: Vec<SessionSummary>,
    /// One row per session: the id and the sanitized title (the placeholder's words).
    rows: Vec<McRow>,
    selection: Selection,
    /// The [`SessionIndex::revision`] the snapshot was taken at.
    revision: u64,
    /// The `now` the ages were written for.
    now: i64,
    builds: u32,
    fit: Cell<Fit>,
}

impl HistoryWindowState {
    /// A snapshot of `index` with its ages written for `now` (unix seconds).
    #[must_use]
    pub fn new(index: &SessionIndex, now: i64) -> Self {
        let mut window = Self {
            sessions: Vec::new(),
            rows: Vec::new(),
            selection: Selection::default(),
            revision: index.revision(),
            now,
            builds: 0,
            fit: Cell::new(Fit::default()),
        };
        window.replace(index, now);
        window
    }

    fn replace(&mut self, index: &SessionIndex, now: i64) {
        // Reconcile the selection against the rows it pointed into *before* they are
        // replaced, so a removed row leaves its position and label behind for the placeholder.
        if let Some(at) = self
            .selection
            .id
            .as_ref()
            .and_then(|id| self.rows.iter().position(|row| row.id == *id))
        {
            self.selection.at = at;
            self.selection.label = self.rows[at].text.clone();
        }
        self.sessions = index.entries().to_vec();
        self.rows = self
            .sessions
            .iter()
            .map(|session| McRow::new(&session.conversation_id, &label(session), ""))
            .collect();
        self.revision = index.revision();
        self.now = now;
        self.builds += 1;
    }

    /// Read the index again when it changed (or the minute the ages are written for did):
    /// returns whether the snapshot was replaced. An unchanged index costs two comparisons.
    pub fn sync(&mut self, index: &SessionIndex, now: i64) -> bool {
        if self.revision == index.revision() && self.now.div_euclid(60) == now.div_euclid(60) {
            return false;
        }
        self.replace(index, now);
        true
    }

    /// Drop the selection (the bulk delete cleared every conversation it could name).
    pub fn forget_selection(&mut self) {
        self.selection = Selection::default();
    }

    /// How many times the snapshot was taken (an unchanged repaint adds none).
    #[must_use]
    pub fn builds(&self) -> u32 {
        self.builds
    }

    /// The session summaries of the snapshot, newest first.
    #[must_use]
    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    /// The `now` the ages of the snapshot are written for.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.now
    }

    #[must_use]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// How the selection resolves against the snapshot now.
    #[must_use]
    pub fn resolved(&self) -> tabs::Resolved {
        tabs::resolve(&self.rows, &self.selection)
    }

    /// The id of the selected conversation, if it is still in the snapshot (nothing selected
    /// yet means the first row; a placeholder or an empty list means none).
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        match self.resolved() {
            tabs::Resolved::Found(index) => self.rows.get(index).map(|row| row.id.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub fn fit(&self) -> Fit {
        self.fit.get()
    }

    pub fn set_fit(&self, fit: Fit) {
        self.fit.set(fit);
    }

    /// Move the selection by a viewer command (the RU-25 rows of `InspectorBody`).
    pub fn move_selection(&mut self, command: ViewerCmd) {
        use ViewerCmd::{HalfDown, HalfUp, LineDown, LineUp, PageDown, PageUp, Tail, Top};
        let resolved = tabs::resolve(&self.rows, &self.selection);
        let len = tabs::view_len(self.rows.len(), resolved);
        if len == 0 {
            return;
        }
        let pos = tabs::view_pos(resolved);
        let viewport = self.fit.get().viewport.max(1);
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
        if let Some(index) = tabs::target(self.rows.len(), resolved, to) {
            tabs::select(&mut self.selection, &self.rows, index);
        }
    }

    pub(crate) fn rows(&self) -> &[McRow] {
        &self.rows
    }
}

/// The sanitized title a row is listed (and its placeholder named) under.
fn label(session: &SessionSummary) -> String {
    let title = sanitize_display_line(&session.title);
    if title.is_empty() {
        UNTITLED.to_owned()
    } else {
        title
    }
}

/// Open (or refresh) the History window from the session index. A window that is already open
/// keeps its selection and is only brought up to date.
pub fn open(state: &mut TuiState, index: &SessionIndex, now: i64) {
    let window = match state.history_window.take() {
        Some(mut window) => {
            window.sync(index, now);
            window
        }
        None => HistoryWindowState::new(index, now),
    };
    reducer::open_history(state, window);
}

/// Before a draw: bring an open window up to date with the index.
pub fn sync(state: &mut TuiState, index: &SessionIndex, now: i64) {
    if let Some(window) = state.history_window.as_mut() {
        window.sync(index, now);
    }
}

/// The History window is the top frame.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::History)
}

pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    if !is_top(state) {
        return InputAction::Consumed;
    }
    let Resolved::Hit(row) = keymap::resolve(Context::History, host, &[key]) else {
        return InputAction::Consumed;
    };
    state.needs_redraw = true;
    let Some(window) = state.history_window.as_mut() else {
        return InputAction::Consumed;
    };
    let fit = window.fit();
    match row.command {
        CommandId::Mission(MissionCmd::Back) => {
            reducer::back(state);
            InputAction::Consumed
        }
        CommandId::Viewer(command) if !command.horizontal() => {
            window.move_selection(command);
            InputAction::Consumed
        }
        CommandId::Mission(MissionCmd::HistoryOpen) if fit.open => {
            window.selected_id().map_or(InputAction::Consumed, |id| {
                InputAction::OpenHistoryEntry(id.to_owned())
            })
        }
        CommandId::Mission(MissionCmd::HistoryDelete) if fit.delete => {
            window.selected_id().map_or(InputAction::Consumed, |id| {
                InputAction::DeleteHistoryEntry(id.to_owned())
            })
        }
        // CrossSearch opens above History and returns to it on `Esc`.
        CommandId::Mission(MissionCmd::HistorySearch) if fit.search => {
            reducer::open(state, Surface::CrossSearch);
            InputAction::OpenCrossSearch
        }
        _ => InputAction::Consumed,
    }
}

/// A wheel step moves the selection like the line keys (state-only).
pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    use crate::domain::models::view_state::ScrollDelta;
    let (command, times) = match delta {
        ScrollDelta::LineUp => (ViewerCmd::LineUp, 1),
        ScrollDelta::LineDown => (ViewerCmd::LineDown, 1),
        ScrollDelta::WheelUp(lines) => (ViewerCmd::LineUp, usize::from(lines)),
        ScrollDelta::WheelDown(lines) => (ViewerCmd::LineDown, usize::from(lines)),
        ScrollDelta::HalfPageUp => (ViewerCmd::HalfUp, 1),
        ScrollDelta::HalfPageDown => (ViewerCmd::HalfDown, 1),
        ScrollDelta::FullPageUp => (ViewerCmd::PageUp, 1),
        ScrollDelta::FullPageDown => (ViewerCmd::PageDown, 1),
        _ => return,
    };
    if let Some(window) = state.history_window.as_mut() {
        for _ in 0..times {
            window.move_selection(command);
        }
        state.needs_redraw = true;
    }
}

/// The raw press-only rule of the frame (RU-29): only a plain Press acts, and a Ctrl letter
/// only where the registry binds it.
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if !is_top(state) {
        return false;
    }
    if key.kind != KeyEventKind::Press {
        return true;
    }
    match key.code {
        KeyCode::Char(c) => {
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            !plain && !(key.modifiers == KeyModifiers::CONTROL && "xpqcdu".contains(c))
        }
        _ => !key.modifiers.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ConversationSummary;

    fn index(ids: &[(&str, &str, i64)]) -> SessionIndex {
        SessionIndex::build(
            ids.iter()
                .map(|(id, title, updated)| ConversationSummary {
                    id: (*id).to_owned(),
                    title: (*title).to_owned(),
                    created_at: 0,
                    updated_at: *updated,
                    message_count: 1,
                    has_fork_source: false,
                })
                .collect(),
        )
    }

    #[test]
    fn story21_7_the_selection_is_an_id_and_a_removed_row_leaves_a_placeholder() {
        let mut live = index(&[("a", "Alpha", 3), ("b", "Beta", 2), ("c", "Gamma", 1)]);
        let mut window = HistoryWindowState::new(&live, 1_000);
        window.move_selection(ViewerCmd::LineDown);
        assert_eq!(window.selected_id(), Some("b"));
        // A new conversation lands first: `b` is now row 2, still the selected id.
        live.touch("new", Some("New".to_owned()), Some(1));
        assert!(window.sync(&live, 1_000));
        assert_eq!(window.selected_id(), Some("b"));
        // `b` leaves: nothing is selected, the placeholder keeps its old position.
        live.remove("b");
        window.sync(&live, 1_000);
        assert_eq!(window.selected_id(), None);
        assert!(matches!(window.resolved(), tabs::Resolved::Gone(2)));
        assert_eq!(window.selection().label, "Beta");
    }

    #[test]
    fn story21_7_an_unchanged_index_is_not_read_again_and_the_minute_rewrites_the_ages() {
        let live = index(&[("a", "Alpha", 3)]);
        let mut window = HistoryWindowState::new(&live, 1_000);
        assert!(!window.sync(&live, 1_000));
        assert!(!window.sync(&live, 1_000 + 30 - 1_000 % 60));
        assert_eq!(window.builds(), 1);
        assert!(window.sync(&live, 1_000 + 120));
        assert_eq!(window.builds(), 2);
    }

    #[test]
    fn story21_7_titles_are_sanitized_and_an_empty_one_is_untitled() {
        let live = index(&[("a", "\u{1b}[31mred\u{202e}\nline", 2), ("b", "", 1)]);
        let window = HistoryWindowState::new(&live, 0);
        assert!(!window.rows()[0].text.contains('\u{1b}'));
        assert!(!window.rows()[0].text.contains('\u{202e}'));
        assert!(!window.rows()[0].text.contains('\n'));
        assert_eq!(window.rows()[1].text, UNTITLED);
    }
}
