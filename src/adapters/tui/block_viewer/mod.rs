//! S-BLOCK (Story 21.13): frame-owned inspection of one code block or table.
//! The Stage stays a read-only projection: this module never touches its caches.

use std::collections::BTreeMap;
use std::ops::Range;

use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};

use crate::adapters::tui::{
    app::InputAction,
    keymap::{
        self, CommandId, Context, Host, Key, Resolved,
        table_block::{BlockCmd, BodyCmd},
        table_overlay::OverlayCmd,
    },
    markdown::{self, BlockKind, BlockMode, BlockRender, RenderOptions},
    presentation::{
        Surface,
        markdown_body::{self, BodyMode},
        reducer,
        viewer::{self, RowOrigin, Viewer, ViewerCmd, ViewerSearch},
    },
    state::TuiState,
    theme::Theme,
};
use crate::domain::models::{
    Conversation, PartId, Turn, TurnId, TurnPart,
    turn::{block_ordinal_of, is_block_item_id, is_reasoning_item_id, reasoning_part_of},
    view_state::ScrollDelta,
};
use crate::domain::services::reducer::ReducerState;

/// The reducer alone creates/releases entries, including suspended frames.
pub type BlockViewers = BTreeMap<u64, BlockViewerState>;

/// Payload identity; the reducer allocates the matching frame id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockFrameKey(pub u64);

/// What a block frame shows (Story 21.9 D8): one code block or table of a prose
/// part, or the whole text of a reasoning part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSubject {
    Block { ordinal: usize },
    Reasoning,
}

/// A Stage-bound identity (D9): its turn, the part and what of it is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTarget {
    pub conversation_id: String,
    pub turn_id: TurnId,
    pub part_id: PartId,
    pub subject: BlockSubject,
}

/// Where the block sits among the inspectable blocks of its turn.
#[derive(Debug, Clone)]
pub struct BlockSummary {
    pub kind: BlockKind,
    pub index: usize,
    pub total: usize,
}

/// Identity of the prepared rows: canonical parts revision, text length, mode,
/// and (only while wrapping) the width and gutter the wrap was cut for.
type Identity = (u64, usize, BlockMode, Option<(usize, bool)>);

#[derive(Debug)]
pub struct BlockViewerState {
    pub target: BlockTarget,
    pub mode: BlockMode,
    /// Source line numbers in a gutter (code; a table only in Source mode).
    pub numbers: bool,
    /// Code wrapped at the viewport width (D13); off by default.
    pub wrap: bool,
    pub viewer: Viewer,
    pub missing: bool,
    pub summary: Option<BlockSummary>,
    /// The frame title of a reasoning subject (`Thinking · 4.2s · 1.3k tokens`).
    pub heading: Option<String>,
    /// Rows that scroll sideways (`h_off` applies); `None` = every row (blocks).
    pub wide_rows: Option<Vec<Range<usize>>>,
    pub rows: Vec<Line<'static>>,
    pub row_widths: Vec<usize>,
    /// 1-based source line of each row (`None` for chrome and continuations).
    pub source_line: Vec<Option<usize>>,
    pub widest: usize,
    pub search: ViewerSearch,
    pub viewport: usize,
    pub cols: usize,
    /// Digits of the last source line (the gutter's number width).
    digits: usize,
    /// A toggle changed what the rows will be; `prepare` has not rebuilt them yet.
    dirty: bool,
    /// Horizontal keys issued while `dirty`, applied after the rebuild.
    pending_horizontal: Vec<ViewerCmd>,
    prepared: Option<Identity>,
    searched: Option<Identity>,
    /// Rows that hold a search match, pre-styled once per commit or step (never
    /// per paint); every other row is painted straight from `rows`.
    marked: BTreeMap<usize, Line<'static>>,
}

impl BlockViewerState {
    pub fn new(target: BlockTarget) -> Self {
        Self {
            target,
            mode: BlockMode::Rendered,
            numbers: false,
            wrap: false,
            viewer: Viewer::default(),
            missing: false,
            summary: None,
            heading: None,
            wide_rows: None,
            rows: Vec::new(),
            row_widths: Vec::new(),
            source_line: Vec::new(),
            widest: 0,
            search: ViewerSearch::default(),
            viewport: 1,
            cols: 1,
            digits: 1,
            dirty: false,
            pending_horizontal: Vec::new(),
            prepared: None,
            searched: None,
            marked: BTreeMap::new(),
        }
    }

    fn is_code(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|summary| matches!(summary.kind, BlockKind::Code { .. }))
    }

    /// The frame shows a reasoning part: no `#`, `w`, `[`, `]` apply.
    pub fn is_reasoning(&self) -> bool {
        self.target.subject == BlockSubject::Reasoning
    }

    /// The frame title once prepared (`None` before, or when missing).
    pub fn frame_title(&self) -> Option<String> {
        if self.missing {
            return None;
        }
        match self.target.subject {
            BlockSubject::Block { .. } => self.summary.as_ref().map(title),
            BlockSubject::Reasoning => self.heading.clone(),
        }
    }

    /// The horizontal offset row `index` is painted at: reasoning prose rows
    /// never move, only the scrollable rows do.
    pub fn row_h_off(&self, index: usize) -> usize {
        let off = self
            .viewer
            .h_off
            .min(self.widest.saturating_sub(self.content_cols()));
        match &self.wide_rows {
            Some(wide) if !markdown_body::is_wide_row(wide, index) => 0,
            _ => off,
        }
    }

    /// `#`: code in both modes; a table only as source (rendered table rows do
    /// not map one to one onto source lines). One rule for the key and the hint.
    pub fn applies_numbers(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|summary| match summary.kind {
                BlockKind::Code { .. } => true,
                BlockKind::Table { .. } => self.mode == BlockMode::Source,
            })
    }

    /// `w`: code only.
    pub fn applies_wrap(&self) -> bool {
        self.is_code()
    }

    /// Code is cut at the viewport only while wrap is on (and for code).
    pub fn wrapped(&self) -> bool {
        self.wrap && self.is_code()
    }

    pub fn has_previous(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|summary| summary.index > 1)
    }

    pub fn has_next(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|summary| summary.index < summary.total)
    }

    /// Cells the line-number gutter takes left of the body (0 when off or when
    /// it would leave no room for content).
    pub fn gutter(&self) -> usize {
        let width = self.digits + 3;
        if self.numbers && self.applies_numbers() && width < self.cols {
            width
        } else {
            0
        }
    }

    /// Cells left for block content beside the gutter.
    pub fn content_cols(&self) -> usize {
        self.cols.saturating_sub(self.gutter()).max(1)
    }

    /// The gutter text of row `index`: right-aligned source line, then ` │ `.
    pub fn gutter_text(&self, index: usize) -> String {
        let number = self
            .source_line
            .get(index)
            .copied()
            .flatten()
            .map(|n| n.to_string())
            .unwrap_or_default();
        format!("{number:>width$} │ ", width = self.digits)
    }

    /// Resolve the target against its owning turns; only a changed source, mode,
    /// wrap cut or gutter re-renders, so an unchanged repaint does no
    /// block-sized work.
    pub fn prepare(
        &mut self,
        turns: &[Turn],
        open_turn: Option<&Turn>,
        cols: usize,
        viewport: usize,
        theme: &Theme,
    ) {
        self.cols = cols;
        self.viewport = viewport;
        let Some((turn, text)) = resolve(turns, open_turn, &self.target) else {
            self.missing = true;
            return;
        };
        let wrap = match self.target.subject {
            BlockSubject::Block { .. } => self.wrap.then_some((cols, self.numbers)),
            // A reasoning body wraps at the viewport: its rows depend on the width.
            BlockSubject::Reasoning => Some((cols, false)),
        };
        let identity = (turn.parts.revision(), text.len(), self.mode, wrap);
        if self.prepared != Some(identity) {
            let built = match self.target.subject {
                BlockSubject::Block { ordinal } => self.build_block(turn, text, ordinal, theme),
                BlockSubject::Reasoning => self.build_reasoning(turn, text, theme),
            };
            if !built {
                return;
            }
            self.prepared = Some(identity);
            self.searched = None;
        }
        self.missing = false;
        let maximum = self.rows.len().saturating_sub(self.viewport);
        self.viewer.top = if self.viewer.follow {
            maximum
        } else {
            self.viewer.top.min(maximum)
        };
        self.viewer.h_off = self
            .viewer
            .h_off
            .min(self.widest.saturating_sub(self.content_cols()));
        if !self.search.query().is_empty() && self.searched != self.prepared {
            self.project_search(theme);
            // The rows changed under an open search: show its focused match again.
            self.search
                .jump(&mut self.viewer, self.rows.len(), self.viewport);
            self.reveal_match();
        }
        // Horizontal keys that arrived after a toggle act on the rebuilt rows.
        self.dirty = false;
        for command in std::mem::take(&mut self.pending_horizontal) {
            if self.wide() {
                let (widest, cols) = (self.widest, self.content_cols());
                viewer::apply_horizontal(&mut self.viewer, command, widest, cols);
            }
        }
    }

    /// Rows of one code block or table; `false` (and `missing`) when the
    /// ordinal no longer exists.
    fn build_block(&mut self, turn: &Turn, text: &str, ordinal: usize, theme: &Theme) -> bool {
        // The wrap width depends on the gutter, whose width depends on the
        // last line number: cut once, and again only if that width moved.
        let mut view = None;
        for _ in 0..2 {
            let render = BlockRender {
                mode: self.mode,
                wrap: self.wrap.then(|| self.wrap_width()),
            };
            let Some(rendered) = markdown::render_block(text, ordinal, theme, &render) else {
                self.missing = true;
                return false;
            };
            let digits = rendered
                .source_line
                .iter()
                .flatten()
                .max()
                .map_or(1, |last| last.to_string().len());
            let settled = digits == self.digits;
            self.digits = digits;
            view = Some(rendered);
            if settled || !(self.wrap && self.numbers) {
                break;
            }
        }
        let view = view.expect("one render ran");
        self.row_widths = view.rows.iter().map(Line::width).collect();
        self.widest = view.cells;
        self.rows = view.rows;
        self.source_line = view.source_line;
        self.summary = Some(summarize(turn, &self.target, ordinal, view.kind));
        self.heading = None;
        self.wide_rows = None;
        true
    }

    /// Rows of a reasoning part: the sanitized display projection through the
    /// one markdown body layout (AD-2116: origin safety exactly once).
    fn build_reasoning(&mut self, turn: &Turn, text: &str, theme: &Theme) -> bool {
        let display = crate::domain::services::peer_text::sanitize_tool_text_block(text);
        let mode = match self.mode {
            BlockMode::Rendered => BodyMode::Rendered,
            BlockMode::Source => BodyMode::Source,
        };
        let body = markdown_body::layout(&display, self.cols, theme, mode, false);
        self.row_widths = body.lines.iter().map(Line::width).collect();
        self.widest = body.widest;
        self.source_line = vec![None; body.lines.len()];
        self.rows = body.lines;
        self.wide_rows = Some(body.wide);
        self.digits = 1;
        self.summary = None;
        let (duration_ms, tokens) = turn
            .parts
            .iter()
            .find_map(|part| match part {
                TurnPart::Reasoning {
                    id,
                    duration_ms,
                    tokens,
                    ..
                } if *id == self.target.part_id => Some((*duration_ms, *tokens)),
                _ => None,
            })
            .unwrap_or((None, None));
        self.heading = Some(format!(
            "Thinking · {}",
            crate::adapters::tui::widgets::chat_pane::reasoning::facts(text, duration_ms, tokens)
        ));
        true
    }

    /// Content width a wrapped code row is cut at: the viewport minus the
    /// gutter, and minus the box borders when the box is painted.
    fn wrap_width(&self) -> usize {
        let gutter = if self.numbers { self.digits + 3 } else { 0 };
        let boxed = if self.mode == BlockMode::Rendered {
            2
        } else {
            0
        };
        self.cols.saturating_sub(gutter + boxed).max(1)
    }

    /// Search matches the displayed rows: each row is one logical line.
    fn project_search(&mut self, theme: &Theme) {
        let text = self
            .rows
            .iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let origins: Vec<Option<RowOrigin>> = text
            .iter()
            .enumerate()
            .map(|(logical_line, row)| {
                Some(RowOrigin {
                    logical_line,
                    bytes: 0..row.len(),
                })
            })
            .collect();
        self.search
            .project(&text.join("\n"), &origins, self.viewer.top);
        self.searched = self.prepared;
        self.restyle(theme);
    }

    pub(crate) fn accept_search(&mut self, theme: &Theme) {
        self.search.editor.accept();
        let query = self.search.editor.query.as_str().to_owned();
        self.search.commit(query);
        self.project_search(theme);
        self.search
            .jump(&mut self.viewer, self.rows.len(), self.viewport);
        self.reveal_match();
    }

    pub(crate) fn clear_search(&mut self) {
        self.search.clear();
        self.searched = None;
        self.marked.clear();
    }

    /// `n`/`N`: move the focused match, restyle and scroll it into view.
    pub(crate) fn step_match(&mut self, forward: bool, theme: &Theme) {
        self.search
            .step(&mut self.viewer, forward, self.rows.len(), self.viewport);
        self.restyle(theme);
        self.reveal_match();
    }

    /// Every cell of the displayed rows that a horizontal key could reveal.
    pub fn wide(&self) -> bool {
        !self.wrapped() && self.widest > self.content_cols()
    }

    /// The row to paint: pre-styled when it holds a match, else borrowed as is.
    pub fn painted(&self, index: usize) -> &Line<'static> {
        self.marked.get(&index).unwrap_or(&self.rows[index])
    }

    /// Scroll horizontally just enough that the focused match's first cell shows
    /// (clear of the `←`/`→` edge cells).
    fn reveal_match(&mut self) {
        let Some(found) = self.search.focused.and_then(|i| self.search.matches.get(i)) else {
            return;
        };
        let (line, start) = (found.logical_line, found.bytes.start);
        // Prose rows of a reasoning body never move sideways.
        if let Some(wide) = &self.wide_rows
            && !markdown_body::is_wide_row(wide, line)
        {
            return;
        }
        let mut column = 0usize;
        let mut at = 0usize;
        'spans: for span in &self.rows[line].spans {
            let text = span.content.as_ref();
            if start < at + text.len() {
                column += crate::adapters::tui::text_geometry::cells(&text[..start - at]);
                break 'spans;
            }
            column += crate::adapters::tui::text_geometry::cells(text);
            at += text.len();
        }
        let inner = self.content_cols().saturating_sub(2).max(1);
        let maximum = self.widest.saturating_sub(self.content_cols());
        if column < self.viewer.h_off + 1 || column >= self.viewer.h_off + 1 + inner {
            self.viewer.h_off = column.saturating_sub(1).min(maximum);
        }
    }

    /// Pre-style the rows that hold matches in one grapheme pass per row (a
    /// grapheme touched by any match is styled whole); never per paint.
    fn restyle(&mut self, theme: &Theme) {
        use unicode_segmentation::UnicodeSegmentation;
        self.marked.clear();
        let matches = &self.search.matches;
        let mut cursor = 0;
        while cursor < matches.len() {
            let index = matches[cursor].logical_line;
            let hi = cursor + matches[cursor..].partition_point(|m| m.logical_line <= index);
            let mut spans: Vec<Span<'static>> = Vec::new();
            let (mut next, mut at) = (cursor, 0usize);
            for span in &self.rows[index].spans {
                let text = span.content.as_ref();
                let style_of = |mark: Option<bool>| match mark {
                    None => span.style,
                    Some(true) => span.style.patch(theme.search_highlight_focused),
                    Some(false) => span.style.patch(theme.search_highlight),
                };
                let (mut run, mut run_mark) = (0usize, None);
                for (offset, grapheme) in text.grapheme_indices(true) {
                    let (from, to) = (at + offset, at + offset + grapheme.len());
                    while next < hi && matches[next].bytes.end <= from {
                        next += 1;
                    }
                    let mark = (next < hi && matches[next].bytes.start < to)
                        .then(|| self.search.focused == Some(next));
                    if mark != run_mark {
                        if offset > run {
                            spans.push(Span::styled(
                                text[run..offset].to_owned(),
                                style_of(run_mark),
                            ));
                        }
                        (run, run_mark) = (offset, mark);
                    }
                }
                if run < text.len() {
                    spans.push(Span::styled(text[run..].to_owned(), style_of(run_mark)));
                }
                at += text.len();
            }
            self.marked.insert(index, Line::from(spans));
            cursor = hi;
        }
    }
}

fn resolve<'a>(
    turns: &'a [Turn],
    open_turn: Option<&'a Turn>,
    target: &BlockTarget,
) -> Option<(&'a Turn, &'a str)> {
    let turn = open_turn
        .into_iter()
        .chain(turns)
        .find(|turn| turn.id == target.turn_id)?;
    turn.parts
        .iter()
        .find_map(|part| match (part, target.subject) {
            (TurnPart::Prose { id, text }, BlockSubject::Block { .. }) if *id == target.part_id => {
                Some((turn, text.as_str()))
            }
            (TurnPart::Reasoning { id, text, .. }, BlockSubject::Reasoning)
                if *id == target.part_id =>
            {
                Some((turn, text.as_str()))
            }
            _ => None,
        })
}

fn summarize(turn: &Turn, target: &BlockTarget, ordinal: usize, kind: BlockKind) -> BlockSummary {
    let (mut before, mut total) = (0, 0);
    for part in &turn.parts {
        if let TurnPart::Prose { id, text } = part {
            let count = markdown::block_count(text);
            if *id == target.part_id {
                before = total;
            }
            total += count;
        }
    }
    BlockSummary {
        kind,
        index: before + ordinal + 1,
        total,
    }
}

/// An info-string label safe to paint: control and escape sequences removed.
pub fn safe_language(language: &str) -> String {
    crate::domain::services::peer_text::sanitize_tool_text_block(language).replace('\n', " ")
}

/// `Block · rust #2`: the code language (sanitized), `code` or `table`.
fn title(summary: &BlockSummary) -> String {
    let label = match &summary.kind {
        BlockKind::Code {
            language: Some(language),
            ..
        } => safe_language(language),
        BlockKind::Code { language: None, .. } => "code".to_owned(),
        BlockKind::Table { .. } => "table".to_owned(),
    };
    format!("Block · {label} #{}", summary.index)
}

/// The block or reasoning target a Stage item id names in the live conversation.
fn target_of(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    item: &str,
) -> Option<BlockTarget> {
    open_turn
        .into_iter()
        .chain(&conversation.turns)
        .find_map(|turn| {
            turn.parts.iter().find_map(|part| match part {
                TurnPart::Prose { id, .. } => {
                    block_ordinal_of(&turn.id, *id, item).map(|ordinal| BlockTarget {
                        conversation_id: conversation.id.clone(),
                        turn_id: turn.id.clone(),
                        part_id: *id,
                        subject: BlockSubject::Block { ordinal },
                    })
                }
                TurnPart::Reasoning { id, .. } => {
                    reasoning_part_of(&turn.id, *id, item).then(|| BlockTarget {
                        conversation_id: conversation.id.clone(),
                        turn_id: turn.id.clone(),
                        part_id: *id,
                        subject: BlockSubject::Reasoning,
                    })
                }
                _ => None,
            })
        })
}

/// Presence-only discovery for the Stage hint.
pub fn contains_block(turns: &[Turn], open_turn: Option<&Turn>, item: &str) -> bool {
    open_turn.into_iter().chain(turns).any(|turn| {
        turn.parts.iter().any(|part| {
            matches!(part, TurnPart::Prose { id, .. }
                if block_ordinal_of(&turn.id, *id, item).is_some())
        })
    })
}

/// Presence-only discovery of a reasoning part for the Stage hint.
pub fn contains_reasoning(turns: &[Turn], open_turn: Option<&Turn>, item: &str) -> bool {
    open_turn.into_iter().chain(turns).any(|turn| {
        turn.parts.iter().any(|part| {
            matches!(part, TurnPart::Reasoning { id, .. }
                if reasoning_part_of(&turn.id, *id, item))
        })
    })
}

/// Exact original text of a reasoning Stage id (Chat `c` with a selected
/// reasoning item): the bytes as received, never the display projection.
/// All or nothing like the window's copy: above `COPY_MAX_BYTES` it is `None`
/// and nothing is cloned (review P6).
pub fn reasoning_source_of(turns: &[Turn], open_turn: Option<&Turn>, item: &str) -> Option<String> {
    let limit = crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
    open_turn.into_iter().chain(turns).find_map(|turn| {
        turn.parts.iter().find_map(|part| match part {
            TurnPart::Reasoning { id, text, .. }
                if reasoning_part_of(&turn.id, *id, item) && text.len() <= limit =>
            {
                Some(text.clone())
            }
            _ => None,
        })
    })
}

/// Exact source of a Stage block id (Chat `c` with a selected block, D8).
pub fn source_of(
    turns: &[Turn],
    open_turn: Option<&Turn>,
    item: &str,
    theme: &Theme,
) -> Option<String> {
    open_turn.into_iter().chain(turns).find_map(|turn| {
        turn.parts.iter().find_map(|part| match part {
            TurnPart::Prose { id, text } => {
                let ordinal = block_ordinal_of(&turn.id, *id, item)?;
                let render = BlockRender {
                    mode: BlockMode::Rendered,
                    wrap: None,
                };
                markdown::render_block(text, ordinal, theme, &render).map(|view| view.source)
            }
            _ => None,
        })
    })
}

/// Key-time copy of the top frame's block from the owning canonical parts: no
/// resident second copy of the payload. All-or-nothing and bounded like S-TOOL.
pub fn copy_source(
    state: &mut TuiState,
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    copy: &dyn Fn(&str) -> crate::adapters::tui::clipboard::ClipboardResult,
) -> bool {
    use crate::adapters::tui::handlers::clipboard::{notify_copy, notify_failure};
    let Some(Surface::BlockViewer(BlockFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return false;
    };
    let Some(block) = state.block_viewers.get(&id) else {
        return false;
    };
    if block.target.conversation_id != conversation.id {
        return false;
    }
    let target = block.target.clone();
    let Some((_, text)) = resolve(&conversation.turns, open_turn, &target) else {
        return false;
    };
    // Bound the work first: above the limit no block (sources, styled tree,
    // rows) is ever built for a copy.
    let limit = crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
    let ordinal = match target.subject {
        BlockSubject::Block { ordinal } => ordinal,
        // A reasoning frame copies the original text, all or nothing.
        BlockSubject::Reasoning => {
            if text.len() > limit {
                notify_failure(state, "reasoning text exceeds the 8 MiB copy limit");
            } else {
                notify_copy(state, text, copy);
            }
            return true;
        }
    };
    match markdown::block_source_len(text, ordinal) {
        None => return false,
        Some(len) if len > limit => {
            notify_failure(state, "block source exceeds the 8 MiB copy limit");
            return true;
        }
        Some(_) => {}
    }
    let render = BlockRender {
        mode: BlockMode::Rendered,
        wrap: None,
    };
    let Some(view) = markdown::render_block(text, ordinal, &state.theme, &render) else {
        return false;
    };
    notify_copy(state, &view.source, copy);
    true
}

/// Move the open viewer to the previous/next inspectable block of the same
/// turn (D14): part order, then ordinal; never another turn. The frame keeps
/// its id and depth, and its mode, numbers and wrap; position and search reset.
pub fn step(
    state: &mut TuiState,
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    forward: bool,
) -> bool {
    let Some(Surface::BlockViewer(BlockFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return false;
    };
    let current = &state.block_viewers[&id];
    if current.target.conversation_id != conversation.id || current.is_reasoning() {
        return false;
    }
    let Some(turn) = open_turn
        .into_iter()
        .chain(&conversation.turns)
        .find(|turn| turn.id == current.target.turn_id)
    else {
        return false;
    };
    let mut blocks = Vec::new();
    for part in &turn.parts {
        if let TurnPart::Prose { id, text } = part {
            blocks.extend((0..markdown::block_count(text)).map(|ordinal| (*id, ordinal)));
        }
    }
    let current_ordinal = match current.target.subject {
        BlockSubject::Block { ordinal } => ordinal,
        BlockSubject::Reasoning => return false,
    };
    let Some(at) = blocks
        .iter()
        .position(|&(part, ordinal)| part == current.target.part_id && ordinal == current_ordinal)
    else {
        return false;
    };
    let Some(&(part_id, ordinal)) = (if forward {
        blocks.get(at + 1)
    } else {
        at.checked_sub(1).and_then(|before| blocks.get(before))
    }) else {
        return false;
    };
    let mut replacement = BlockViewerState::new(BlockTarget {
        conversation_id: conversation.id.clone(),
        turn_id: turn.id.clone(),
        part_id,
        subject: BlockSubject::Block { ordinal },
    });
    (replacement.mode, replacement.numbers, replacement.wrap) =
        (current.mode, current.numbers, current.wrap);
    let (cols, viewport) = (current.cols, current.viewport);
    replacement.prepare(&conversation.turns, open_turn, cols, viewport, &state.theme);
    let Some(summary) = replacement.summary.clone().filter(|_| !replacement.missing) else {
        return false;
    };
    reducer::retarget_block(state, id, replacement, title(&summary))
}

/// Discovery facts use the live conversation, never a stale copy selection.
pub fn stage_target(
    state: &TuiState,
    conversation: &Conversation,
    open_turn: Option<&Turn>,
) -> bool {
    match state.focused_item_id.as_deref() {
        Some(id) if is_block_item_id(id) => {
            state.focus == crate::domain::models::FocusState::Chat
                && state.modal_stack.is_empty()
                && contains_block(&conversation.turns, open_turn, id)
        }
        Some(id) if is_reasoning_item_id(id) => {
            state.focus == crate::domain::models::FocusState::Chat
                && state.modal_stack.is_empty()
                && contains_reasoning(&conversation.turns, open_turn, id)
        }
        _ => crate::adapters::tui::tool_inspector::stage_target(state, conversation, open_turn),
    }
}

/// Event-loop opening seam for the focused Stage item: a block opens S-BLOCK,
/// a tool call keeps opening S-TOOL. Selection changes only once the exact
/// target resolves.
pub fn inspect_focused(
    state: &mut TuiState,
    conversation: &Conversation,
    open_turn: Option<&Turn>,
) -> bool {
    let Some((id, target)) = state
        .focused_item_id
        .clone()
        .filter(|id| is_block_item_id(id) || is_reasoning_item_id(id))
        .and_then(|id| target_of(conversation, open_turn, &id).map(|target| (id, target)))
    else {
        return crate::adapters::tui::tool_inspector::inspect_focused(
            state,
            conversation,
            open_turn,
        );
    };
    let mut fresh = BlockViewerState::new(target);
    let body = frame_rect(state).map(|rect| parts(rect).body);
    let (cols, viewport) = body.map_or((1, 1), |b| (usize::from(b.width), usize::from(b.height)));
    fresh.prepare(&conversation.turns, open_turn, cols, viewport, &state.theme);
    let Some(frame_title) = fresh.frame_title() else {
        return false;
    };
    reducer::open_block(state, fresh, frame_title);
    state.selected_item_id = Some(id);
    true
}

fn frame_rect(state: &TuiState) -> Option<Rect> {
    crate::adapters::tui::tool_inspector::frame_rect(state, 1)
}

/// The fitted allocation shared by preparation and paint (D9): the body keeps at
/// least one row, the rail one row (back key first), the status row one.
#[derive(Debug, Clone, Copy)]
pub struct Parts {
    pub status: Option<Rect>,
    pub body: Rect,
    pub rail: Option<Rect>,
}

pub fn parts(rect: Rect) -> Parts {
    let inner = rect.inner(Margin::new(1, 1));
    let rail_h = u16::from(inner.height >= 2);
    let status_h = u16::from(inner.height >= 3);
    let body_h = inner.height.saturating_sub(rail_h + status_h);
    let row = |y, height| Rect::new(inner.x, y, inner.width, height);
    Parts {
        status: (status_h != 0).then(|| row(inner.y, status_h)),
        body: row(inner.y + status_h, body_h),
        rail: (rail_h != 0).then(|| row(inner.y + status_h + body_h, rail_h)),
    }
}

/// Called before the real draw, with the actual owning tab's reducer.
pub fn refresh(
    state: &mut TuiState,
    conversation: &Conversation,
    reducer: Option<&ReducerState>,
    tab_count: usize,
) {
    if state.block_viewers.is_empty() {
        return;
    }
    let Some(rect) = crate::adapters::tui::tool_inspector::frame_rect(state, tab_count) else {
        return;
    };
    let body = parts(rect).body;
    let open_turn = reducer.and_then(|reducer| reducer.open_turn.as_ref());
    for block in state.block_viewers.values_mut() {
        if block.target.conversation_id != conversation.id {
            block.missing = true;
            continue;
        }
        block.prepare(
            &conversation.turns,
            open_turn,
            usize::from(body.width),
            usize::from(body.height),
            &state.theme,
        );
    }
}

/// Top-frame keys resolve only in S-BLOCK's registered context.
pub fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    let Some(Surface::BlockViewer(BlockFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return InputAction::Consumed;
    };
    let Resolved::Hit(row) = keymap::resolve(Context::BlockViewer, host, &[key]) else {
        return InputAction::Consumed;
    };
    if row.command == CommandId::Block(BlockCmd::Back) {
        reducer::back(state);
        return InputAction::Consumed;
    }
    let block = state
        .block_viewers
        .get_mut(&id)
        .expect("frame owns its block payload");
    if block.missing {
        return InputAction::Consumed;
    }
    let (rows, viewport) = (block.rows.len(), block.viewport);
    match row.command {
        CommandId::Overlay(OverlayCmd::InspectorSearch) => block.search.editor.open(),
        CommandId::Block(command @ (BlockCmd::NextMatch | BlockCmd::PrevMatch)) => {
            block.step_match(command == BlockCmd::NextMatch, &state.theme)
        }
        CommandId::Block(BlockCmd::Copy) => return InputAction::CopyBlockSource,
        CommandId::Body(BodyCmd::ToggleMode) => {
            block.mode = match block.mode {
                BlockMode::Rendered => BlockMode::Source,
                BlockMode::Source => BlockMode::Rendered,
            };
            // The rows change: start at the top, first column (nothing to keep).
            block.viewer = Viewer::default();
            block.dirty = true;
        }
        CommandId::Block(BlockCmd::Numbers) => {
            if block.applies_numbers() {
                block.numbers = !block.numbers;
                block.dirty = true;
            }
        }
        CommandId::Block(BlockCmd::Wrap) => {
            if block.applies_wrap() {
                block.wrap = !block.wrap;
                block.viewer.h_off = 0;
                block.dirty = true;
            }
        }
        CommandId::Block(command @ (BlockCmd::PrevBlock | BlockCmd::NextBlock)) => {
            return InputAction::StepBlock {
                forward: command == BlockCmd::NextBlock,
            };
        }
        CommandId::Viewer(command) if command.horizontal() => {
            if block.dirty {
                // The rows are about to be rebuilt by a toggle: judge the key
                // against the rebuilt rows, in order, at the next preparation.
                block.pending_horizontal.push(command);
            } else if block.wide() {
                let (widest, cols) = (block.widest, block.content_cols());
                viewer::apply_horizontal(&mut block.viewer, command, widest, cols);
            }
        }
        CommandId::Viewer(command) => viewer::apply(&mut block.viewer, command, rows, viewport),
        _ => return InputAction::Consumed,
    }
    state.needs_redraw = true;
    InputAction::Consumed
}

pub fn on_wheel(state: &mut TuiState, delta: ScrollDelta) -> InputAction {
    let Some(Surface::BlockViewer(BlockFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return InputAction::Consumed;
    };
    let block = state
        .block_viewers
        .get_mut(&id)
        .expect("frame owns its block payload");
    if block.missing {
        return InputAction::Consumed;
    }
    viewer::apply_scroll(&mut block.viewer, delta, block.rows.len(), block.viewport);
    state.needs_redraw = true;
    InputAction::Consumed
}

/// Rows that the painter shows: the horizontal window of one row, with `←`/`→`
/// reserved cells when content is hidden on either side (never a half glyph).
pub fn window(
    row: &Line<'static>,
    row_width: usize,
    h_off: usize,
    cols: usize,
) -> (Line<'static>, bool, bool) {
    let left = h_off > 0 && row_width > 0;
    let right = row_width > h_off + cols;
    let from = h_off + usize::from(left);
    let width = cols.saturating_sub(usize::from(left) + usize::from(right));
    (
        crate::adapters::tui::text_geometry::slice_line(row, from, width),
        left,
        right,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Turn, BlockTarget) {
        let mut turn = Turn::new("model".into(), 0);
        let part = turn.push_part(|id| TurnPart::Prose {
            id,
            text: format!("```rust\n{}\n```\n", "x".repeat(150)),
        });
        let target = BlockTarget {
            conversation_id: "c".into(),
            turn_id: turn.id.clone(),
            part_id: part,
            subject: BlockSubject::Block { ordinal: 0 },
        };
        (turn, target)
    }

    #[test]
    fn story21_13_prepare_clamps_offsets_after_a_resize_and_reuses_unchanged_rows() {
        let (turn, target) = fixture();
        let theme = Theme::dark();
        let mut block = BlockViewerState::new(target);
        block.prepare(std::slice::from_ref(&turn), None, 40, 5, &theme);
        assert_eq!(block.widest, 152);
        viewer::apply_horizontal(&mut block.viewer, ViewerCmd::RightEdge, block.widest, 40);
        assert_eq!(block.viewer.h_off, 112);
        // A wider viewport after a resize clamps the stale offset.
        block.prepare(std::slice::from_ref(&turn), None, 100, 5, &theme);
        assert_eq!(block.viewer.h_off, 52);
        block.prepare(std::slice::from_ref(&turn), None, 200, 5, &theme);
        assert_eq!(block.viewer.h_off, 0);
        assert!(!block.wide());
        // Unchanged source: the prepared rows are not rebuilt.
        let rows = block.rows.as_ptr();
        block.prepare(std::slice::from_ref(&turn), None, 200, 5, &theme);
        assert_eq!(block.rows.as_ptr(), rows);
        // A vanished target is reported, never panics.
        block.prepare(&[], None, 200, 5, &theme);
        assert!(block.missing);
    }

    #[test]
    fn story21_13_window_reserves_marker_cells_and_titles_are_sanitized() {
        let row = Line::from("0123456789".to_owned());
        let (line, left, right) = window(&row, 10, 0, 6);
        assert_eq!((left, right), (false, true));
        assert_eq!(line.spans[0].content, "01234");
        let (line, left, right) = window(&row, 10, 2, 6);
        assert_eq!((left, right), (true, true));
        assert_eq!(line.spans[0].content, "3456");
        let (_, left, right) = window(&row, 10, 4, 6);
        assert_eq!((left, right), (true, false));
        assert_eq!(safe_language("ru\x1b[31mst\nx"), "rust x");
    }

    fn searched(source: &str, query: &str, cols: usize) -> BlockViewerState {
        let mut turn = Turn::new("model".into(), 0);
        let part = turn.push_part(|id| TurnPart::Prose {
            id,
            text: format!("```\n{source}\n```\n"),
        });
        let target = BlockTarget {
            conversation_id: "c".into(),
            turn_id: turn.id.clone(),
            part_id: part,
            subject: BlockSubject::Block { ordinal: 0 },
        };
        let theme = Theme::dark();
        let mut block = BlockViewerState::new(target);
        block.prepare(std::slice::from_ref(&turn), None, cols, 5, &theme);
        block.search.editor.query.set(query);
        block.accept_search(&theme);
        block
    }

    #[test]
    fn story21_13_search_scrolls_a_clipped_match_into_view_and_marks_only_matching_rows() {
        let source = format!("{}needle{}", "a".repeat(250), "b".repeat(50));
        let mut block = searched(&source, "needle", 40);
        // The match sits at row cell 251; the window must show it.
        assert!(block.viewer.h_off < 251 && 251 < block.viewer.h_off + 39);
        assert_eq!(block.marked.len(), 1, "only the matching row is pre-styled");
        assert!(
            std::ptr::eq(block.painted(0), &block.rows[0]),
            "unmarked rows are borrowed"
        );
        let row = block.painted(1);
        let at = row
            .spans
            .iter()
            .position(|s| s.content == "needle")
            .expect("the match is its own span");
        assert_ne!(row.spans[at].style, row.spans[at - 1].style, "styled apart");
        assert_eq!(
            row.spans[at].style,
            row.spans[at - 1]
                .style
                .patch(Theme::dark().search_highlight_focused)
        );
        block.step_match(true, &Theme::dark());
        assert_eq!(block.search.status().as_deref(), Some("Search 1/1"));
    }

    #[test]
    fn story21_13_search_over_a_one_mebibyte_line_is_bounded_and_unchanged() {
        use std::{sync::mpsc, time::Duration};
        let (send, recv) = mpsc::channel();
        std::thread::spawn(move || {
            let source = "ab".repeat(512 * 1024);
            let plain = searched(&source, "zzz", 80);
            let block = searched(&source, "ab", 80);
            // 512 Ki matches are capped; painting never walks the row per paint.
            assert_eq!(
                block.search.matches.len(),
                crate::adapters::tui::presentation::viewer::MAX_SEARCH_MATCHES
            );
            let text = |line: &Line<'_>| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            };
            assert_eq!(
                text(block.painted(1)),
                text(plain.painted(1)),
                "identical output text"
            );
            let started = std::time::Instant::now();
            for _ in 0..200 {
                let (line, _, _) = window(block.painted(1), block.row_widths[1], 0, 78);
                assert_eq!(line.width(), 77);
            }
            send.send(started.elapsed()).unwrap();
        });
        let elapsed = recv
            .recv_timeout(Duration::from_secs(60))
            .expect("bounded work");
        assert!(
            elapsed < Duration::from_secs(2),
            "200 paints took {elapsed:?}"
        );
    }

    /// Review P6: the chat `c` of a reasoning item is bounded at the 8 MiB copy
    /// limit exactly like the window's copy.
    #[test]
    fn story21_9_chat_copy_of_reasoning_is_bounded_at_the_copy_limit() {
        let limit = crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
        for (len, copied) in [(limit, true), (limit + 1, false)] {
            let mut turn = Turn::new("model".into(), 0);
            let part = turn.push_part(|id| TurnPart::Reasoning {
                id,
                text: "x".repeat(len),
                duration_ms: None,
                tokens: None,
            });
            let item = crate::domain::models::turn::reasoning_item_id_for(&turn.id, part);
            let got = reasoning_source_of(std::slice::from_ref(&turn), None, &item);
            assert_eq!(got.is_some(), copied, "{len} bytes");
            if let Some(text) = got {
                assert_eq!(text.len(), len);
            }
        }
    }
}
