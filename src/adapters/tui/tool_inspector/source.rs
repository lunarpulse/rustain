//! Original tab sources and their sanitized, literal display boundary.
//!
//! The diff derivation and honest provenance messages formerly belonged to the
//! expanded tool widget. The inspector is their owner after the Stage cutover.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::rc::Rc;

use ratatui::prelude::*;
use serde::Serialize;

use crate::adapters::tui::markdown::{self, RenderOptions};
use crate::adapters::tui::presentation::viewer::{RowOrigin, row_origins};
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{
    DIFF_MAX_LINES, DiffKind, DiffLine, InvocationStatus, NotCapturedReason, WriteDiffState,
    diff::capped_display_diff,
};
use crate::domain::services::peer_text::sanitize_tool_text_block;

use super::model::{ToolTab, ToolView};

/// Output is raw by default, but still uses the shared markdown pipeline.
pub const OUTPUT_OPTIONS: RenderOptions = RenderOptions::literal();

const BASE_TABS: &[ToolTab] = &[ToolTab::Output, ToolTab::Input, ToolTab::Raw];
const DIFF_TABS: &[ToolTab] = &[ToolTab::Output, ToolTab::Input, ToolTab::Raw, ToolTab::Diff];
const NO_CHANGE: &str = "no line changes — the file content is unchanged";

/// Debug-only payload-work counters, scoped to the current render thread.
#[cfg(any(test, debug_assertions))]
pub mod metrics {
    use std::cell::Cell;

    thread_local! {
        static BYTES: Cell<usize> = const { Cell::new(0) };
        static PREPARATIONS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn payload(bytes: usize) {
        BYTES.with(|count| count.set(count.get() + bytes));
    }

    pub(super) fn prepare() {
        PREPARATIONS.with(|count| count.set(count.get() + 1));
    }

    pub fn counts() -> (usize, usize) {
        (BYTES.with(Cell::get), PREPARATIONS.with(Cell::get))
    }

    pub fn reset() {
        BYTES.with(|count| count.set(0));
        PREPARATIONS.with(|count| count.set(0));
    }
}

// Borrow the original record; identity precedes potentially large result text.
#[derive(Serialize)]
struct RawRecord<'a> {
    id: &'a str,
    tool: &'a str,
    status: &'a InvocationStatus,
    input: &'a serde_json::Value,
    result: Option<RawResult<'a>>,
    started_at: i64,
    ended_at: Option<i64>,
}

#[derive(Serialize)]
struct RawResult<'a> {
    content: &'a str,
    is_error: bool,
}

/// Site-supplied rows stay borrowed; memoized input-derived rows share storage.
#[derive(Debug, Clone)]
pub enum DiffLines<'a> {
    Borrowed(&'a [DiffLine]),
    Memoized(Rc<[DiffLine]>),
}

impl AsRef<[DiffLine]> for DiffLines<'_> {
    fn as_ref(&self) -> &[DiffLine] {
        match self {
            Self::Borrowed(lines) => lines,
            Self::Memoized(lines) => lines,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DiffBody<'a> {
    /// Complete, already capped rows, including any elision/cap markers.
    Lines {
        lines: DiffLines<'a>,
    },
    NotCaptured {
        bytes: usize,
        reason: NotCapturedReason,
    },
    NoChange,
}

impl DiffBody<'_> {
    /// Also used by the Stage's content-aware turn-fold heuristic.
    pub fn row_count(&self) -> usize {
        match self {
            Self::Lines { lines } => lines.as_ref().len(),
            Self::NotCaptured { .. } | Self::NoChange => 1,
        }
    }
}

/// Derive a diff for a completed non-error Edit/Write, with no I/O.
///
/// Completion/error/status gating belongs to the caller. The field-oriented
/// API also serves the Stage's fold heuristic without manufacturing a view or
/// copying `ToolCallInfo`/`ToolOutput` between their distinct domain shapes.
pub fn diff_body<'a>(
    name: &str,
    args: &serde_json::Value,
    diff: &'a WriteDiffState,
) -> Option<DiffBody<'a>> {
    match name {
        "Edit" | "edit" => {
            let old = args.get("old_string")?.as_str()?;
            let new = args.get("new_string")?.as_str()?;
            if old.is_empty() || old == new {
                return None;
            }
            Some(computed_body(edit_hunk(old, new)))
        }
        "Write" | "write" => match diff {
            WriteDiffState::NotAWrite => None,
            WriteDiffState::NewFile => {
                let content = args.get("content")?.as_str()?;
                Some(computed_body(edit_hunk("", content)))
            }
            WriteDiffState::Diff { lines, more } if lines.is_empty() && *more == 0 => {
                Some(DiffBody::NoChange)
            }
            WriteDiffState::Diff { lines, .. } => Some(DiffBody::Lines {
                lines: DiffLines::Borrowed(lines),
            }),
            WriteDiffState::NotCaptured { reason } => Some(DiffBody::NotCaptured {
                bytes: args
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len),
                reason: *reason,
            }),
        },
        _ => None,
    }
}

fn computed_body(lines: Rc<[DiffLine]>) -> DiffBody<'static> {
    if lines.is_empty() {
        DiffBody::NoChange
    } else {
        DiffBody::Lines {
            lines: DiffLines::Memoized(lines),
        }
    }
}

/// Bounded memoization of the existing capped display-diff computation.
/// Shared rows avoid copying the entire hunk on each Stage height calculation.
fn edit_hunk(old: &str, new: &str) -> Rc<[DiffLine]> {
    #[cfg(any(test, debug_assertions))]
    metrics::payload(old.len() + new.len());
    const MEMO_CAP: usize = 256;
    thread_local! {
        static MEMO: RefCell<HashMap<u64, Rc<[DiffLine]>>> = RefCell::new(HashMap::new());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    old.hash(&mut hasher);
    new.hash(&mut hasher);
    let key = hasher.finish();
    MEMO.with(|memo| {
        if let Some(hit) = memo.borrow().get(&key) {
            return Rc::clone(hit);
        }
        let (lines, _) = capped_display_diff(old, new, DIFF_MAX_LINES);
        let lines: Rc<[DiffLine]> = lines.into_iter().map(|line| line.into_owned()).collect();
        let mut memo = memo.borrow_mut();
        if memo.len() >= MEMO_CAP {
            memo.clear();
        }
        memo.insert(key, Rc::clone(&lines));
        lines
    })
}

/// No dead Diff tab: a successful invocation AND a non-error result are needed.
pub fn available_tabs(view: &ToolView<'_>) -> &'static [ToolTab] {
    let Some(result) = view.result else {
        return BASE_TABS;
    };
    if *view.status == InvocationStatus::Success
        && !result.is_error
        && diff_body(view.name, view.args, &result.diff).is_some()
    {
        DIFF_TABS
    } else {
        BASE_TABS
    }
}

fn view_diff<'a>(view: &ToolView<'a>) -> Option<DiffBody<'a>> {
    if *view.status != InvocationStatus::Success {
        return None;
    }
    let result = view.result.filter(|result| !result.is_error)?;
    diff_body(view.name, view.args, &result.diff)
}

/// Original bytes only: display notices and sanitization never change a source.
/// In particular an absent Output remains empty rather than becoming copyable
/// UI prose, and a completed result always takes precedence over a live tail.
pub fn text<'a>(view: &ToolView<'a>, tab: ToolTab) -> Cow<'a, str> {
    match tab {
        ToolTab::Output => Cow::Borrowed(
            view.result
                .map(|result| result.content.as_str())
                .or_else(|| view.is_running().then_some(view.tail).flatten())
                .unwrap_or(""),
        ),
        ToolTab::Input => Cow::Owned(
            serde_json::to_string_pretty(view.args).expect("JSON values always serialize"),
        ),
        ToolTab::Raw => {
            let record = RawRecord {
                id: &view.target.tool_call_id,
                tool: view.name,
                status: view.status,
                input: view.args,
                result: view.result.map(|result| RawResult {
                    content: &result.content,
                    is_error: result.is_error,
                }),
                started_at: view.started_at,
                ended_at: view.ended_at,
            };
            Cow::Owned(serde_json::to_string_pretty(&record).expect("JSON values always serialize"))
        }
        ToolTab::Diff => match view_diff(view) {
            Some(DiffBody::Lines { lines }) => Cow::Owned(diff_text(lines.as_ref())),
            Some(DiffBody::NotCaptured { bytes, reason }) => {
                Cow::Owned(not_captured(bytes, reason))
            }
            Some(DiffBody::NoChange) => Cow::Borrowed(NO_CHANGE),
            None => Cow::Borrowed(""),
        },
    }
}

/// Canonical copy is all-or-nothing and bounded in UTF-8 bytes, not cells.
pub const COPY_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopySourceError {
    Exceeded,
    Serialization,
}

impl CopySourceError {
    /// Fixed nonsecret diagnostics: never surface serde errors or source text.
    pub fn message(self) -> &'static str {
        match self {
            Self::Exceeded => "tool source exceeds the 8 MiB copy limit",
            Self::Serialization => "tool source could not be serialized",
        }
    }
}

#[derive(Default)]
struct CopyWriter {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for CopyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > COPY_MAX_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(io::Error::other("copy limit exceeded"));
        }
        // Vec's geometric growth must not reserve beyond the copy bound.
        let required = self.bytes.len() + bytes.len();
        if required > self.bytes.capacity() {
            let capacity = required
                .max(self.bytes.capacity().saturating_mul(2))
                .min(COPY_MAX_BYTES);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl CopyWriter {
    fn finish<E>(self, result: Result<(), E>) -> Result<String, CopySourceError> {
        if self.exceeded {
            Err(CopySourceError::Exceeded)
        } else if result.is_err() {
            Err(CopySourceError::Serialization)
        } else {
            String::from_utf8(self.bytes).map_err(|_| CopySourceError::Serialization)
        }
    }
}

/// Resolve the original source without first allocating an unbounded JSON or
/// diff string. Output/live tails remain borrowed. Raw's private adapters are
/// exactly those used by `text`, including field order and absent optionals.
pub fn copy_text<'a>(view: &ToolView<'a>, tab: ToolTab) -> Result<Cow<'a, str>, CopySourceError> {
    if tab == ToolTab::Output {
        let source = text(view, tab);
        return if source.len() <= COPY_MAX_BYTES {
            Ok(source)
        } else {
            Err(CopySourceError::Exceeded)
        };
    }
    let mut writer = CopyWriter::default();
    let result = match tab {
        ToolTab::Input => serde_json::to_writer_pretty(&mut writer, view.args),
        ToolTab::Raw => serde_json::to_writer_pretty(
            &mut writer,
            &RawRecord {
                id: &view.target.tool_call_id,
                tool: view.name,
                status: view.status,
                input: view.args,
                result: view.result.map(|result| RawResult {
                    content: &result.content,
                    is_error: result.is_error,
                }),
                started_at: view.started_at,
                ended_at: view.ended_at,
            },
        ),
        ToolTab::Diff => return copy_diff(view),
        ToolTab::Output => unreachable!(),
    };
    writer.finish(result).map(Cow::Owned)
}

// Serialization consumes the same canonical borrowed, capped diff rows as
// display derivation. Only the final byte-bounded source copy is owned.
type CopyRow<'a> = (DiffKind, Cow<'a, str>);

fn copy_diff<'a>(view: &ToolView<'a>) -> Result<Cow<'a, str>, CopySourceError> {
    if *view.status != InvocationStatus::Success {
        return Ok(Cow::Borrowed(""));
    }
    let Some(result) = view.result.filter(|result| !result.is_error) else {
        return Ok(Cow::Borrowed(""));
    };
    let rows = match view.name {
        "Edit" | "edit" => {
            let Some((old, new)) = view
                .args
                .get("old_string")
                .and_then(|s| s.as_str())
                .zip(view.args.get("new_string").and_then(|s| s.as_str()))
            else {
                return Ok(Cow::Borrowed(""));
            };
            if old.is_empty() || old == new {
                return Ok(Cow::Borrowed(""));
            }
            capped_display_diff(old, new, DIFF_MAX_LINES).0
        }
        "Write" | "write" => match &result.diff {
            WriteDiffState::NotAWrite => return Ok(Cow::Borrowed("")),
            WriteDiffState::NewFile => {
                let Some(content) = view.args.get("content").and_then(|s| s.as_str()) else {
                    return Ok(Cow::Borrowed(""));
                };
                capped_display_diff("", content, DIFF_MAX_LINES).0
            }
            WriteDiffState::Diff { lines, more } => {
                if lines.is_empty() && *more == 0 {
                    return Ok(Cow::Borrowed(NO_CHANGE));
                }
                // Persisted rows are already capped. Do not re-cap or fabricate
                // a marker from `more`: copy the exact original source rows.
                return write_copy_rows(
                    lines
                        .iter()
                        .map(|line| (line.kind, Cow::Borrowed(line.content.as_str()))),
                );
            }
            WriteDiffState::NotCaptured { reason } => {
                return Ok(Cow::Owned(not_captured(
                    view.args
                        .get("content")
                        .and_then(|s| s.as_str())
                        .map_or(0, str::len),
                    *reason,
                )));
            }
        },
        _ => return Ok(Cow::Borrowed("")),
    };
    if rows.is_empty() {
        return Ok(Cow::Borrowed(NO_CHANGE));
    }
    write_copy_rows(rows.into_iter().map(|line| (line.kind, line.content)))
}

fn write_copy_rows<'a>(
    rows: impl IntoIterator<Item = CopyRow<'a>>,
) -> Result<Cow<'a, str>, CopySourceError> {
    let mut writer = CopyWriter::default();
    let result = (|| {
        for (index, (kind, text)) in rows.into_iter().enumerate() {
            if index != 0 {
                writer.write_all(b"\n")?;
            }
            writer.write_all(diff_prefix(kind).as_bytes())?;
            writer.write_all(text.as_bytes())?;
        }
        Ok::<(), io::Error>(())
    })();
    writer.finish(result).map(Cow::Owned)
}

fn not_captured(bytes: usize, reason: NotCapturedReason) -> String {
    let verb = if reason == NotCapturedReason::NoActiveCheckpoint {
        "overwrote"
    } else {
        "wrote"
    };
    format!(
        "{verb} {bytes} bytes — previous content not captured ({})",
        reason.describe()
    )
}

fn diff_prefix(kind: DiffKind) -> &'static str {
    match kind {
        DiffKind::Added => "+ ",
        DiffKind::Removed => "- ",
        DiffKind::Context => "  ",
        DiffKind::Elided => "",
    }
}

fn diff_text(lines: &[DiffLine]) -> String {
    let mut text = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        text.push_str(diff_prefix(line.kind));
        text.push_str(&line.content);
    }
    text
}

/// Display-only notices are deliberately separate from original tab sources.
/// The injected wall time is used only for the no-tail running notice.
pub fn display_text<'a>(view: &ToolView<'a>, tab: ToolTab, now_ms: i64) -> Cow<'a, str> {
    if tab == ToolTab::Input
        && (view.args.is_null() || view.args.as_object().is_some_and(|args| args.is_empty()))
    {
        return Cow::Borrowed("No input.");
    }
    if tab == ToolTab::Output {
        if let Some(result) = view.result {
            if result.content.is_empty() {
                return Cow::Borrowed("The tool returned no output.");
            }
        } else if view.is_running() {
            let tail = view.tail.unwrap_or("");
            if tail.is_empty() {
                let elapsed = now_ms.saturating_sub(view.started_at).max(0) as f64 / 1000.0;
                return Cow::Owned(format!("No output yet — running ({elapsed:.1}s)"));
            }
            return Cow::Owned(format!(
                "Live tail (last {} lines) — the full output appears when the tool finishes\n{tail}",
                tail.lines().count(),
            ));
        } else {
            return Cow::Borrowed("The tool returned no output.");
        }
    }
    text(view, tab)
}

/// A sanitized cache candidate. The host first compares [`revision`], then
/// prepares changed sources once to derive its sanitized key and render on a
/// cache miss. In particular Input/Raw are not serialized a second time.
pub struct PreparedSource<'a> {
    body: PreparedBody<'a>,
    options: RenderOptions,
    dim_header: bool,
    hash: u64,
}

enum PreparedBody<'a> {
    Literal(Cow<'a, str>),
    Diff { text: String, kinds: Vec<DiffKind> },
    Muted(Cow<'a, str>),
    Empty,
}

/// Actual shared-pipeline rows and their normalized logical-source positions.
pub struct RenderedSource {
    pub lines: Vec<Line<'static>>,
    pub origins: Vec<Option<RowOrigin>>,
}

impl PreparedSource<'_> {
    /// Hash of sanitized source bytes and their rendering style. Width and
    /// wrap are the frame's remaining key axes, not part of source identity.
    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// Sanitized display text, borrowed without copying or serializing again.
    /// Literal normalization and tab expansion belong to the shared viewer.
    pub fn logical_text(&self) -> &str {
        match &self.body {
            PreparedBody::Literal(text) | PreparedBody::Muted(text) => text,
            PreparedBody::Diff { text, .. } => text,
            PreparedBody::Empty => "",
        }
    }

    /// Render and map this candidate without reading or preparing the view again.
    pub fn render(&self, width: usize, theme: &Theme) -> RenderedSource {
        if width == 0 || matches!(&self.body, PreparedBody::Empty) {
            return RenderedSource {
                lines: Vec::new(),
                origins: Vec::new(),
            };
        }
        let text = self.logical_text();
        let mut lines = markdown::render(text, width, theme, &self.options).lines;
        let mut origins = row_origins(text, &lines);
        // Diff and provenance notices historically have no paragraph separator.
        // Remove only Literal's display-only trailing blank, after mapping it.
        if matches!(
            &self.body,
            PreparedBody::Diff { .. } | PreparedBody::Muted(_)
        ) && !lines.is_empty()
        {
            lines.pop();
            origins.pop();
        }
        for (line, origin) in lines.iter_mut().zip(&origins) {
            let color = match &self.body {
                PreparedBody::Diff { kinds, .. } => origin
                    .as_ref()
                    .and_then(|origin| kinds.get(origin.logical_line))
                    .map(|kind| diff_color(*kind, theme)),
                PreparedBody::Muted(_) => Some(theme.colors.fg_muted),
                PreparedBody::Literal(_) | PreparedBody::Empty => None,
            };
            if let Some(color) = color {
                for span in &mut line.spans {
                    span.style = span.style.fg(color);
                }
            }
        }
        if self.dim_header {
            if let Some(header) = lines.first_mut() {
                for span in &mut header.spans {
                    span.style = span.style.fg(theme.colors.fg_muted);
                }
            }
        }
        RenderedSource { lines, origins }
    }
}

/// Prepare one changed source at the display boundary. Clean borrowed Output
/// remains borrowed; clean owned JSON retains its existing allocation.
pub fn prepare<'a>(view: &ToolView<'a>, tab: ToolTab, now_ms: i64) -> PreparedSource<'a> {
    #[cfg(any(test, debug_assertions))]
    metrics::prepare();
    let body = if tab == ToolTab::Diff {
        match view_diff(view) {
            Some(DiffBody::Lines { lines }) => prepare_diff(lines.as_ref()),
            Some(DiffBody::NotCaptured { bytes, reason }) => {
                PreparedBody::Muted(Cow::Owned(not_captured(bytes, reason)))
            }
            Some(DiffBody::NoChange) => PreparedBody::Muted(Cow::Borrowed(NO_CHANGE)),
            None => PreparedBody::Empty,
        }
    } else {
        PreparedBody::Literal(sanitize_source(display_text(view, tab, now_ms)))
    };
    let options = if tab == ToolTab::Output {
        OUTPUT_OPTIONS
    } else {
        RenderOptions::literal()
    };
    let dim_header = tab == ToolTab::Output && view.result.is_none() && view.is_running();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    dim_header.hash(&mut hash);
    match &body {
        PreparedBody::Literal(text) => {
            #[cfg(any(test, debug_assertions))]
            metrics::payload(text.len());
            0_u8.hash(&mut hash);
            text.hash(&mut hash);
        }
        PreparedBody::Diff { text, kinds } => {
            #[cfg(any(test, debug_assertions))]
            metrics::payload(text.len());
            1_u8.hash(&mut hash);
            text.hash(&mut hash);
            // Equal text can still have distinct added/removed/elided styles.
            for kind in kinds {
                std::mem::discriminant(kind).hash(&mut hash);
            }
        }
        PreparedBody::Muted(text) => {
            2_u8.hash(&mut hash);
            text.hash(&mut hash);
        }
        PreparedBody::Empty => 3_u8.hash(&mut hash),
    }
    PreparedSource {
        body,
        options,
        dim_header,
        hash: hash.finish(),
    }
}

fn sanitize_source(source: Cow<'_, str>) -> Cow<'_, str> {
    match source {
        Cow::Borrowed(source) => sanitize_tool_text_block(source),
        Cow::Owned(source) => match sanitize_tool_text_block(&source) {
            Cow::Borrowed(_) => Cow::Owned(source),
            Cow::Owned(sanitized) => Cow::Owned(sanitized),
        },
    }
}

/// Convenience entry point over the same preparation/rendering implementation
/// used by frame-owned caches.
pub fn render(
    view: &ToolView<'_>,
    tab: ToolTab,
    width: u16,
    theme: &Theme,
    now_ms: i64,
) -> Vec<Line<'static>> {
    prepare(view, tab, now_ms)
        .render(usize::from(width), theme)
        .lines
}

/// Join and sanitize the display diff once. Each normalized logical line keeps
/// its originating kind, including embedded newlines in a site-supplied row.
/// Original unsanitized copy sources continue to use [`diff_text`] separately.
fn prepare_diff(diff: &[DiffLine]) -> PreparedBody<'static> {
    let capacity = diff
        .iter()
        .map(|line| diff_prefix(line.kind).len() + line.content.len())
        .sum::<usize>()
        + diff.len().saturating_sub(1);
    let mut text = String::with_capacity(capacity);
    let mut kinds = Vec::with_capacity(diff.len());
    for (index, line) in diff.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        let safe = sanitize_tool_text_block(&line.content);
        text.push_str(diff_prefix(line.kind));
        text.push_str(&safe);
        // The sanitizer has already normalized CRLF/bare CR. Splitting without
        // trimming retains blanks before the next diff row; Literal only trims
        // newlines at the end of the complete body.
        kinds.extend(std::iter::repeat_n(line.kind, safe.split('\n').count()));
    }
    PreparedBody::Diff { text, kinds }
}

fn diff_color(kind: DiffKind, theme: &Theme) -> Color {
    match kind {
        DiffKind::Added => theme.colors.success,
        DiffKind::Removed => theme.colors.error,
        DiffKind::Context => theme.colors.fg_secondary,
        DiffKind::Elided => theme.colors.fg_muted,
    }
}

/// The moved diff helper uses the same Literal renderer, without a width cap.
pub fn render_diff_lines(diff: &[DiffLine], theme: &Theme) -> Vec<Line<'static>> {
    PreparedSource {
        body: prepare_diff(diff),
        options: RenderOptions::literal(),
        dim_header: false,
        hash: 0,
    }
    .render(usize::MAX, theme)
    .lines
}

/// Constant-size canonical identity; no source bytes are inspected on a hit.
/// The host adds width and, for a no-tail running notice, its elapsed-time tick.
pub fn revision(view: &ToolView<'_>, tab: ToolTab) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    view.source_revision.hash(&mut hash);
    tab.hash(&mut hash);
    if tab == ToolTab::Output && view.is_running() && view.result.is_none() {
        view.tail_revision.hash(&mut hash);
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::super::model::ToolTarget;
    use super::*;
    use crate::domain::models::{PartId, ToolOutput, TurnId};
    use serde_json::json;

    fn with_view(
        name: &str,
        args: serde_json::Value,
        status: InvocationStatus,
        result: Option<ToolOutput>,
        check: impl FnOnce(ToolView<'_>),
    ) {
        let target = ToolTarget {
            conversation_id: "conversation".into(),
            tool_call_id: "tc_turn_2".into(),
        };
        let turn_id = TurnId("turn".into());
        check(ToolView {
            target: &target,
            turn_id: &turn_id,
            part_id: PartId(2),
            name,
            status: &status,
            ordinal: 2,
            args: &args,
            result: result.as_ref(),
            tail: None,
            started_at: 1_000,
            ended_at: Some(2_000),
            source_revision: 1,
            tail_revision: 0,
        });
    }

    fn output(content: &str, diff: WriteDiffState) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            is_error: false,
            diff,
            images: Vec::new(),
        }
    }

    #[test]
    fn story21_5_copy_keeps_output_and_per_call_tail_borrowed() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Running,
            None,
            |mut view| {
                view.tail = Some("original\r\n\u{1b}[31m");
                let source = copy_text(&view, ToolTab::Output).unwrap();
                assert!(matches!(source, Cow::Borrowed(_)));
                assert!(std::ptr::eq(source.as_ptr(), view.tail.unwrap().as_ptr()));
            },
        );
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("completed", WriteDiffState::NotAWrite)),
            |mut view| {
                view.tail = Some("stale tail must lose to completed output");
                let source = copy_text(&view, ToolTab::Output).unwrap();
                assert!(matches!(source, Cow::Borrowed(_)));
                assert!(std::ptr::eq(
                    source.as_ptr(),
                    view.result.unwrap().content.as_ptr()
                ));
            },
        );
    }

    #[test]
    fn story21_5_copy_writer_is_bounded_and_errors_are_fixed_nonsecret() {
        let mut writer = CopyWriter::default();
        let chunk = vec![b'x'; COPY_MAX_BYTES];
        writer.write_all(&chunk).unwrap();
        assert_eq!(writer.bytes.len(), COPY_MAX_BYTES);
        assert!(writer.bytes.capacity() <= COPY_MAX_BYTES);
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(
            writer.finish(Err::<(), _>(())),
            Err(CopySourceError::Exceeded)
        );

        struct Failing;
        impl Serialize for Failing {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(
                    "secret source must never surface",
                ))
            }
        }
        let mut writer = CopyWriter::default();
        let result = serde_json::to_writer_pretty(&mut writer, &Failing);
        let error = writer.finish(result).unwrap_err();
        assert_eq!(error, CopySourceError::Serialization);
        assert_eq!(error.message(), "tool source could not be serialized");
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[test]
    fn story21_5_bounded_derived_diff_matches_original_elision_cap_and_tie_breaking() {
        let contexts = (0..240)
            .map(|i| format!("same {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let removed = (0..2100)
            .map(|i| format!("old {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let added = (0..2100)
            .map(|i| format!("new {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for (old, new) in [
            ("old".to_owned(), "new".to_owned()),
            ("a\r\nb\n".into(), "a\nb".into()),
            ("a\nb\na".into(), "b\na\nb".into()),
            (
                format!("{contexts}\nold\n{contexts}"),
                format!("{contexts}\nnew\n{contexts}"),
            ),
            (removed, added),
            ("".into(), contexts.clone()),
            ("before".into(), "".into()),
        ] {
            for name in ["Edit", "Write"] {
                if name == "Edit" && old.is_empty() {
                    continue;
                }
                let args = if name == "Edit" {
                    json!({"old_string":old, "new_string":new})
                } else {
                    json!({"content":new})
                };
                with_view(
                    name,
                    args,
                    InvocationStatus::Success,
                    Some(output("", WriteDiffState::NewFile)),
                    |view| {
                        assert_eq!(
                            copy_text(&view, ToolTab::Diff).unwrap(),
                            text(&view, ToolTab::Diff),
                            "{name}: bounded copy must preserve provenance and rows"
                        );
                    },
                );
            }
        }
        // Cover short overlapping context runs and every LCS tie in a small
        // alphabet through the independent original-source implementation.
        let texts = [
            "a",
            "a\nb",
            "b\na",
            "a\nb\na",
            "b\na\nb",
            "x\na\nb\nc\nd\ny",
        ];
        for old in texts {
            for new in texts {
                with_view(
                    "Edit",
                    json!({"old_string":old, "new_string":new}),
                    InvocationStatus::Success,
                    Some(output("", WriteDiffState::NotAWrite)),
                    |view| {
                        assert_eq!(
                            copy_text(&view, ToolTab::Diff).unwrap(),
                            text(&view, ToolTab::Diff)
                        )
                    },
                );
            }
        }
    }

    #[test]
    fn story21_5_bounded_diff_preserves_provenance_and_persisted_rows() {
        for reason in [
            NotCapturedReason::NoActiveCheckpoint,
            NotCapturedReason::SnapshotUnavailable,
            NotCapturedReason::SnapshotReadFailed,
            NotCapturedReason::OriginalNotUtf8,
            NotCapturedReason::SupersededInBatch,
            NotCapturedReason::HistoricalOrReattached,
        ] {
            with_view(
                "Write",
                json!({"content":"界"}),
                InvocationStatus::Success,
                Some(output("", WriteDiffState::NotCaptured { reason })),
                |view| {
                    assert_eq!(
                        copy_text(&view, ToolTab::Diff).unwrap(),
                        text(&view, ToolTab::Diff)
                    );
                },
            );
        }
        for diff in [
            WriteDiffState::Diff {
                lines: vec![],
                more: 0,
            },
            WriteDiffState::Diff {
                lines: vec![
                    DiffLine {
                        kind: DiffKind::Added,
                        content: "\u{1b}[31m界\r\t".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Removed,
                        content: "old".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Context,
                        content: "same".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Elided,
                        content: "… 3 more lines".into(),
                    },
                ],
                more: 3,
            },
        ] {
            with_view(
                "Write",
                json!({}),
                InvocationStatus::Success,
                Some(output("", diff)),
                |view| {
                    assert_eq!(
                        copy_text(&view, ToolTab::Diff).unwrap(),
                        text(&view, ToolTab::Diff)
                    );
                },
            );
        }
    }

    #[test]
    fn story21_5_bounded_copy_uses_the_original_source_availability_guards() {
        for status in [
            InvocationStatus::Pending,
            InvocationStatus::Running,
            InvocationStatus::Error,
            InvocationStatus::Cancelled,
        ] {
            with_view(
                "Write",
                json!({"content":"new"}),
                status,
                Some(output("result", WriteDiffState::NewFile)),
                |view| {
                    assert!(copy_text(&view, ToolTab::Diff).unwrap().is_empty());
                },
            );
        }
        for (name, args, diff) in [
            (
                "Edit",
                json!({"old_string":"","new_string":"new"}),
                WriteDiffState::NotAWrite,
            ),
            (
                "Edit",
                json!({"old_string":"same","new_string":"same"}),
                WriteDiffState::NotAWrite,
            ),
            (
                "Edit",
                json!({"old_string":"old"}),
                WriteDiffState::NotAWrite,
            ),
            ("Write", json!({}), WriteDiffState::NewFile),
            ("Write", json!({"content":"new"}), WriteDiffState::NotAWrite),
            ("Bash", json!({"content":"new"}), WriteDiffState::NewFile),
        ] {
            with_view(
                name,
                args,
                InvocationStatus::Success,
                Some(output("", diff)),
                |view| {
                    assert!(copy_text(&view, ToolTab::Diff).unwrap().is_empty());
                },
            );
        }
        let mut failed = output("failed", WriteDiffState::NewFile);
        failed.is_error = true;
        with_view(
            "Write",
            json!({"content":"new"}),
            InvocationStatus::Success,
            Some(failed),
            |view| assert!(copy_text(&view, ToolTab::Diff).unwrap().is_empty()),
        );
        for status in [
            InvocationStatus::Success,
            InvocationStatus::Error,
            InvocationStatus::Cancelled,
        ] {
            with_view("Bash", json!({}), status, None, |mut view| {
                view.tail = Some("not running");
                assert!(copy_text(&view, ToolTab::Output).unwrap().is_empty());
            });
        }
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("", WriteDiffState::NotAWrite)),
            |mut view| {
                view.tail = Some("not the empty result's source");
                assert!(copy_text(&view, ToolTab::Output).unwrap().is_empty());
            },
        );
    }

    fn rendered(view: &ToolView<'_>, tab: ToolTab) -> Vec<String> {
        render(view, tab, 200, &Theme::dark(), 2_500)
            .iter()
            .map(Line::to_string)
            .collect()
    }

    #[test]
    fn story21_5_edit_hunk_preserves_order_colors_and_output_source() {
        with_view(
            "Edit",
            json!({"old_string":"a\nb", "new_string":"a\nc"}),
            InvocationStatus::Success,
            Some(output("Successfully edited", WriteDiffState::NotAWrite)),
            |view| {
                assert_eq!(available_tabs(&view), DIFF_TABS);
                assert_eq!(rendered(&view, ToolTab::Diff), ["  a", "- b", "+ c"]);
                assert_eq!(text(&view, ToolTab::Diff), "  a\n- b\n+ c");
                assert_eq!(text(&view, ToolTab::Output), "Successfully edited");
                let theme = Theme::dark();
                let lines = render(&view, ToolTab::Diff, 80, &theme, 2_500);
                for (line, color) in lines.iter().zip([
                    theme.colors.fg_secondary,
                    theme.colors.error,
                    theme.colors.success,
                ]) {
                    assert_eq!(line.spans[0].style.fg, Some(color));
                }
            },
        );
    }

    #[test]
    fn story21_5_write_new_overwrite_and_cap_preserve_diff_rows() {
        for (args, diff, expected) in [
            (
                json!({"content":"fn a() {}\nfn b() {}"}),
                WriteDiffState::NewFile,
                vec!["+ fn a() {}", "+ fn b() {}"],
            ),
            (
                json!({"content":"x\nz"}),
                WriteDiffState::from_original(b"x\ny", "x\nz"),
                vec!["  x", "- y", "+ z"],
            ),
        ] {
            with_view(
                "Write",
                args,
                InvocationStatus::Success,
                Some(output("wrote", diff)),
                |view| {
                    assert_eq!(available_tabs(&view), DIFF_TABS);
                    assert_eq!(rendered(&view, ToolTab::Diff), expected);
                },
            );
        }
        let content: String = (0..500).map(|i| format!("line{i}\n")).collect();
        with_view(
            "Write",
            json!({"content":content}),
            InvocationStatus::Success,
            Some(output("wrote", WriteDiffState::NewFile)),
            |view| {
                let rows = rendered(&view, ToolTab::Diff);
                assert_eq!(
                    rows.iter().filter(|row| row.starts_with("+ ")).count(),
                    DIFF_MAX_LINES
                );
                assert!(rows.iter().any(|row| row == "… 300 more lines"));
                assert_eq!(rows.len(), DIFF_MAX_LINES + 1);
            },
        );
    }

    #[test]
    fn story21_5_write_no_change_and_missing_capture_are_honest() {
        let reasons = [
            NotCapturedReason::NoActiveCheckpoint,
            NotCapturedReason::SnapshotUnavailable,
            NotCapturedReason::SnapshotReadFailed,
            NotCapturedReason::OriginalNotUtf8,
            NotCapturedReason::SupersededInBatch,
            NotCapturedReason::HistoricalOrReattached,
        ];
        for reason in reasons {
            with_view(
                "Write",
                json!({"content":"界"}),
                InvocationStatus::Success,
                Some(output("wrote", WriteDiffState::NotCaptured { reason })),
                |view| {
                    let verb = if reason == NotCapturedReason::NoActiveCheckpoint {
                        "overwrote"
                    } else {
                        "wrote"
                    };
                    let expected = format!(
                        "{verb} 3 bytes — previous content not captured ({})",
                        reason.describe()
                    );
                    assert_eq!(text(&view, ToolTab::Diff), expected);
                    assert_eq!(rendered(&view, ToolTab::Diff), [expected]);
                    assert_eq!(available_tabs(&view), DIFF_TABS);
                    let theme = Theme::dark();
                    let rows = render(&view, ToolTab::Diff, 80, &theme, 0);
                    assert!(
                        rows[0]
                            .spans
                            .iter()
                            .all(|span| span.style.fg == Some(theme.colors.fg_muted))
                    );
                },
            );
        }
        for diff in [
            WriteDiffState::Diff {
                lines: Vec::new(),
                more: 0,
            },
            WriteDiffState::NewFile,
        ] {
            with_view(
                "Write",
                json!({"content":""}),
                InvocationStatus::Success,
                Some(output("", diff)),
                |view| {
                    assert_eq!(text(&view, ToolTab::Diff), NO_CHANGE);
                    assert_eq!(rendered(&view, ToolTab::Diff), [NO_CHANGE]);
                },
            );
        }
    }

    #[test]
    fn story21_5_diff_tab_requires_success_valid_input_and_body() {
        for name in ["Edit", "Write", "Bash"] {
            for status in [
                InvocationStatus::Pending,
                InvocationStatus::Running,
                InvocationStatus::Error,
                InvocationStatus::Cancelled,
            ] {
                with_view(
                    name,
                    json!({"old_string":"old", "new_string":"new", "content":"new"}),
                    status,
                    Some(output("result", WriteDiffState::NewFile)),
                    |view| {
                        assert_eq!(available_tabs(&view), BASE_TABS);
                        assert!(text(&view, ToolTab::Diff).is_empty());
                    },
                );
            }
        }
        for (name, args, diff) in [
            ("Bash", json!({}), WriteDiffState::NotAWrite),
            (
                "Edit",
                json!({"old_string":"", "new_string":"new"}),
                WriteDiffState::NotAWrite,
            ),
            (
                "Edit",
                json!({"old_string":"same", "new_string":"same"}),
                WriteDiffState::NotAWrite,
            ),
            (
                "Edit",
                json!({"old_string":"old"}),
                WriteDiffState::NotAWrite,
            ),
            ("Write", json!({"content":"new"}), WriteDiffState::NotAWrite),
            ("Write", json!({}), WriteDiffState::NewFile),
        ] {
            with_view(
                name,
                args,
                InvocationStatus::Success,
                Some(output("result", diff)),
                |view| {
                    assert_eq!(available_tabs(&view), BASE_TABS);
                },
            );
        }
        let mut failed = output("failed", WriteDiffState::NewFile);
        failed.is_error = true;
        with_view(
            "Write",
            json!({"content":"new"}),
            InvocationStatus::Success,
            Some(failed),
            |view| {
                assert_eq!(available_tabs(&view), BASE_TABS);
            },
        );
        with_view(
            "Edit",
            json!({"old_string":"old", "new_string":"new"}),
            InvocationStatus::Success,
            None,
            |view| {
                assert_eq!(available_tabs(&view), BASE_TABS);
            },
        );
    }

    #[test]
    fn story21_5_literal_display_is_sanitized_original_sources_are_not() {
        let original = "# literal * |\r\n\x1b[31mred\x1b[0m\tx\u{202e}evil\x07\n界e\u{301}👩‍💻";
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output(original, WriteDiffState::NotAWrite)),
            |view| {
                assert!(matches!(text(&view, ToolTab::Output), Cow::Borrowed(_)));
                assert_eq!(text(&view, ToolTab::Output), original);
                let rows = rendered(&view, ToolTab::Output).join("\n");
                for control in ['\x1b', '\x07', '\u{202e}', '\t', '\r'] {
                    assert!(!rows.contains(control), "{rows:?}");
                }
                assert!(rows.contains("# literal * |\nred"));
                assert!(rows.contains("界e\u{301}👩‍💻"));
                let safe = sanitize_tool_text_block(original);
                assert_eq!(
                    render(&view, ToolTab::Output, 200, &Theme::dark(), 0),
                    markdown::render(&safe, 200, &Theme::dark(), &RenderOptions::literal()).lines
                );
            },
        );
    }

    #[test]
    fn story21_5_empty_notices_never_replace_original_bytes() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Running,
            None,
            |mut view| {
                assert_eq!(text(&view, ToolTab::Output), "");
                assert_eq!(
                    display_text(&view, ToolTab::Output, 2_500),
                    "No output yet — running (1.5s)"
                );
                assert_eq!(display_text(&view, ToolTab::Input, 0), "No input.");
                assert_eq!(text(&view, ToolTab::Input), "{}");
                view.tail = Some("A1\nA2");
                assert_eq!(text(&view, ToolTab::Output), "A1\nA2");
                assert_eq!(
                    display_text(&view, ToolTab::Output, 0),
                    "Live tail (last 2 lines) — the full output appears when the tool finishes\nA1\nA2"
                );
            },
        );
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("", WriteDiffState::NotAWrite)),
            |view| {
                assert_eq!(text(&view, ToolTab::Output), "");
                assert_eq!(
                    display_text(&view, ToolTab::Output, 0),
                    "The tool returned no output."
                );
            },
        );
    }

    #[test]
    fn story21_5_raw_is_exact_projected_record_and_revision_tracks_sources() {
        with_view(
            "Bash",
            json!({"command":"printf hi"}),
            InvocationStatus::Success,
            Some(output("hi\u{1b}[31m", WriteDiffState::NotAWrite)),
            |mut view| {
                let raw: serde_json::Value =
                    serde_json::from_str(&text(&view, ToolTab::Raw)).unwrap();
                assert_eq!(
                    raw,
                    json!({
                        "id":"tc_turn_2", "tool":"Bash", "status":"success",
                        "input":{"command":"printf hi"},
                        "result":{"content":"hi\u{1b}[31m", "is_error":false},
                        "started_at":1000, "ended_at":2000,
                    })
                );
                let revisions = [ToolTab::Output, ToolTab::Input, ToolTab::Raw]
                    .map(|tab| prepare(&view, tab, 0).hash());
                view.tail = Some("irrelevant after completion");
                assert_eq!(
                    revisions,
                    [ToolTab::Output, ToolTab::Input, ToolTab::Raw]
                        .map(|tab| prepare(&view, tab, 0).hash())
                );
                let changed = output("changed", WriteDiffState::NotAWrite);
                let view = ToolView {
                    result: Some(&changed),
                    ..view
                };
                assert_ne!(revisions[0], prepare(&view, ToolTab::Output, 0).hash());
                assert_eq!(revisions[1], prepare(&view, ToolTab::Input, 0).hash());
                assert_ne!(revisions[2], prepare(&view, ToolTab::Raw, 0).hash());
            },
        );
        with_view(
            "Bash",
            json!(null),
            InvocationStatus::Running,
            None,
            |view| {
                let raw: serde_json::Value =
                    serde_json::from_str(&text(&view, ToolTab::Raw)).unwrap();
                assert!(raw["result"].is_null());
            },
        );
        // Private borrowed serialization must remain value-equivalent to A3's
        // json! projection, including large Unicode results and absent fields.
        let large = "界e\u{301}👩‍💻\r\n".repeat(4096);
        for (status, content) in [
            (InvocationStatus::Pending, None),
            (InvocationStatus::Running, None),
            (InvocationStatus::Success, Some(large.as_str())),
            (
                InvocationStatus::Error,
                Some("error \"quoted\"\n\u{1b}[31m"),
            ),
            (InvocationStatus::Cancelled, Some("")),
        ] {
            let mut result = content.map(|text| output(text, WriteDiffState::NotAWrite));
            if let Some(result) = &mut result {
                result.is_error = status == InvocationStatus::Error;
            }
            with_view(
                "工具👩‍💻",
                json!({"unicode":"界e\u{301}👩‍💻", "nested":[null, true, 42], "crlf":"a\r\nb"}),
                status,
                result,
                |mut view| {
                    if view.is_running() {
                        view.ended_at = None;
                    }
                    let baseline = json!({
                        "id":view.target.tool_call_id, "tool":view.name, "status":view.status,
                        "input":view.args,
                        "result":view.result.map(|r| json!({"content":r.content, "is_error":r.is_error})),
                        "started_at":view.started_at, "ended_at":view.ended_at,
                    });
                    let raw = text(&view, ToolTab::Raw);
                    assert_eq!(copy_text(&view, ToolTab::Raw).unwrap(), raw);
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
                        baseline
                    );
                    assert!(raw.find("\"tool\":").unwrap() < raw.find("\"result\":").unwrap());
                },
            );
        }
    }

    #[test]
    fn story21_5_diff_sanitizes_and_expands_tabs_without_mutating_source() {
        let diff = vec![DiffLine {
            kind: DiffKind::Added,
            content: "\x1b[31mx\t界\u{2066}".into(),
        }];
        let rows = render_diff_lines(&diff, &Theme::dark());
        assert_eq!(rows[0].to_string(), "+ x     界");
        assert_eq!(diff_text(&diff), "+ \x1b[31mx\t界\u{2066}");
    }

    #[test]
    fn story21_5_diff_rows_stay_borrowed_and_fold_height_matches_body() {
        let args = json!({"content": "after"});
        let diff = WriteDiffState::from_original(b"before", "after");
        let body = diff_body("write", &args, &diff).unwrap();
        let DiffBody::Lines { lines } = &body else {
            panic!("site-supplied lines");
        };
        let WriteDiffState::Diff {
            lines: original, ..
        } = &diff
        else {
            panic!("site-supplied diff");
        };
        assert!(std::ptr::eq(lines.as_ref(), original.as_slice()));
        assert_eq!(
            body.row_count(),
            render_diff_lines(original, &Theme::dark()).len()
        );
        assert_eq!(DiffBody::NoChange.row_count(), 1);
        assert_eq!(
            DiffBody::NotCaptured {
                bytes: 10,
                reason: NotCapturedReason::SnapshotReadFailed,
            }
            .row_count(),
            1,
        );
    }

    #[test]
    fn story21_5_line_ending_change_and_elision_keep_honest_markers() {
        with_view(
            "edit",
            json!({"old_string": "same\n", "new_string": "same"}),
            InvocationStatus::Success,
            Some(output("edited", WriteDiffState::NotAWrite)),
            |view| {
                assert_eq!(available_tabs(&view), DIFF_TABS);
                assert_eq!(
                    rendered(&view, ToolTab::Diff),
                    ["⚠ line endings or trailing newline changed — no line content changed"],
                );
            },
        );
        let original: String = (0..40).map(|i| format!("line{i}\n")).collect();
        let changed = original.replace("line20\n", "changed\n");
        with_view(
            "Write",
            json!({"content": changed}),
            InvocationStatus::Success,
            Some(output(
                "wrote",
                WriteDiffState::from_original(original.as_bytes(), &changed),
            )),
            |view| {
                let rows = rendered(&view, ToolTab::Diff);
                assert!(rows.iter().any(|row| row.starts_with("⋯ ")));
                assert!(rows.iter().any(|row| row == "- line20"));
                assert!(rows.iter().any(|row| row == "+ changed"));
                assert!(!rows.iter().any(|row| row.starts_with("  ⋯ ")));
            },
        );
    }

    #[test]
    fn story21_5_tail_revision_is_call_source_only_and_input_revision_changes() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Running,
            None,
            |mut view| {
                let output_revision = prepare(&view, ToolTab::Output, 0).hash();
                let raw_revision = prepare(&view, ToolTab::Raw, 0).hash();
                let input_revision = prepare(&view, ToolTab::Input, 0).hash();
                view.tail = Some("this call's live output");
                assert_ne!(prepare(&view, ToolTab::Output, 0).hash(), output_revision);
                assert_eq!(prepare(&view, ToolTab::Raw, 0).hash(), raw_revision);
                assert_eq!(prepare(&view, ToolTab::Input, 0).hash(), input_revision);
                let args = json!({"command": "changed"});
                let view = ToolView {
                    args: &args,
                    ..view
                };
                assert_ne!(prepare(&view, ToolTab::Input, 0).hash(), input_revision);
                assert_ne!(prepare(&view, ToolTab::Raw, 0).hash(), raw_revision);
            },
        );
    }

    #[test]
    fn story21_5_prepared_source_borrows_clean_output_and_hashes_sanitized_bytes() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("plain", WriteDiffState::NotAWrite)),
            |view| {
                let prepared = prepare(&view, ToolTab::Output, 0);
                assert!(matches!(
                    &prepared.body,
                    PreparedBody::Literal(Cow::Borrowed("plain"))
                ));
                let changed = output("\x1b[31mplain\x1b[0m", WriteDiffState::NotAWrite);
                let changed_view = ToolView {
                    result: Some(&changed),
                    source_revision: view.source_revision + 1,
                    ..view
                };
                assert_ne!(
                    revision(&view, ToolTab::Output),
                    revision(&changed_view, ToolTab::Output),
                );
                let sanitized = prepare(&changed_view, ToolTab::Output, 0);
                assert_eq!(prepared.hash(), sanitized.hash());
                assert_eq!(
                    prepared.render(80, &Theme::dark()).lines,
                    sanitized.render(80, &Theme::dark()).lines,
                );
            },
        );
    }

    #[test]
    fn story21_5_prepared_json_renders_once_without_repreparing() {
        with_view(
            "Bash",
            json!({"command": "echo hi"}),
            InvocationStatus::Success,
            Some(output("hi", WriteDiffState::NotAWrite)),
            |view| {
                let prepared = prepare(&view, ToolTab::Raw, 0);
                let PreparedBody::Literal(Cow::Owned(json)) = &prepared.body else {
                    panic!("pretty record owns its serialized allocation");
                };
                let record: serde_json::Value = serde_json::from_str(json).unwrap();
                assert_eq!(record["result"]["content"], "hi");
                let renders = markdown::metrics::renders();
                let lines = prepared.render(80, &Theme::dark()).lines;
                assert_eq!(markdown::metrics::renders(), renders + 1);
                assert!(!lines.is_empty());
                assert_eq!(lines, render(&view, ToolTab::Raw, 80, &Theme::dark(), 0),);
            },
        );
    }

    #[test]
    fn story21_5_prepared_diff_hash_sanitizes_rows_and_retains_style() {
        with_view(
            "Write",
            json!({"content": "plain"}),
            InvocationStatus::Success,
            Some(output(
                "wrote",
                WriteDiffState::Diff {
                    lines: vec![DiffLine {
                        kind: DiffKind::Added,
                        content: "plain".into(),
                    }],
                    more: 0,
                },
            )),
            |view| {
                let clean = prepare(&view, ToolTab::Diff, 0);
                let changed = output(
                    "wrote",
                    WriteDiffState::Diff {
                        lines: vec![DiffLine {
                            kind: DiffKind::Added,
                            content: "\x1b[31mplain\x1b[0m".into(),
                        }],
                        more: 0,
                    },
                );
                let changed_view = ToolView {
                    result: Some(&changed),
                    ..view
                };
                let sanitized = prepare(&changed_view, ToolTab::Diff, 0);
                assert_eq!(clean.hash(), sanitized.hash());
                assert_eq!(
                    clean.render(80, &Theme::dark()).lines,
                    sanitized.render(80, &Theme::dark()).lines,
                );
                let removed = output(
                    "wrote",
                    WriteDiffState::Diff {
                        lines: vec![DiffLine {
                            kind: DiffKind::Removed,
                            content: "plain".into(),
                        }],
                        more: 0,
                    },
                );
                let removed_view = ToolView {
                    result: Some(&removed),
                    ..view
                };
                assert_ne!(
                    clean.hash(),
                    prepare(&removed_view, ToolTab::Diff, 0).hash()
                );
            },
        );
    }

    #[test]
    fn story21_5_logical_text_reuses_prepared_sources_and_preserves_copy_bytes() {
        let original = "\x1b[31mred\x1b[0m\t界\r\nnext\u{202e}";
        with_view(
            "Bash",
            json!({"command": "echo hi"}),
            InvocationStatus::Success,
            Some(output(original, WriteDiffState::NotAWrite)),
            |view| {
                let prepared = prepare(&view, ToolTab::Output, 0);
                assert_eq!(prepared.logical_text(), "red\t界\nnext");
                assert_eq!(text(&view, ToolTab::Output), original);
                for tab in [ToolTab::Input, ToolTab::Raw] {
                    let prepared = prepare(&view, tab, 0);
                    let PreparedBody::Literal(Cow::Owned(serialized)) = &prepared.body else {
                        panic!("JSON owns its single prepared serialization");
                    };
                    let counts = metrics::counts();
                    assert_eq!(prepared.logical_text().as_ptr(), serialized.as_ptr());
                    let original_source = text(&view, tab);
                    assert_eq!(
                        prepared.logical_text(),
                        sanitize_tool_text_block(&original_source).as_ref()
                    );
                    if tab == ToolTab::Raw {
                        assert!(
                            original_source.contains('\u{202e}'),
                            "copy projection keeps original bytes"
                        );
                        assert!(
                            !prepared.logical_text().contains('\u{202e}'),
                            "display strips bidi controls"
                        );
                    }
                    assert_eq!(metrics::counts(), counts);
                }
            },
        );
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("clean\t界\n", WriteDiffState::NotAWrite)),
            |view| {
                let prepared = prepare(&view, ToolTab::Output, 0);
                assert_eq!(
                    prepared.logical_text().as_ptr(),
                    view.result.unwrap().content.as_ptr()
                );
            },
        );
    }

    #[test]
    fn story21_5_mapped_literal_rows_use_normalized_line_byte_ranges() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output(
                "abcdefghij\r\n界e\u{301}\n",
                WriteDiffState::NotAWrite,
            )),
            |view| {
                let prepared = prepare(&view, ToolTab::Output, 0);
                let rendered = prepared.render(4, &Theme::dark());
                assert_eq!(
                    rendered
                        .lines
                        .iter()
                        .map(Line::to_string)
                        .collect::<Vec<_>>(),
                    ["abcd", "efgh", "ij", "界e\u{301}", ""],
                );
                assert_eq!(rendered.origins.len(), rendered.lines.len());
                for (origin, (logical_line, bytes)) in
                    rendered.origins[..4]
                        .iter()
                        .zip([(0, 0..4), (0, 4..8), (0, 8..10), (1, 0..6)])
                {
                    let origin = origin.as_ref().expect("source row");
                    assert_eq!(origin.logical_line, logical_line);
                    assert_eq!(origin.bytes, bytes);
                }
                assert!(rendered.origins[4].is_none());
                let wide = prepared.render(usize::from(u16::MAX) + 1, &Theme::dark());
                assert_eq!(wide.lines.len(), 3);
                let renders = markdown::metrics::renders();
                let empty = prepared.render(0, &Theme::dark());
                assert!(empty.lines.is_empty());
                assert!(empty.origins.is_empty());
                assert_eq!(markdown::metrics::renders(), renders);
            },
        );
    }

    #[test]
    fn story21_5_mapped_diff_wraps_once_and_keeps_multiline_kind_colors() {
        let diff = WriteDiffState::Diff {
            lines: vec![
                DiffLine {
                    kind: DiffKind::Added,
                    content: "\x1b[31malpha\t界\r\nsecond\x1b[0m".into(),
                },
                DiffLine {
                    kind: DiffKind::Removed,
                    content: "third".into(),
                },
            ],
            more: 0,
        };
        with_view(
            "Write",
            json!({"content": "after"}),
            InvocationStatus::Success,
            Some(output("wrote", diff)),
            |view| {
                let prepared = prepare(&view, ToolTab::Diff, 0);
                assert_eq!(prepared.logical_text(), "+ alpha\t界\nsecond\n- third");
                assert!(text(&view, ToolTab::Diff).contains('\x1b'));
                let theme = Theme::dark();
                let renders = markdown::metrics::renders();
                let counts = metrics::counts();
                let rendered = prepared.render(8, &theme);
                assert_eq!(markdown::metrics::renders(), renders + 1);
                assert_eq!(metrics::counts(), counts);
                assert_eq!(rendered.lines.len(), rendered.origins.len());
                assert!(rendered.lines.len() > 3, "the added row wraps");
                for (row, origin) in rendered.lines.iter().zip(&rendered.origins) {
                    let origin = origin.as_ref().expect("no paragraph blank in Diff");
                    let color = match origin.logical_line {
                        0 | 1 => theme.colors.success,
                        2 => theme.colors.error,
                        line => panic!("unexpected logical line {line}"),
                    };
                    assert!(row.spans.iter().all(|span| span.style.fg == Some(color)));
                }
                let wide = prepared.render(200, &theme);
                assert_eq!(wide.lines.len(), 3);
                assert!(wide.origins.iter().all(Option::is_some));
            },
        );
    }

    #[test]
    fn story21_5_mapped_notices_and_live_tail_keep_display_provenance() {
        with_view(
            "Write",
            json!({"content": "new"}),
            InvocationStatus::Success,
            Some(output(
                "wrote",
                WriteDiffState::NotCaptured {
                    reason: NotCapturedReason::SnapshotUnavailable,
                },
            )),
            |view| {
                let prepared = prepare(&view, ToolTab::Diff, 0);
                assert_eq!(
                    prepared.logical_text(),
                    not_captured(3, NotCapturedReason::SnapshotUnavailable)
                );
                let theme = Theme::dark();
                let rendered = prepared.render(20, &theme);
                assert!(rendered.lines.len() > 1);
                assert!(rendered.origins.iter().all(Option::is_some));
                for row in &rendered.lines {
                    assert!(
                        row.spans
                            .iter()
                            .all(|span| span.style.fg == Some(theme.colors.fg_muted))
                    );
                }
            },
        );
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Running,
            None,
            |mut view| {
                view.tail = Some("first\nsecond");
                let prepared = prepare(&view, ToolTab::Output, 0);
                assert_eq!(
                    prepared.logical_text(),
                    display_text(&view, ToolTab::Output, 0)
                );
                let theme = Theme::dark();
                let rendered = prepared.render(200, &theme);
                assert_eq!(rendered.lines.len(), 4);
                assert!(
                    rendered.lines[0]
                        .spans
                        .iter()
                        .all(|span| span.style.fg == Some(theme.colors.fg_muted))
                );
                assert_eq!(rendered.origins[2].as_ref().unwrap().logical_line, 2);
                assert!(rendered.origins[3].is_none());
            },
        );
    }
}
