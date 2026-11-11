//! The Messages tab's page (Story 21.7 A12): 21-8's A2A log view, re-homed from S-THREAD.
//!
//! `/team log [args]` builds one text block per journal row (its provenance clauses included)
//! and the closing claim last (`team_command::log_blocks`); the blocks are escaped **once** here
//! (journal and export bytes stay as they are: control and bidi characters of peer-derived
//! scalars become visible escapes) and become one filterable turn each, the closing claim kept
//! under every filter. The page functions are the ones S-THREAD paints its threads with
//! (`thread_body::{shown_turns, visible_segments, build}` over a [`PageView`]); the log rows
//! are `Text` segments laid out literally, so there is no source/rendered toggle.
//!
//! Commit-on-present (19.16g, A13): the body paint sets `painted`; the post-draw hook
//! (`TuiState::log_visits_presented`) consumes it only when Messages is the top frame.

use std::cell::{Cell, Ref, RefCell};

use ratatui::text::Line;

use crate::adapters::tui::presentation::inspector_search::InspectorSearch;
use crate::adapters::tui::presentation::markdown_body::BodyMode;
use crate::adapters::tui::presentation::viewer::Viewer;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::thread_body::{self, PageView, ThreadSegment, TurnMark};
use crate::domain::services::transparency::TransparencyReport;

/// Opening title suffix of a `--filter=` view.
pub const FILTERED_SUFFIX: &str = " · filtered";

#[derive(Default)]
struct LogLayout {
    key: Option<(u16, u64)>,
    lines: Vec<Line<'static>>,
}

/// One read of the A2A log: what the Messages tab shows and what its keys act on.
pub struct LogPage {
    /// The page as the legacy literal text (tests read it; the filter paints the segments).
    pub content: String,
    pub segments: Vec<ThreadSegment>,
    pub turns: Vec<TurnMark>,
    /// A `--filter=` view (never a full visit, never the export's source).
    pub filtered: bool,
    /// The turn filter (`/`): the closing claim always stays.
    pub search: InspectorSearch,
    pub viewer: Viewer,
    /// Bumped by every change the painted body depends on besides the width.
    pub generation: u64,
    /// The whole journal report this view was read from: what `e` exports (never the filtered
    /// rows, never a fresh read).
    pub report: Option<TransparencyReport>,
    /// `(rows, viewport)` of the last paint: the row list the viewer clamps against is the one
    /// that was painted.
    fit: Cell<(usize, usize)>,
    painted: Cell<bool>,
    layout: RefCell<LogLayout>,
    layouts_built: Cell<usize>,
}

impl std::fmt::Debug for LogPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPage")
            .field("filtered", &self.filtered)
            .field("turns", &self.turns.len())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl LogPage {
    /// Build the page from the log blocks (`team_command::log_blocks`): escape once, one turn
    /// per block, the last block (the closing claim) kept under every filter.
    pub fn new(filtered: bool, blocks: &[String], report: Option<TransparencyReport>) -> Self {
        let escaped: Vec<String> = blocks
            .iter()
            .map(|block| crate::infrastructure::runtime::recipient_view::escape_disclosable(block))
            .collect();
        let (content, segments, turns) = thread_body::block_page(&escaped, true);
        Self {
            content,
            segments,
            turns,
            filtered,
            search: InspectorSearch::default(),
            viewer: Viewer::default(),
            generation: 0,
            report,
            fit: Cell::new((0, 0)),
            painted: Cell::new(false),
            layout: RefCell::new(LogLayout::default()),
            layouts_built: Cell::new(0),
        }
    }

    /// The frame's painted title.
    #[must_use]
    pub fn title(&self) -> String {
        let mut title = super::McTab::Messages.title();
        if self.filtered {
            title.push_str(FILTERED_SUFFIX);
        }
        title
    }

    /// The borrowed page the shared page functions read.
    #[must_use]
    pub fn page(&self) -> PageView<'_> {
        PageView {
            segments: &self.segments,
            turns: &self.turns,
            search: &self.search,
            mode: BodyMode::Rendered,
        }
    }

    /// Something the painted body depends on changed (the filter was edited or cleared).
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.viewer = Viewer::default();
    }

    /// The body rows for `width`: laid out once per (width, generation) and reused by every
    /// repaint (bounded per-repaint work).
    pub fn body(&self, theme: &Theme, width: u16) -> Ref<'_, [Line<'static>]> {
        let key = (width, self.generation);
        if self.layout.borrow().key != Some(key) {
            let laid = thread_body::build(self.page(), theme, usize::from(width));
            *self.layout.borrow_mut() = LogLayout {
                key: Some(key),
                lines: laid.lines,
            };
            self.layouts_built.set(self.layouts_built.get() + 1);
        }
        Ref::map(self.layout.borrow(), |layout| layout.lines.as_slice())
    }

    /// How many body layouts were computed (a repaint at the same key adds none).
    #[must_use]
    pub fn layouts_built(&self) -> usize {
        self.layouts_built.get()
    }

    #[must_use]
    pub fn fit(&self) -> (usize, usize) {
        self.fit.get()
    }

    pub fn set_fit(&self, rows: usize, viewport: usize) {
        self.fit.set((rows, viewport));
    }

    /// The paint showed the body with at least one row.
    pub fn mark_painted(&self) {
        self.painted.set(true);
    }

    pub(crate) fn clear_painted(&self) {
        self.painted.set(false);
    }

    /// Consume the paint mark: `true` once per painted body of an unfiltered view.
    pub(crate) fn take_painted(&self) -> bool {
        let painted = self.painted.take();
        painted && !self.filtered
    }

    /// Whether the filter matches anything of the page.
    #[must_use]
    pub fn has_match(&self) -> bool {
        thread_body::has_match(&self.segments, &self.search)
    }
}
