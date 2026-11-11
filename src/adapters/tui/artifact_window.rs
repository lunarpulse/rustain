//! The artifact window (Story 21.7 A15, AC8): one artifact's facts and body in a frame of its
//! own, opened by `Enter` on an Artifacts row and by `/artifact show <id>`. One window at a
//! time (global like S-THREAD): opening another retargets it. The facts are the drill-down's
//! header rows (`artifact_command::render_show`); the body is the artifact's stored bytes laid
//! out through `markdown_body::layout` (Rendered by default, `v` Source, explicit link URLs).
//!
//! Display and copy are different things. Everything painted is sanitized at the line builder
//! (`sanitize_tool_text_block`: controls and bidi overrides never reach a cell); `c` copies the
//! **original** bytes, refused above [`COPY_MAX_BYTES`] before any string is built. A body
//! above that bound is not laid out either: the window says so instead.
//!
//! `/` is a line filter over the body: while a query is kept only the source lines containing
//! it are shown (literal rows; the facts stay).

use std::cell::{Cell, Ref, RefCell};

use ratatui::text::Line;

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_block::BodyCmd;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::presentation::inspector_search::InspectorSearch;
use crate::adapters::tui::presentation::markdown_body::{
    self, BodyMode, RENDERED_BODY_MAX_BYTES, RENDERED_BODY_NOTICE,
};
use crate::adapters::tui::presentation::viewer::{self, Viewer, ViewerCmd};
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
use crate::domain::services::peer_text::{sanitize_display_line, sanitize_tool_text_block};

/// Why a copy was refused.
enum CopyRefusal {
    TooLarge,
    NotUtf8,
    Nothing,
}

/// What the window shows under the facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactBody {
    /// The artifact's stored bytes, exactly as read (what `c` copies).
    Source(Vec<u8>),
    /// The body could not be read: the reason, painted literally.
    Unavailable(String),
}

/// Which keys the last paint offered whole: a key whose hint was not painted is inert.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Live {
    pub search: bool,
    pub copy: bool,
    pub view: bool,
    /// The agent window's `p` hint was painted whole.
    pub pause: bool,
    /// The agent window's `x` hint was painted whole.
    pub kill: bool,
    /// The task window's `s` hint was painted whole.
    pub skip: bool,
    /// The task window's `r` hint was painted whole.
    pub retry: bool,
    /// The task window's `e` hint was painted whole.
    pub edit: bool,
}

#[derive(Default)]
struct Cache {
    key: Option<(u16, BodyMode, u64)>,
    lines: Vec<Line<'static>>,
    wide: Vec<std::ops::Range<usize>>,
    widest: usize,
}

#[derive(Default)]
struct Laid {
    lines: Vec<Line<'static>>,
    wide: Vec<std::ops::Range<usize>>,
    widest: usize,
}

/// How much of a body above the copy bound is laid out (bounded per-repaint work).
pub const DISPLAY_MAX_BYTES: usize = 1 << 20;

pub struct ArtifactWindowState {
    /// The full artifact id the window shows.
    pub id: String,
    pub body: ArtifactBody,
    pub mode: BodyMode,
    pub viewer: Viewer,
    pub search: InspectorSearch,
    /// Bumped by every change the painted body depends on besides the width.
    pub generation: u64,
    header: String,
    fit: Cell<(usize, usize)>,
    wide_fit: Cell<(usize, usize)>,
    live: Cell<Live>,
    cache: RefCell<Cache>,
    layouts_built: Cell<usize>,
    copy_builds: Cell<usize>,
    empty_text: &'static str,
    lossy: bool,
    /// What a successful copy says (a task window's status-specific wording); `None`: the
    /// shared "Copied to clipboard".
    copy_done: Option<String>,
    literal: bool,
}

impl std::fmt::Debug for ArtifactWindowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactWindowState")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl ArtifactWindowState {
    /// A window on artifact `id`: `header` is the facts text (one fact per line), `body` the
    /// stored bytes.
    #[must_use]
    pub fn new(id: String, header: String, body: ArtifactBody) -> Self {
        Self {
            id,
            body,
            mode: BodyMode::Rendered,
            viewer: Viewer::default(),
            search: InspectorSearch::default(),
            generation: 0,
            header,
            fit: Cell::new((0, 0)),
            wide_fit: Cell::new((0, 0)),
            live: Cell::new(Live::default()),
            cache: RefCell::new(Cache::default()),
            layouts_built: Cell::new(0),
            copy_builds: Cell::new(0),
            empty_text: "body is empty",
            lossy: false,
            copy_done: None,
            literal: false,
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.viewer = Viewer::default();
    }

    /// The shown body is a lossy rendering of bytes that were not valid text: `c` refuses
    /// rather than copy the replacement characters as "the original".
    pub fn set_lossy(&mut self, lossy: bool) {
        self.lossy = lossy;
    }

    /// What a successful copy says (the task window's `Copied error to clipboard.` etc.).
    pub fn set_copy_done(&mut self, message: String) {
        self.copy_done = Some(message);
    }

    /// The body is literal text (a task's error, skip or cancel reason): it is shown as Source
    /// and never offered as markdown (`v` is neither painted nor live; 7D-04).
    pub fn set_literal(&mut self, literal: bool) {
        let was = self.literal;
        self.literal = literal;
        // Entering the literal state forces Source; leaving it (a retried task completed)
        // returns to the Rendered default.
        let wanted = if literal {
            Some(BodyMode::Source)
        } else if was {
            Some(BodyMode::Rendered)
        } else {
            None
        };
        if let Some(mode) = wanted
            && self.mode != mode
        {
            self.mode = mode;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// The body may not be shown as markdown.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        self.literal
    }

    /// What the window says when its body is empty (default `body is empty`).
    #[must_use]
    pub fn with_empty_text(mut self, text: &'static str) -> Self {
        self.empty_text = text;
        self
    }

    /// Replace the facts and the body in place (the agent window follows its agent): the
    /// scroll position stays, and nothing is laid out again unless something changed.
    pub fn replace(&mut self, header: String, body: ArtifactBody) {
        if self.header != header || self.body != body {
            self.header = header;
            self.body = body;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// The window has a body that can be shown, searched, toggled and copied.
    #[must_use]
    pub fn has_source(&self) -> bool {
        matches!(&self.body, ArtifactBody::Source(bytes) if !bytes.is_empty())
    }

    /// The body is longer than the window lays out: only the first [`DISPLAY_MAX_BYTES`] are
    /// shown (with a notice naming what is not), copy is refused above [`COPY_MAX_BYTES`].
    #[must_use]
    pub fn display_truncated(&self) -> bool {
        matches!(&self.body, ArtifactBody::Source(bytes) if bytes.len() > COPY_MAX_BYTES)
    }

    /// The rows for `width`: facts, a blank row, then the body. Laid out once per (width,
    /// mode, generation) and reused by every repaint.
    pub fn rows(&self, theme: &Theme, width: u16) -> Ref<'_, [Line<'static>]> {
        let key = (width, self.mode, self.generation);
        if self.cache.borrow().key != Some(key) {
            let laid = self.build(theme, usize::from(width));
            *self.cache.borrow_mut() = Cache {
                key: Some(key),
                lines: laid.lines,
                wide: laid.wide,
                widest: laid.widest,
            };
            self.layouts_built.set(self.layouts_built.get() + 1);
        }
        Ref::map(self.cache.borrow(), |cache| cache.lines.as_slice())
    }

    /// The rows of the last layout that scroll sideways (wide code blocks and tables of a
    /// rendered body) and the widest block in cells.
    pub fn wide_rows(&self) -> (Vec<std::ops::Range<usize>>, usize) {
        let cache = self.cache.borrow();
        (cache.wide.clone(), cache.widest)
    }

    fn build(&self, theme: &Theme, width: usize) -> Laid {
        let width = width.max(1);
        let literal = |text: &str| {
            let mut lines = markdown_body::layout(text, width, theme, BodyMode::Source, true).lines;
            if lines.last().is_some_and(|line| line.width() == 0) {
                lines.pop();
            }
            lines
        };
        // The facts: every line passes the display sanitizer (producer and host ids come from
        // the journal), one row group per fact.
        let facts: String = self
            .header
            .lines()
            .map(|line| format!("{}\n", sanitize_tool_text_block(line)))
            .collect();
        let mut out = Laid {
            lines: literal(facts.trim_end()),
            ..Laid::default()
        };
        out.lines.push(Line::default());
        match &self.body {
            ArtifactBody::Unavailable(reason) => {
                out.lines.extend(literal(&format!(
                    "body unavailable: {}",
                    sanitize_display_line(reason)
                )));
            }
            ArtifactBody::Source(bytes) if bytes.is_empty() => {
                out.lines.extend(literal(self.empty_text));
            }
            ArtifactBody::Source(bytes) => {
                // Bounded per-repaint work: a body above the copy bound shows its first
                // DISPLAY_MAX_BYTES (Source) under a notice that names what is not shown.
                let truncated = bytes.len() > COPY_MAX_BYTES;
                let shown = if truncated {
                    &bytes[..DISPLAY_MAX_BYTES.min(bytes.len())]
                } else {
                    &bytes[..]
                };
                let text = String::from_utf8_lossy(shown);
                let safe = sanitize_tool_text_block(&text);
                if truncated {
                    out.lines.extend(literal(&format!(
                        "body is {} bytes: showing the first {} as source; the other {} bytes \
                         are not shown, searched or copyable (copy limit 8 MiB)",
                        bytes.len(),
                        shown.len(),
                        bytes.len() - shown.len()
                    )));
                }
                if !self.search.query.is_empty() {
                    let matching: Vec<&str> = safe
                        .lines()
                        .filter(|line| self.search.matches(line))
                        .collect();
                    if matching.is_empty() {
                        out.lines.extend(literal("no line matches this filter"));
                    } else {
                        out.lines.extend(
                            markdown_body::layout(
                                &matching.join("\n"),
                                width,
                                theme,
                                BodyMode::Source,
                                true,
                            )
                            .lines,
                        );
                    }
                } else if self.mode == BodyMode::Rendered
                    && !truncated
                    && safe.len() <= RENDERED_BODY_MAX_BYTES
                {
                    // Character references are decoded by the markdown parser: dangerous ones
                    // (bidi, controls) are defused to visible escapes first (21-8's 8B-01).
                    let defused =
                        crate::infrastructure::runtime::recipient_view::defuse_char_references(
                            &safe,
                        );
                    let laid =
                        markdown_body::layout(&defused, width, theme, BodyMode::Rendered, true);
                    let base = out.lines.len();
                    out.wide = laid
                        .wide
                        .iter()
                        .map(|r| base + r.start..base + r.end)
                        .collect();
                    out.widest = laid.widest;
                    out.lines.extend(laid.lines);
                } else {
                    if self.mode == BodyMode::Rendered && !truncated {
                        out.lines.extend(literal(RENDERED_BODY_NOTICE));
                    }
                    out.lines.extend(
                        markdown_body::layout(&safe, width, theme, BodyMode::Source, true).lines,
                    );
                }
            }
        }
        out
    }

    /// How many body layouts were computed (a repaint at the same key adds none).
    #[must_use]
    pub fn layouts_built(&self) -> usize {
        self.layouts_built.get()
    }

    /// How many copy strings were built (a refused copy builds none).
    #[must_use]
    pub fn copy_builds(&self) -> usize {
        self.copy_builds.get()
    }

    #[must_use]
    pub fn fit(&self) -> (usize, usize) {
        self.fit.get()
    }

    pub fn set_fit(&self, rows: usize, viewport: usize) {
        self.fit.set((rows, viewport));
    }

    /// `(widest wide block in cells, columns it scrolls in)` of the last paint; the widest is
    /// 0 when no block is wider than the body.
    #[must_use]
    pub fn wide_fit(&self) -> (usize, usize) {
        self.wide_fit.get()
    }

    pub fn set_wide_fit(&self, widest: usize, cols: usize) {
        self.wide_fit.set((widest, cols));
    }

    #[must_use]
    pub fn live(&self) -> Live {
        self.live.get()
    }

    pub fn set_live(&self, live: Live) {
        self.live.set(live);
    }
}

/// Where a text window keeps its state and what it is called. The artifact window and the
/// agent window share one body, one key executor and one painter and differ only here.
#[derive(Clone, Copy)]
pub(crate) struct Slot {
    pub surface: Surface,
    pub context: Context,
    /// The registry command whose key copies the original text.
    pub copy: MissionCmd,
    /// What a refused or empty copy names (`artifact body`, `agent transcript`).
    pub noun: &'static str,
    pub get: fn(&TuiState) -> Option<&ArtifactWindowState>,
    pub get_mut: fn(&mut TuiState) -> Option<&mut ArtifactWindowState>,
}

fn artifact_get(state: &TuiState) -> Option<&ArtifactWindowState> {
    state.artifact_window.as_ref()
}

fn artifact_get_mut(state: &mut TuiState) -> Option<&mut ArtifactWindowState> {
    state.artifact_window.as_mut()
}

pub(crate) const ARTIFACT: Slot = Slot {
    surface: Surface::ArtifactWindow,
    context: Context::ArtifactWindow,
    copy: MissionCmd::CopySource,
    noun: "artifact body",
    get: artifact_get,
    get_mut: artifact_get_mut,
};

/// The artifact window is the top frame.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    is_top_in(state, &ARTIFACT)
}

pub(crate) fn is_top_in(state: &TuiState, slot: &Slot) -> bool {
    state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == slot.surface)
}

/// The filter field of the window is open (a text context of the frame).
#[must_use]
pub fn search_editing(state: &TuiState) -> bool {
    search_editing_in(state, &ARTIFACT)
}

pub(crate) fn search_editing_in(state: &TuiState, slot: &Slot) -> bool {
    is_top_in(state, slot) && (slot.get)(state).is_some_and(|window| window.search.editing)
}

pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    on_key_in(state, key, host, &ARTIFACT, &mut |_, _| None)
}

/// One key of a text window: it resolves in the slot's context. `extra` sees the resolved
/// command first (the agent window's `p` and `x`) and may answer it with an action.
pub(crate) fn on_key_in(
    state: &mut TuiState,
    key: Key,
    host: Host,
    slot: &Slot,
    extra: &mut dyn FnMut(&mut TuiState, CommandId) -> Option<InputAction>,
) -> InputAction {
    if !is_top_in(state, slot) {
        return InputAction::Consumed;
    }
    let Resolved::Hit(row) = keymap::resolve(slot.context, host, &[key]) else {
        return InputAction::Consumed;
    };
    state.needs_redraw = true;
    if let Some(action) = extra(state, row.command) {
        return action;
    }
    match row.command {
        CommandId::Mission(MissionCmd::Back) => back_in(state, slot),
        CommandId::Mission(command) if command == slot.copy => {
            if (slot.get)(state).is_some_and(|window| window.live().copy) {
                copy_in(state, slot);
            }
        }
        CommandId::Overlay(OverlayCmd::InspectorSearch) => {
            if let Some(window) = (slot.get_mut)(state)
                && window.live().search
            {
                window.search.open();
            }
        }
        CommandId::Body(BodyCmd::ToggleMode) => {
            if let Some(window) = (slot.get_mut)(state)
                && window.live().view
                && !window.is_literal()
            {
                window.mode = window.mode.toggled();
                window.invalidate();
            }
        }
        CommandId::Viewer(command) if !command.horizontal() => scroll_in(state, slot, command),
        // Sideways movement exists only where the last paint showed a wide block.
        CommandId::Viewer(command) => {
            if let Some(window) = (slot.get_mut)(state) {
                let (widest, cols) = window.wide_fit();
                viewer::apply_horizontal(&mut window.viewer, command, widest, cols);
            }
        }
        _ => {}
    }
    InputAction::Consumed
}

/// `Esc` / `Ctrl+C`: a kept filter clears first, then the window backs to what is beneath.
fn back_in(state: &mut TuiState, slot: &Slot) {
    if (slot.get)(state).is_some_and(|window| !window.search.query.is_empty()) {
        clear_search_in(state, slot);
    } else {
        reducer::back(state);
    }
}

fn scroll_in(state: &mut TuiState, slot: &Slot, command: ViewerCmd) {
    if let Some(window) = (slot.get_mut)(state) {
        let (rows, viewport) = window.fit();
        if window.viewer.follow {
            window.viewer.top = usize::MAX;
        }
        viewer::apply(&mut window.viewer, command, rows, viewport);
    }
}

/// A wheel step scrolls the body like the line keys (state-only).
pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    on_wheel_in(state, &ARTIFACT, delta);
}

pub(crate) fn on_wheel_in(
    state: &mut TuiState,
    slot: &Slot,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    if let Some(window) = (slot.get_mut)(state) {
        let (rows, viewport) = window.fit();
        if window.viewer.follow {
            window.viewer.top = usize::MAX;
        }
        viewer::apply_scroll(&mut window.viewer, delta, rows, viewport);
        state.needs_redraw = true;
    }
}

/// The filter was edited or cleared: the body rows depend on it.
pub(crate) fn search_changed(state: &mut TuiState) {
    search_changed_in(state, &ARTIFACT);
}

pub(crate) fn search_changed_in(state: &mut TuiState, slot: &Slot) {
    if let Some(window) = (slot.get_mut)(state) {
        window.invalidate();
    }
    state.needs_redraw = true;
}

/// `Enter` in the filter field keeps it; a query matching no line says so and is dropped.
pub(crate) fn accept_search(state: &mut TuiState) {
    accept_search_in(state, &ARTIFACT);
}

pub(crate) fn accept_search_in(state: &mut TuiState, slot: &Slot) {
    let Some(window) = (slot.get_mut)(state) else {
        return;
    };
    window.search.accept();
    let query = window.search.query.as_str().to_owned();
    let displayed = match &window.body {
        ArtifactBody::Source(bytes) => {
            let end = if window.display_truncated() {
                DISPLAY_MAX_BYTES
            } else {
                bytes.len()
            };
            &bytes[..end.min(bytes.len())]
        }
        ArtifactBody::Unavailable(_) => &[][..],
    };
    let matched = query.is_empty()
        || String::from_utf8_lossy(displayed)
            .lines()
            .any(|line| window.search.matches(line));
    if !matched {
        window.search.back();
        crate::adapters::tui::notifications::info(
            state,
            crate::adapters::tui::notifications::NoticeSource::Search,
            format!("Search: no match for \"{query}\""),
        );
    }
    search_changed_in(state, slot);
}

/// `Esc` / `Ctrl+C` in the filter field clears it and leaves the field.
pub(crate) fn clear_search(state: &mut TuiState) {
    clear_search_in(state, &ARTIFACT);
}

pub(crate) fn clear_search_in(state: &mut TuiState, slot: &Slot) {
    if let Some(window) = (slot.get_mut)(state) {
        window.search.back();
    }
    search_changed_in(state, slot);
}

/// `c`: copy the artifact's **original** bytes (never the sanitized display). The bound is
/// checked on the byte length before any string is built; a refusal is a notice, never a
/// partial copy.
pub(crate) fn copy_source(state: &mut TuiState) {
    copy_in(state, &ARTIFACT);
}

/// Deliver an already-bounded copy: `None` = no source (a notice), `Some(None)` = refused above
/// the bound (a notice, nothing copied), `Some(Some(text))` = copied. Shared by the hub's `c`.
pub(crate) fn deliver_copy(state: &mut TuiState, noun: &str, outcome: Option<Option<String>>) {
    use crate::adapters::tui::handlers::clipboard::{notify_copy, notify_failure};
    use crate::adapters::tui::notifications::{self, NoticeSource};
    let copier = state.clipboard_copier.clone();
    match outcome {
        None => notifications::info(state, NoticeSource::Clipboard, "Nothing to copy"),
        Some(None) => notify_failure(state, &format!("{noun} exceeds the 8 MiB copy limit")),
        Some(Some(text)) => match copier {
            Some(copier) => notify_copy(state, &text, &*copier),
            None => notify_copy(
                state,
                &text,
                &crate::adapters::tui::clipboard::copy_to_clipboard,
            ),
        },
    }
}

pub(crate) fn copy_in(state: &mut TuiState, slot: &Slot) {
    use crate::adapters::tui::handlers::clipboard::{notify_copy_with, notify_failure};
    use crate::adapters::tui::notifications::{self, NoticeSource};
    let copier = state.clipboard_copier.clone();
    let success = (slot.get)(state)
        .and_then(|window| window.copy_done.clone())
        .unwrap_or_else(|| "Copied to clipboard".to_owned());
    // The bound is judged on the byte length; only then is the copy string built.
    let outcome = match (slot.get)(state) {
        None => return,
        Some(window) => match &window.body {
            ArtifactBody::Source(bytes) if !bytes.is_empty() => {
                if bytes.len() > COPY_MAX_BYTES {
                    Err(CopyRefusal::TooLarge)
                } else if window.lossy || std::str::from_utf8(bytes).is_err() {
                    // The clipboard takes text: never copy a lossy conversion as "the original".
                    Err(CopyRefusal::NotUtf8)
                } else {
                    window.copy_builds.set(window.copy_builds.get() + 1);
                    Ok(String::from_utf8_lossy(bytes).into_owned())
                }
            }
            _ => Err(CopyRefusal::Nothing),
        },
    };
    let text = match outcome {
        Ok(text) => text,
        Err(CopyRefusal::TooLarge) => {
            notify_failure(
                state,
                &format!("{} exceeds the 8 MiB copy limit", slot.noun),
            );
            return;
        }
        Err(CopyRefusal::NotUtf8) => {
            notify_failure(
                state,
                &format!("{} is not valid UTF-8; nothing was copied", slot.noun),
            );
            return;
        }
        Err(CopyRefusal::Nothing) => {
            notifications::info(state, NoticeSource::Clipboard, "Nothing to copy");
            return;
        }
    };
    match copier {
        Some(copier) => notify_copy_with(state, &text, &*copier, &success),
        None => notify_copy_with(
            state,
            &text,
            &crate::adapters::tui::clipboard::copy_to_clipboard,
            &success,
        ),
    }
}

/// The raw press-only rule of the frame (RU-29): only a plain Press acts; a Ctrl letter acts
/// only where the registry binds it. The open filter field is a text context: the guard stands
/// aside there.
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    ignores_raw_key_in(state, &ARTIFACT, key)
}

pub(crate) fn ignores_raw_key_in(
    state: &TuiState,
    slot: &Slot,
    key: &crossterm::event::KeyEvent,
) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if !is_top_in(state, slot) || search_editing_in(state, slot) {
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
