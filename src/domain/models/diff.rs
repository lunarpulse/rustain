//! Line-diff model for Write/Edit tool-block display (Story 19.1, FR30).
//!
//! Pure domain computation — no ratatui, no I/O. The rendering half
//! (`render_diff_lines`) lives in the TUI widget (`tool_block.rs`); this
//! module holds the data shape because [`WriteDiffState`] persists with the
//! conversation (UI-only — never on the provider-facing `ToolResult`).
//!
//! ## Why the state is an enum and not `Option<Vec<DiffLine>>`
//!
//! Code review of 19.1 (Decision 2, Code Review Crew unanimous 4/4) found the
//! original `Option<Vec<DiffLine>>` channel encoded THREE meanings in two
//! unambiguous slots: `None` conflated "not a Write" / "new file" / "recorded
//! before this feature" / "re-projected on attach", and `Some(vec![])` was used
//! as a "no snapshot" sentinel even though `compute_diff` legitimately returns
//! an empty vector. Both collisions were reachable and both made the widget
//! state something false, which is exactly what ruling A4 forbids. The states
//! are now distinct values, so no consumer has to guess and every "no diff"
//! render can name its real reason.

use std::borrow::Cow;
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Maximum diff lines a tool block shows before the `… N more lines` marker
/// (ruling A6). Applied at the producing site as well as the widget so the
/// journal never carries an unbounded diff.
pub const DIFF_MAX_LINES: usize = 200;

/// Unchanged lines kept on each side of a change when eliding (classic
/// unified-diff context).
pub const DIFF_CONTEXT_LINES: usize = 3;

/// Cell budget for the LCS table. Exceeding it degrades the *changed middle*
/// to all-removed + all-added; the common prefix/suffix are already trimmed by
/// then, so a large file with a small change never reaches this.
const LCS_MAX_CELLS: usize = 4_000_000;

/// Kind of a single diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Context,
    /// A collapsed run of unchanged lines, or a note about a change a line
    /// diff cannot show. `content` is the already-rendered muted text.
    Elided,
}

/// One line of a line-level diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub content: String,
}

#[cfg(test)]
impl DiffLine {
    fn of(kind: DiffKind, content: impl Into<String>) -> Self {
        DiffLine {
            kind,
            content: content.into(),
        }
    }

    /// Whether this line represents an actual content change.
    pub fn is_change(&self) -> bool {
        matches!(self.kind, DiffKind::Added | DiffKind::Removed)
    }
}

/// A display row borrowing source text; only generated markers need ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLineRef<'a> {
    pub kind: DiffKind,
    pub content: Cow<'a, str>,
}

impl DiffLineRef<'_> {
    pub fn into_owned(self) -> DiffLine {
        DiffLine {
            kind: self.kind,
            content: self.content.into_owned(),
        }
    }
}

#[cfg(test)]
impl From<DiffLine> for DiffLineRef<'_> {
    fn from(line: DiffLine) -> Self {
        Self {
            kind: line.kind,
            content: Cow::Owned(line.content),
        }
    }
}

/// Why a completed Write has no line diff to show.
///
/// Every variant renders its own parenthetical, so the widget can never
/// claim "no active checkpoint" for a failure that had nothing to do with
/// checkpoints (code review finding: that false statement was reachable
/// through a snapshot read error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotCapturedReason {
    /// No checkpoint was active, so `execute_write` took no snapshot. This is
    /// the case ruling A4 and AC4 describe.
    NoActiveCheckpoint,
    /// A checkpoint was active but no snapshot exists for this path.
    SnapshotUnavailable,
    /// The snapshot exists but could not be read or decoded.
    SnapshotReadFailed,
    /// The original is not valid UTF-8, so a line diff would be meaningless.
    OriginalNotUtf8,
    /// An earlier Write to this same path in the same checkpoint already owns
    /// the snapshot, so the original for *this* call cannot be established.
    /// Snapshots are first-write-wins per `(checkpoint, path)`.
    SupersededInBatch,
    /// Recorded before Story 19.1, or re-projected by a client that did not
    /// carry the field. Provenance is unknown, so nothing may be inferred.
    HistoricalOrReattached,
}

impl NotCapturedReason {
    /// Parenthetical shown after `previous content not captured`.
    pub fn describe(self) -> &'static str {
        match self {
            NotCapturedReason::NoActiveCheckpoint => "no active checkpoint",
            NotCapturedReason::SnapshotUnavailable => "snapshot unavailable",
            NotCapturedReason::SnapshotReadFailed => "snapshot read failed",
            NotCapturedReason::OriginalNotUtf8 => "original is not valid UTF-8",
            NotCapturedReason::SupersededInBatch => "superseded by another write in the same batch",
            NotCapturedReason::HistoricalOrReattached => {
                "recorded before this feature, or re-projected on attach"
            }
        }
    }
}

/// Display-diff state riding a tool result (Story 19.1, FR30).
///
/// UI-only: it is journaled and persisted with the conversation but never
/// reaches the provider-facing `ToolResult` (ruling A5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WriteDiffState {
    /// Not a Write. Edit derives its hunk from its own input at the widget
    /// (ruling A2); every other tool renders its output text as before.
    NotAWrite,
    /// The Write created the file (or replaced an empty one), so every line of
    /// the new content is an addition and there is nothing to remove. The
    /// widget derives the lines from the tool input — no plumbing needed.
    NewFile,
    /// A real diff against the captured original, already elided and capped at
    /// [`DIFF_MAX_LINES`]; `more` counts lines dropped by the cap.
    ///
    /// An empty `lines` with `more == 0` is a legitimate value meaning "the
    /// write changed nothing" — it is NOT a sentinel for missing capture.
    Diff { lines: Vec<DiffLine>, more: usize },
    /// The write completed but the original could not be established. The
    /// widget renders one honest line naming `reason` and never guesses.
    NotCaptured { reason: NotCapturedReason },
}

/// A missing field on a persisted or re-projected record means the provenance
/// is unknown — never that the file was new. Fabricating `NewFile` here is
/// what painted an all-additions diff over a replaced file.
impl Default for WriteDiffState {
    fn default() -> Self {
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::HistoricalOrReattached,
        }
    }
}

impl WriteDiffState {
    /// Build the state for a completed Write whose original content is known.
    ///
    /// Handles the honest edge cases the review found: a non-UTF8 original
    /// cannot be line-diffed, and an empty original is genuinely a new-file
    /// render rather than a diff with nothing on the left.
    pub fn from_original(original: &[u8], new_content: &str) -> Self {
        if original.is_empty() {
            return WriteDiffState::NewFile;
        }
        let Ok(original) = std::str::from_utf8(original) else {
            return WriteDiffState::NotCaptured {
                reason: NotCapturedReason::OriginalNotUtf8,
            };
        };
        let (lines, more) = capped_display_diff(original, new_content, DIFF_MAX_LINES);
        WriteDiffState::Diff {
            lines: lines.into_iter().map(DiffLineRef::into_owned).collect(),
            more,
        }
    }
}

/// Compute the diff a tool block should show: a line diff, annotated when the
/// change is invisible to a line diff, with long unchanged runs collapsed.
///
/// Elision retains nearby changes before the head-and-tail display cap is
/// applied, so changes after a long common prefix remain visible.
#[cfg(test)]
pub fn display_diff(original: &str, new_content: &str) -> Vec<DiffLine> {
    capped_display_diff(original, new_content, usize::MAX)
        .0
        .into_iter()
        .map(DiffLineRef::into_owned)
        .collect()
}

/// Derive the canonical display diff without owning source text or retaining
/// uncapped rows. The DP cell count is capped; a highly asymmetric changed
/// middle can still retain many borrowed line references within that budget.
pub fn capped_display_diff<'a>(
    original: &'a str,
    new_content: &'a str,
    max: usize,
) -> (Vec<DiffLineRef<'a>>, usize) {
    if original == new_content {
        return (Vec::new(), 0);
    }
    let mut rows = CappedRows::new(max);
    let mut elision = Elision::new(DIFF_CONTEXT_LINES, |line| rows.push(line));
    if visit_diff(original, new_content, |line| elision.push(line)) {
        elision.finish();
    } else {
        drop(elision);
        rows.push(DiffLineRef {
            kind: DiffKind::Elided,
            content: Cow::Borrowed(
                "⚠ line endings or trailing newline changed — no line content changed",
            ),
        });
    }
    rows.finish()
}

/// Exact changed-line counts of one edit (Story 21.9 D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStat {
    pub added: usize,
    pub removed: usize,
}

/// Largest LCS table (cells) a per-frame stat may build: well under
/// [`LCS_MAX_CELLS`], so the count is always the exact line diff and one Stage
/// row never costs more than about a megabyte per paint.
const STAT_MAX_CELLS: usize = 250_000;

/// Count the added and removed rows of the line diff `original` → `new_content`
/// in one pass over [`visit_diff`]: no row is stored. `None` when the changed
/// middle is too large to count exactly within [`STAT_MAX_CELLS`]; a degraded
/// all-removed/all-added count is never returned as if it were exact.
pub fn diff_stat(original: &str, new_content: &str) -> Option<DiffStat> {
    let old_count = original.lines().count();
    let new_count = new_content.lines().count();
    let prefix = original
        .lines()
        .zip(new_content.lines())
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = original
        .lines()
        .rev()
        .take(old_count - prefix)
        .zip(new_content.lines().rev().take(new_count - prefix))
        .take_while(|(old, new)| old == new)
        .count();
    let (m, n) = (old_count - prefix - suffix, new_count - prefix - suffix);
    if m > 0 && n > 0 && (m + 1).saturating_mul(n + 1) > STAT_MAX_CELLS {
        return None;
    }
    let mut stat = DiffStat::default();
    visit_diff(original, new_content, |line| match line.kind {
        DiffKind::Added => stat.added += 1,
        DiffKind::Removed => stat.removed += 1,
        DiffKind::Context | DiffKind::Elided => {}
    });
    Some(stat)
}

/// Collapse runs of unchanged lines further than `context` from any change
/// into a single `Elided` marker.
#[cfg(test)]
pub fn elide_unchanged(lines: Vec<DiffLine>, context: usize) -> Vec<DiffLine> {
    if !lines.iter().any(DiffLine::is_change) {
        // Nothing changed: there is no hunk to centre context on, so keep the
        // annotation lines only and let the caller render an honest summary.
        return lines
            .into_iter()
            .filter(|l| l.kind == DiffKind::Elided)
            .collect();
    }
    let mut out = Vec::new();
    let mut elision = Elision::new(context, |line| out.push(line.into_owned()));
    for line in lines {
        elision.push(line.into());
    }
    elision.finish();
    out
}

/// Truncate to `max` content lines, returning the number dropped.
///
/// Takes a HEAD and a TAIL rather than just the head, with an inline marker
/// between them. A wholesale rewrite emits every removal before every
/// addition, so a head-only cap showed 200 red lines and hid every single
/// addition — the file looked deleted. Elision cannot help there: with no
/// common lines there are no unchanged runs to collapse.
#[cfg(test)]
pub fn cap_lines(lines: Vec<DiffLine>, max: usize) -> (Vec<DiffLine>, usize) {
    if lines.len() <= max {
        return (lines, 0);
    }
    let mut rows = CappedRows::new(max);
    for line in lines {
        rows.push(line.into());
    }
    let (rows, dropped) = rows.finish();
    (
        rows.into_iter().map(DiffLineRef::into_owned).collect(),
        dropped,
    )
}

struct CappedRows<'a> {
    head: Vec<DiffLineRef<'a>>,
    tail: VecDeque<DiffLineRef<'a>>,
    max: usize,
    count: usize,
}

impl<'a> CappedRows<'a> {
    fn new(max: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            max,
            count: 0,
        }
    }

    fn push(&mut self, line: DiffLineRef<'a>) {
        self.count += 1;
        if self.head.len() < self.max / 2 {
            self.head.push(line);
        } else if self.max != 0 {
            if self.tail.len() == self.max - self.max / 2 {
                self.tail.pop_front();
            }
            self.tail.push_back(line);
        }
    }

    fn finish(mut self) -> (Vec<DiffLineRef<'a>>, usize) {
        let dropped = self.count.saturating_sub(self.max);
        if dropped != 0 {
            self.head.push(DiffLineRef {
                kind: DiffKind::Elided,
                content: Cow::Owned(format!("… {} more lines", dropped)),
            });
        }
        self.head.extend(self.tail);
        (self.head, dropped)
    }
}

/// Pending context is bounded by `context`, even for an unchanged huge file.
struct Elision<'a, F> {
    emit: F,
    pending: VecDeque<DiffLineRef<'a>>,
    context: usize,
    dropped: usize,
    after: usize,
}

impl<'a, F: FnMut(DiffLineRef<'a>)> Elision<'a, F> {
    fn new(context: usize, emit: F) -> Self {
        Self {
            emit,
            pending: VecDeque::new(),
            context,
            dropped: 0,
            after: 0,
        }
    }

    fn marker(&mut self, count: usize) {
        if count != 0 {
            (self.emit)(DiffLineRef {
                kind: DiffKind::Elided,
                content: Cow::Owned(format!("⋯ {} unchanged lines", count)),
            });
        }
    }

    fn push(&mut self, line: DiffLineRef<'a>) {
        if line.kind == DiffKind::Context {
            if self.after != 0 {
                self.after -= 1;
                (self.emit)(line);
            } else {
                self.pending.push_back(line);
                if self.pending.len() > self.context {
                    self.pending.pop_front();
                    self.dropped += 1;
                }
            }
        } else {
            self.marker(self.dropped);
            self.dropped = 0;
            for line in self.pending.drain(..) {
                (self.emit)(line);
            }
            (self.emit)(line);
            self.after = self.context;
        }
    }

    fn finish(mut self) {
        self.marker(self.dropped + self.pending.len());
    }
}

/// Compute a line-by-line diff using longest common subsequence.
///
/// The common prefix and suffix are trimmed to `Context` first: it makes the
/// common "large file, small change" case linear, keeps the LCS table small
/// enough that the degrade branch is effectively unreachable in practice, and
/// yields the diff a reader expects.
#[cfg(test)]
pub fn compute_diff(original: &str, new_content: &str) -> Vec<DiffLine> {
    let mut result = Vec::new();
    visit_diff(original, new_content, |line| result.push(line.into_owned()));
    result
}

/// Visit raw rows in order, trimming before allocating any middle metadata.
/// Returns whether there was a line-content change (not just terminators).
fn visit_diff<'a>(
    original: &'a str,
    new_content: &'a str,
    mut emit: impl FnMut(DiffLineRef<'a>),
) -> bool {
    let old_count = original.lines().count();
    let new_count = new_content.lines().count();
    let prefix = original
        .lines()
        .zip(new_content.lines())
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = original
        .lines()
        .rev()
        .take(old_count - prefix)
        .zip(new_content.lines().rev().take(new_count - prefix))
        .take_while(|(old, new)| old == new)
        .count();
    let m = old_count - prefix - suffix;
    let n = new_count - prefix - suffix;
    let row = |kind, content| DiffLineRef {
        kind,
        content: Cow::Borrowed(content),
    };
    for line in original.lines().take(prefix) {
        emit(row(DiffKind::Context, line));
    }
    let old_mid = || original.lines().skip(prefix).take(m);
    let new_mid = || new_content.lines().skip(prefix).take(n);
    if m == 0 || n == 0 || (m + 1).saturating_mul(n + 1) > LCS_MAX_CELLS {
        for line in old_mid() {
            emit(row(DiffKind::Removed, line));
        }
        for line in new_mid() {
            emit(row(DiffKind::Added, line));
        }
    } else {
        diff_middle(
            &old_mid().collect::<Vec<_>>(),
            &new_mid().collect::<Vec<_>>(),
            &mut emit,
        );
    }
    for line in original.lines().skip(old_count - suffix) {
        emit(row(DiffKind::Context, line));
    }
    m != 0 || n != 0
}

/// Diff the changed middle once the common prefix/suffix are removed.
fn diff_middle<'a>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
    mut emit: impl FnMut(DiffLineRef<'a>),
) {
    let m = old_lines.len();
    let n = new_lines.len();

    // LCS table, flat: one allocation instead of m+1 nested Vecs.
    let stride = n + 1;
    let mut dp = vec![0u32; (m + 1) * stride];
    for i in 1..=m {
        for j in 1..=n {
            dp[i * stride + j] = if old_lines[i - 1] == new_lines[j - 1] {
                dp[(i - 1) * stride + (j - 1)] + 1
            } else {
                dp[(i - 1) * stride + j].max(dp[i * stride + (j - 1)])
            };
        }
    }

    let mut result = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_lines[i - 1] == new_lines[j - 1] {
            result.push(DiffLineRef {
                kind: DiffKind::Context,
                content: Cow::Borrowed(old_lines[i - 1]),
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i * stride + (j - 1)] >= dp[(i - 1) * stride + j]) {
            result.push(DiffLineRef {
                kind: DiffKind::Added,
                content: Cow::Borrowed(new_lines[j - 1]),
            });
            j -= 1;
        } else {
            result.push(DiffLineRef {
                kind: DiffKind::Removed,
                content: Cow::Borrowed(old_lines[i - 1]),
            });
            i -= 1;
        }
    }
    for line in result.into_iter().rev() {
        emit(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_new_file() {
        let diff = compute_diff("", "line1\nline2");
        assert_eq!(diff.len(), 2);
        assert!(diff.iter().all(|d| d.kind == DiffKind::Added));
    }

    #[test]
    fn test_diff_modification() {
        let diff = compute_diff(
            "fn main() {\n    println!(\"hello\");\n}",
            "fn main() {\n    println!(\"hello world\");\n    println!(\"goodbye\");\n}",
        );
        assert!(diff.iter().any(|d| d.kind == DiffKind::Added));
        assert!(diff.iter().any(|d| d.kind == DiffKind::Context));
    }

    /// The defect elision exists to fix: a change past the display cap used to
    /// be replaced by 200 unchanged lines, so the user saw no change at all.
    #[test]
    fn change_past_the_cap_is_still_visible() {
        let old: String = (0..300).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line250\n", "CHANGED\n");
        let (shown, _more) = cap_lines(display_diff(&old, &new), DIFF_MAX_LINES);
        assert!(
            shown
                .iter()
                .any(|l| l.kind == DiffKind::Added && l.content == "CHANGED"),
            "the addition must survive the cap: {shown:?}"
        );
        assert!(
            shown
                .iter()
                .any(|l| l.kind == DiffKind::Removed && l.content == "line250"),
            "the removal must survive the cap: {shown:?}"
        );
        assert!(shown.len() <= DIFF_MAX_LINES);
    }

    /// A large file whose middle is entirely rewritten still shows additions,
    /// never 200 removals and nothing else.
    #[test]
    fn wholesale_rewrite_shows_both_sides() {
        let old: String = (0..400).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..400).map(|i| format!("new{i}\n")).collect();
        let (shown, _more) = cap_lines(display_diff(&old, &new), DIFF_MAX_LINES);
        assert!(shown.iter().any(|l| l.kind == DiffKind::Removed));
        assert!(
            shown.iter().any(|l| l.kind == DiffKind::Added),
            "additions must not be hidden behind the removals: {shown:?}"
        );
    }

    #[test]
    fn terminator_only_change_is_named_not_silent() {
        let lines = display_diff("a", "a\n");
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffKind::Elided
                    && l.content.contains("trailing newline changed")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(DiffLine::is_change));
    }

    #[test]
    fn crlf_normalisation_is_named_not_silent() {
        let lines = display_diff("a\r\nb\r\n", "a\nb\n");
        assert!(
            lines.iter().any(|l| l.kind == DiffKind::Elided),
            "a CRLF→LF rewrite must not render as an empty diff: {lines:?}"
        );
    }

    #[test]
    fn identical_content_yields_no_lines() {
        assert!(display_diff("a\nb\n", "a\nb\n").is_empty());
    }

    /// An empty real diff is a value, not a sentinel — the whole point of the
    /// enum. An empty original is a new file, and a non-UTF8 original says so.
    #[test]
    fn state_distinguishes_empty_diff_from_missing_capture() {
        assert_eq!(
            WriteDiffState::from_original(b"", ""),
            WriteDiffState::NewFile
        );
        assert_eq!(
            WriteDiffState::from_original(b"a\n", "a\n"),
            WriteDiffState::Diff {
                lines: vec![],
                more: 0
            }
        );
        assert_eq!(
            WriteDiffState::from_original(&[0xff, 0xfe], "x"),
            WriteDiffState::NotCaptured {
                reason: NotCapturedReason::OriginalNotUtf8
            }
        );
    }

    /// A record with no state field predates the feature; it must not claim
    /// the file was new.
    #[test]
    fn default_state_is_unknown_provenance_not_new_file() {
        assert_eq!(
            WriteDiffState::default(),
            WriteDiffState::NotCaptured {
                reason: NotCapturedReason::HistoricalOrReattached
            }
        );
        let restored: WriteDiffState = serde_json::from_str("null").unwrap_or_default();
        assert!(matches!(restored, WriteDiffState::NotCaptured { .. }));
    }

    #[test]
    fn every_reason_has_its_own_parenthetical() {
        let all = [
            NotCapturedReason::NoActiveCheckpoint,
            NotCapturedReason::SnapshotUnavailable,
            NotCapturedReason::SnapshotReadFailed,
            NotCapturedReason::OriginalNotUtf8,
            NotCapturedReason::SupersededInBatch,
            NotCapturedReason::HistoricalOrReattached,
        ];
        let mut seen = std::collections::HashSet::new();
        for r in all {
            assert!(seen.insert(r.describe()), "duplicate text for {r:?}");
        }
        // AC4 / ruling A4 pin this exact parenthetical for the no-checkpoint case.
        assert_eq!(
            NotCapturedReason::NoActiveCheckpoint.describe(),
            "no active checkpoint"
        );
    }

    #[test]
    fn cap_keeps_both_ends_and_reports_the_dropped_count() {
        let lines: Vec<DiffLine> = (0..500)
            .map(|i| DiffLine::of(DiffKind::Added, format!("l{i}")))
            .collect();
        let (shown, more) = cap_lines(lines, DIFF_MAX_LINES);
        assert_eq!(more, 300);
        // 200 content lines plus the inline marker between the two halves.
        assert_eq!(shown.len(), DIFF_MAX_LINES + 1);
        assert_eq!(shown[0].content, "l0", "head is kept");
        assert_eq!(shown[shown.len() - 1].content, "l499", "tail is kept too");
        assert!(
            shown
                .iter()
                .any(|l| l.kind == DiffKind::Elided && l.content == "… 300 more lines"),
            "{shown:?}"
        );
    }

    // Independent small-input oracle: legacy trimmed LCS, bitmap elision and
    // slice-based cap. Do not call the production visitors from this oracle.
    fn reference_diff(old: &str, new: &str) -> Vec<DiffLine> {
        let (old, new): (Vec<_>, Vec<_>) = (old.lines().collect(), new.lines().collect());
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old
            .iter()
            .rev()
            .take(old.len() - prefix)
            .zip(new.iter().rev().take(new.len() - prefix))
            .take_while(|(a, b)| a == b)
            .count();
        let (a, b) = (
            &old[prefix..old.len() - suffix],
            &new[prefix..new.len() - suffix],
        );
        let (mut i, mut j) = (a.len(), b.len());
        let mut dp = vec![vec![0; j + 1]; i + 1];
        for x in 1..=i {
            for y in 1..=j {
                dp[x][y] = if a[x - 1] == b[y - 1] {
                    dp[x - 1][y - 1] + 1
                } else {
                    dp[x - 1][y].max(dp[x][y - 1])
                };
            }
        }
        let mut middle = Vec::new();
        while i != 0 || j != 0 {
            if i != 0 && j != 0 && a[i - 1] == b[j - 1] {
                middle.push(DiffLine::of(DiffKind::Context, a[i - 1]));
                i -= 1;
                j -= 1;
            } else if j != 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
                middle.push(DiffLine::of(DiffKind::Added, b[j - 1]));
                j -= 1;
            } else {
                middle.push(DiffLine::of(DiffKind::Removed, a[i - 1]));
                i -= 1;
            }
        }
        old[..prefix]
            .iter()
            .map(|s| DiffLine::of(DiffKind::Context, *s))
            .chain(middle.into_iter().rev())
            .chain(
                old[old.len() - suffix..]
                    .iter()
                    .map(|s| DiffLine::of(DiffKind::Context, *s)),
            )
            .collect()
    }

    fn reference_elision(lines: Vec<DiffLine>, context: usize) -> Vec<DiffLine> {
        if !lines.iter().any(DiffLine::is_change) {
            return lines
                .into_iter()
                .filter(|l| l.kind == DiffKind::Elided)
                .collect();
        }
        let mut keep = vec![false; lines.len()];
        for (i, line) in lines.iter().enumerate() {
            if line.is_change() || line.kind == DiffKind::Elided {
                keep[i.saturating_sub(context)..=i.saturating_add(context).min(lines.len() - 1)]
                    .fill(true);
            }
        }
        let (mut out, mut dropped) = (Vec::new(), 0);
        for (line, keep) in lines
            .into_iter()
            .zip(keep)
            .chain(std::iter::once((DiffLine::of(DiffKind::Context, ""), true)))
        {
            if keep {
                if dropped != 0 {
                    out.push(DiffLine::of(
                        DiffKind::Elided,
                        format!("⋯ {dropped} unchanged lines"),
                    ));
                    dropped = 0;
                }
                out.push(line);
            } else {
                dropped += 1;
            }
        }
        out.pop(); // The sentinel only flushes a trailing unchanged run.
        out
    }

    #[test]
    fn borrowed_display_matches_independent_legacy_shapes() {
        let mut inputs = vec![String::new()];
        for len in 1..=3 {
            inputs.extend((0..1 << len).map(|bits| {
                (0..len)
                    .map(|bit| if bits & (1 << bit) == 0 { "a\n" } else { "b\n" })
                    .collect()
            }));
        }
        inputs.extend(["a", "a\r\nb\r\n", "a\nb\n"].map(String::from));
        inputs.push((0..220).map(|i| format!("{i}\n")).collect());
        inputs.push(inputs.last().unwrap().replace("110\n", "CHANGED\n"));
        for old in &inputs {
            for new in &inputs {
                let raw = reference_diff(old, new);
                assert_eq!(compute_diff(old, new), raw);
                let mut annotated = raw.clone();
                annotated.insert(raw.len() / 2, DiffLine::of(DiffKind::Elided, "note"));
                for context in [0, 1, 3] {
                    assert_eq!(
                        elide_unchanged(raw.clone(), context),
                        reference_elision(raw.clone(), context)
                    );
                    assert_eq!(
                        elide_unchanged(annotated.clone(), context),
                        reference_elision(annotated.clone(), context)
                    );
                }
                let mut expected = reference_elision(raw.clone(), 3);
                if old != new && !raw.iter().any(DiffLine::is_change) {
                    expected.push(DiffLine::of(
                        DiffKind::Elided,
                        "⚠ line endings or trailing newline changed — no line content changed",
                    ));
                }
                assert_eq!(display_diff(old, new), expected);
                for max in [0, 1, 200, usize::MAX] {
                    let more = expected.len().saturating_sub(max);
                    let mut capped = expected.clone();
                    if more != 0 {
                        capped = expected[..max / 2].to_vec();
                        capped.push(DiffLine::of(
                            DiffKind::Elided,
                            format!("… {more} more lines"),
                        ));
                        capped.extend_from_slice(&expected[expected.len() - (max - max / 2)..]);
                    }
                    assert_eq!(cap_lines(expected.clone(), max), (capped.clone(), more));
                    let (rows, dropped) = capped_display_diff(old, new, max);
                    assert_eq!(
                        (
                            rows.into_iter()
                                .map(DiffLineRef::into_owned)
                                .collect::<Vec<_>>(),
                            dropped
                        ),
                        (capped, more)
                    );
                }
            }
        }
    }

    #[test]
    fn retained_source_rows_borrow_even_huge_lines() {
        let old = format!("{}\nold\nend", "x".repeat(1 << 20));
        let new = old.replace("\nold\n", "\nnew\n");
        let (rows, more) = capped_display_diff(&old, &new, 200);
        assert_eq!((rows.len(), more), (4, 0));
        for row in rows {
            let Cow::Borrowed(text) = row.content else {
                panic!("owned source row");
            };
            let source = if row.kind == DiffKind::Added {
                &new
            } else {
                &old
            };
            let expected = source.lines().find(|line| *line == text).unwrap();
            assert_eq!(text.as_ptr(), expected.as_ptr());
        }
        let unchanged = "same\n".repeat(100_000);
        assert_eq!(capped_display_diff(&unchanged, &unchanged, 0), (vec![], 0));
        let (rows, more) = capped_display_diff(&unchanged, unchanged.trim_end_matches('\n'), 1);
        assert_eq!((rows.len(), more), (1, 0));
        assert_eq!(rows[0].kind, DiffKind::Elided);
    }

    #[test]
    fn oversized_middle_streams_borrowed_head_and_tail() {
        let (old, new) = ("old\n".repeat(2_000), "new\n".repeat(2_000));
        let (rows, more) = capped_display_diff(&old, &new, 200);
        assert_eq!((rows.len(), more), (201, 3_800));
        assert_eq!(rows[100].content, "… 3800 more lines");
        assert!(rows[..100].iter().all(|row| row.kind == DiffKind::Removed));
        assert!(rows[101..].iter().all(|row| row.kind == DiffKind::Added));
        assert!(
            rows.iter()
                .filter(|row| row.kind != DiffKind::Elided)
                .all(|row| matches!(&row.content, Cow::Borrowed(_)))
        );
    }
}

#[cfg(test)]
mod story21_9_tests {
    use super::*;

    #[test]
    fn story21_9_diff_stat_matches_the_rows_of_the_line_diff() {
        for (old, new) in [
            ("", "a\nb"),
            ("a\nb", ""),
            ("a\nb\nc", "a\nX\nc"),
            ("a\nb\nc\nd\ne\nf", "a\nB\nc\nd\ne\nF\nG"),
            ("same\n", "same\n"),
            ("a\r\nb\r\n", "a\r\nc\r\n"),
        ] {
            let rows = compute_diff(old, new);
            let added = rows.iter().filter(|r| r.kind == DiffKind::Added).count();
            let removed = rows.iter().filter(|r| r.kind == DiffKind::Removed).count();
            assert_eq!(
                diff_stat(old, new),
                Some(DiffStat { added, removed }),
                "{old:?} -> {new:?}"
            );
        }
    }

    #[test]
    fn story21_9_diff_stat_counts_context_as_neither() {
        let stat = diff_stat("keep\nold\nkeep2", "keep\nnew\nkeep2").unwrap();
        assert_eq!((stat.added, stat.removed), (1, 1));
    }

    #[test]
    fn story21_9_diff_stat_is_none_past_the_per_frame_budget_and_never_over_counts() {
        let old: String = (0..600).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..600).map(|i| format!("new {i}\n")).collect();
        // 600 x 600 cells exceed the stat budget: no number rather than a degraded one.
        assert_eq!(diff_stat(&old, &new), None);
        // A pure add or delete never needs the table, however large.
        let big: String = (0..5000).map(|i| format!("l {i}\n")).collect();
        assert_eq!(
            diff_stat("", &big),
            Some(DiffStat {
                added: 5000,
                removed: 0
            })
        );
        // A small change inside a large file is trimmed to its middle first.
        let edited = big.replace("l 2500\n", "changed\n");
        assert_eq!(
            diff_stat(&big, &edited),
            Some(DiffStat {
                added: 1,
                removed: 1
            })
        );
    }
}
