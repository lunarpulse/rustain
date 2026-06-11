//! 3-mode anchor FSM per ADR-16-01 §3 + UX-DR-ANCHOR-FSM.
//!
//! `Following` snaps to bottom (default; entered on submit / `G`);
//! `Reading` freezes the viewport (entered on scroll-up; new content appends
//! off-screen with "↓ N new lines" hint owned by status bar render);
//! `Pinned(AnchorRef)` locks `(turn_id, line_in_turn)` to a fixed screen row
//! across reflow / fold-toggle / new content (entered on `]]`/`[[`).
//! Reconciled once per frame in `ViewState::reconcile()` consuming a single
//! `ViewEvent` — see AC5.
//!
//! # ViewEvent variants
//!
//! - `Scroll(ScrollDelta)` — user scrolls (keyboard, mouse wheel)
//! - `Submit` — user sends a prompt (mode → Following)
//! - `FoldToggle { turn_id, prev_focused_turn_top, prev_max_offset }` — fold
//!   toggled; caller MUST snapshot `prev_focused_turn_top` and `prev_max_offset`
//!   BEFORE mutating layout heights. AC7 anchor-preservation formula consumes these.
//! - `JumpTurn { turn_id }` — `]]`/`[[` jump to a turn (mode → Pinned)
//! - `StreamAppend { appended_lines }` — new stream content arrived; drives
//!   `pending_append_lines` accumulation in Reading/Pinned (AC9).
//!
//! # AnchorMode variants
//!
//! - `Following` — viewport snaps to bottom on new content; default at app start.
//! - `Reading` — viewport frozen at current scroll position.
//! - `Pinned(AnchorRef)` — locked to a specific (turn_id, line_in_turn).
//!   Re-resolved against `LayoutMetrics.turn_top_offsets` every frame so
//!   reflow / fold-toggle / new-content do NOT invalidate the pin.
//!
//! # scroll_offset is offset-from-bottom
//!
//! `scroll_offset == 0` means "scrolled to bottom"; `scroll_offset == max_offset`
//! means "scrolled to top". This matches the existing `tab.scroll_offset`
//! convention at `chat_pane/mod.rs:454-470`, so S16.6/8 keymap migration is
//! mechanical (rename, no inversion).
//!
//! # No Clock dependency
//!
//! `ViewState::reconcile` is purely event-driven; it does not query elapsed time.
//! `Pinned` survival across reflow is achieved by re-resolving
//! `(turn_id, line_in_turn)` against `LayoutMetrics.turn_top_offsets` every
//! frame, NOT by sampling a clock. This is a purity invariant per ADR-16-01 §3:
//! no `Instant::now()`, no `Clock` dependency, no `tokio::spawn`.
//!
//! # O(N) lookup note
//!
//! `resolve_pinned` performs `find()` over `turn_top_offsets` (a `Vec`, O(N)).
//! For pathological 10k-turn sessions this is still microseconds. If S16.5's
//! height-cache moves to a `HashMap<TurnId, usize>` keying scheme, the
//! `LayoutMetrics` struct should switch too. Deferred to S16.5.
//!
//! # Fold-toggle anchor-preservation worked example
//!
//! 5 turns, fold turn #2 (height 8 → 1, above focused turn #3):
//! viewport=10, T_old=13 (turn #3 top lines-from-top), S_old=15 (offset-from-bottom),
//! prev_max_offset=23 (total 33 - viewport 10), T_new=6, max_after=16.
//!
//! ```text
//! top_visible_before  = 23 - 15  = 8
//! focused_screen_row  = 13 - 8   = 5
//! new_top_visible     = 6 - 5    = 1
//! new_scroll_offset   = 16 - 1   = 15
//! ```
//!
//! Focused turn top stays at screen row 5 before and after the toggle.
//!
//! # Collapse policy notes
//!
//! `is_collapsed(turn)` auto-expands running turns (`stop_reason.is_none()`)
//! and turns containing any errored invocation (`InvocationStatus::Error`).
//! `Cancelled` invocations do **not** trigger failure auto-expand — cancellation
//! is a user action, not a system fault (UX-DR-FAILURE-AUTOEXPAND).
//!
//! # Cross-references
//!
//! - ADR-16-01 §3 (View state and anchor FSM)
//! - UX-DR-ANCHOR-FSM (`ux-design-specification.md:2273-2295`)
//! - S16.4 (render consumer — reads `collapsed`, `summary_tier`, `scroll_offset`)
//! - S16.5 (height cache consumer — keys by `expansion_state` from `is_collapsed`)
//! - S16.6 (vim keymap producer — emits `ViewEvent::*` from key handlers)
//! - S16.8 (fast-scroll keymap producer — emits `ViewEvent::Scroll`/`Submit`/wheel)
//!
//! # Tracing capture deferral
//!
//! The two `tracing::warn!` sites (Pinned-degrade in `resolve_pinned`, and any
//! warn in fold-toggle fallback) are smoke-only in this story: tests assert
//! behavior (mode demotion, scroll_offset clamp) but NOT the warn emission.
//! Introducing a `MakeWriter` capture layer is out of scope; flagged for a future
//! test-infra story.

use std::collections::HashMap;

use crate::domain::models::turn::{InvocationStatus, Turn, TurnId, TurnPart};

/// 3-mode anchor FSM.
///
/// | Current | Event | New mode | scroll_offset |
/// |---|---|---|---|
/// | any | `Submit` | `Following` | `0` |
/// | `Following` | `Scroll(Up/...)` | `Reading` | increase clamped |
/// | `Following` | `Scroll(Down/...)` | `Following` (unchanged) | `0` |
/// | `Reading` | `Scroll(Up/...)` | `Reading` | increase clamped |
/// | `Reading` | `Scroll(Down/...)` landing at `0` | `Following` | `0` |
/// | `Reading` | `Scroll(Down/...)` not at `0` | `Reading` | decrease clamped |
/// | `Reading` | `Scroll(Top)` | `Reading` | `max_offset` |
/// | `Reading` | `Scroll(Bottom)` | `Following` | `0` |
/// | `Pinned(_)` | `Scroll(*)` (any direction) | `Reading` (degrade) | per delta clamped |
/// | any | `JumpTurn { turn_id }` | `Pinned(AnchorRef { turn_id, line_in_turn: 0 })` | resolved so turn-top is near viewport-top |
/// | `Following` | `FoldToggle { .. }` | `Reading` (amended: anchor-preserved, not snap-to-bottom) | per AC7 anchor formula |
/// | `Reading` or `Pinned(_)` | `FoldToggle { prev_focused_turn_top: Some(_), .. }` | unchanged | per AC7 anchor formula |
/// | `Reading` or `Pinned(_)` | `FoldToggle { prev_focused_turn_top: None, .. }` | unchanged | viewport-top fallback clamped |
/// | `Following` | `StreamAppend { .. }` | unchanged | `0` |
/// | `Reading` | `StreamAppend { .. }` | unchanged | unchanged |
/// | `Pinned(_)` | `StreamAppend { .. }` | unchanged (or `Reading` if turn missing) | re-resolved |
#[derive(Clone, Debug, PartialEq)]
pub enum AnchorMode {
    Following,
    Reading,
    Pinned(AnchorRef),
}

/// Re-resolvable anchor to a specific (turn, line) position.
///
/// `line_in_turn` defaults to `0` for `]]`/`[[` jumps; future stories may
/// pin to mid-turn lines (e.g., first `Prose` part's first line — Q2 deferral).
#[derive(Clone, Debug, PartialEq)]
pub struct AnchorRef {
    pub turn_id: TurnId,
    /// 0-indexed line offset within the turn's rendered region.
    /// Re-resolved against `LayoutMetrics.turn_top_offsets` every frame
    /// so reflow / fold-toggle / new-content do NOT invalidate the pin.
    pub line_in_turn: usize,
}

/// Tier selector for collapsed-turn summary rendering.
///
/// `Tier1` (cheap-form, default): model badge + top tool name.
/// `Tier2` (rich): model + tool-count + top-k tool names + prose-preview line.
/// Toggled by `zs` (S16.6) — `toggle_summary_tier` flips `Tier1 ↔ Tier2`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SummaryTier {
    #[default]
    Tier1,
    Tier2,
}

/// Scroll delta emitted by keyboard or mouse input.
///
/// `WheelUp(u16)` / `WheelDown(u16)` carry per-tick line counts.
/// Default per UX-DR-MOUSE: 3 lines per tick, configurable (S16.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDelta {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    FullPageUp,
    FullPageDown,
    WheelUp(u16),
    WheelDown(u16),
    Top,
    Bottom,
}

/// A single view event — exactly one per `reconcile()` call to defuse
/// the anchor-on-toggle vs anchor-on-scroll race (ADR-16-01 §3).
#[derive(Clone, Debug, PartialEq)]
pub enum ViewEvent {
    Scroll(ScrollDelta),
    Submit,
    /// Caller MUST snapshot `prev_focused_turn_top` and `prev_max_offset`
    /// BEFORE mutating layout heights via `toggle_fold` / `collapse_all` /
    /// `expand_all`. AC7 anchor-preservation formula consumes these.
    FoldToggle {
        turn_id: TurnId,
        prev_focused_turn_top: Option<usize>,
        prev_max_offset: usize,
    },
    JumpTurn {
        turn_id: TurnId,
    },
    /// `appended_lines` is the line count added by this stream tick;
    /// drives `pending_append_lines` accumulation in Reading/Pinned (AC9).
    StreamAppend {
        appended_lines: usize,
    },
    /// S16.8 AC15: Explicit anchor drop + scroll.  The two-stage confirmation
    /// gate in `handle_input` emits this on the second scroll-intent within
    /// 2000ms while Pinned.  Flips mode to Reading then applies the delta.
    DropAnchorAndScroll {
        delta: ScrollDelta,
    },
}

/// Minimal layout POD — no `ratatui::Rect` (hexagonal boundary).
///
/// The caller (S16.6/8 event loop) is responsible for computing these values
/// and passing them to `reconcile()`.
#[derive(Clone, Debug)]
pub struct LayoutMetrics {
    /// Visible viewport in lines.
    pub viewport_height: usize,
    /// Total rendered content height in lines.
    pub total_content_height: usize,
    /// Ordered list of (turn_id, line-offset-from-top) for every turn currently
    /// in the layout. Second element is monotonically non-decreasing.
    ///
    /// **Invariant:** TurnIds are unique. First-match-wins on lookup; duplicates
    /// are caller error. Future features (turn-edit, branch-rewind, undo-redo)
    /// must preserve uniqueness.
    pub turn_top_offsets: Vec<(TurnId, usize)>,
    /// `turn_top_offsets`'s second element for the focused turn, or `None`.
    pub focused_turn_top: Option<usize>,
}

/// Per-tab view state — the single source of truth for scroll/anchor/collapse.
///
/// Lives alongside `tab.scroll_offset` / `tab.auto_scroll` (the legacy mirror
/// that survives this story) per the path-B parallel-fields cutover.
#[derive(Clone, Debug)]
pub struct ViewState {
    pub mode: AnchorMode,
    pub collapsed: HashMap<TurnId, bool>,
    pub summary_tier: SummaryTier,
    /// Offset-from-bottom in lines. `0` = bottom; `max_offset` = top.
    pub scroll_offset: usize,
    pub focused_turn: Option<TurnId>,
    /// "↓ N new lines" hint counter for status bar (UX-DR-ANCHOR-FSM).
    /// Increment in StreamAppend's Reading/Pinned arms; reset to 0 on
    /// any transition into Following.
    pub pending_append_lines: usize,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            mode: AnchorMode::Following,
            collapsed: HashMap::new(),
            summary_tier: SummaryTier::Tier1,
            scroll_offset: 0,
            focused_turn: None,
            pending_append_lines: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Collapse policy
// ---------------------------------------------------------------------------

impl ViewState {
    /// Determine whether a turn is collapsed.
    ///
    /// Auto-expand rules fire BEFORE consulting the user-toggle map:
    /// 1. Running turn (`stop_reason.is_none()`) → expanded (never hide work in progress)
    /// 2. Turn contains a failed invocation → expanded (UX-DR-FAILURE-AUTOEXPAND)
    /// 3. Otherwise → respects the user-toggle map.
    ///
    /// Default-expanded on completion (flipped 2026-04-29 party-mode).
    /// `Cancelled` invocations do NOT trigger failure auto-expand — cancellation
    /// is a user action, not a system fault (see module doc).
    pub fn is_collapsed(&self, turn: &Turn) -> bool {
        // Running override: never collapse a turn that's still streaming.
        if turn.stop_reason.is_none() {
            return false;
        }

        // Failure override: auto-expand turns with any errored invocation.
        // Cancelled is NOT an error — Cancelled is user-action, no override.
        let has_error = turn.parts.iter().any(|part| {
            matches!(
                part,
                TurnPart::ToolInvocation {
                    status: InvocationStatus::Error,
                    ..
                }
            )
        });
        if has_error {
            return false;
        }

        // User-toggle map. Default-expanded on completion (flipped 2026-04-29).
        *self.collapsed.get(&turn.id).unwrap_or(&false)
    }

    /// Toggle the collapsed state for a turn.
    ///
    /// The *first* press on a never-toggled turn (which `is_collapsed` reports
    /// as expanded under the new default) inserts `true`, collapsing the turn.
    /// The second press is the inverse.
    ///
    /// Note: `toggle_fold` does NOT consider `stop_reason` / failure status —
    /// it only mutates the map. Auto-expand rules apply at *read* time via
    /// `is_collapsed`, not at write time. User toggles persist even while
    /// a turn is briefly auto-expanded due to running/error, and snap back
    /// to user intent once the override condition clears.
    ///
    /// S16.6 NOTE: pair every `toggle_fold` with `tab_render_state.height_cache.invalidate_turn(turn_id)`
    /// via `chat_pane::toggle_turn_fold` — see S16.5 AC3 for cache coherence requirements.
    pub fn toggle_fold(&mut self, turn_id: &TurnId) {
        self.collapsed
            .entry(turn_id.clone())
            .and_modify(|v| *v = !*v)
            .or_insert(true);
    }

    /// Collapse all turns (backing `zM` chord — S16.6).
    pub fn collapse_all(&mut self, turns: &[Turn]) {
        for turn in turns {
            self.collapsed.insert(turn.id.clone(), true);
        }
    }

    /// Expand all turns (backing `zR` chord — S16.6).
    pub fn expand_all(&mut self, turns: &[Turn]) {
        for turn in turns {
            self.collapsed.insert(turn.id.clone(), false);
        }
    }
}

// ---------------------------------------------------------------------------
// Summary tier toggle
// ---------------------------------------------------------------------------

impl ViewState {
    /// Flip `Tier1 ↔ Tier2`. Does NOT mutate `mode` (tier is mode-orthogonal
    /// per UX-DR-ANCHOR-FSM transition table). Called by `zs` (S16.6).
    ///
    /// S16.6 NOTE: pair every `toggle_summary_tier` with `tab_render_state.height_cache.invalidate_all()`
    /// via `chat_pane::set_summary_tier` — see S16.5 AC4 for cache coherence requirements.
    pub fn toggle_summary_tier(&mut self) {
        self.summary_tier = match self.summary_tier {
            SummaryTier::Tier1 => SummaryTier::Tier2,
            SummaryTier::Tier2 => SummaryTier::Tier1,
        };
    }
}

// ---------------------------------------------------------------------------
// Focus setter
// ---------------------------------------------------------------------------

impl ViewState {
    /// Set the currently focused turn (for fold-toggle anchor preservation).
    /// Called by S16.4/S16.6 when the user navigates with `J`/`K` or `]]`/`[[`.
    pub fn set_focused_turn(&mut self, turn_id: Option<TurnId>) {
        self.focused_turn = turn_id;
    }
}

// ---------------------------------------------------------------------------
// reconcile — mode FSM
// ---------------------------------------------------------------------------

impl ViewState {
    /// Reconcile a single ViewEvent. Returns the resolved scroll_offset.
    ///
    /// One event per frame — never two — to defuse the anchor-race class
    /// (ADR-16-01 §3). When `event = None`, only refreshes the resolved
    /// `scroll_offset` from the current `mode` against the new `layout`.
    ///
    /// # Panics
    ///
    /// This function does NOT panic. Saturating arithmetic throughout.
    pub fn reconcile(&mut self, event: Option<ViewEvent>, layout: &LayoutMetrics) -> usize {
        // --- Apply the event (if any) ---
        let is_fold_toggle = matches!(event, Some(ViewEvent::FoldToggle { .. }));
        if let Some(e) = event {
            self.apply_event(e, layout);
        }

        // --- Resolve scroll_offset for current mode against current layout ---
        let max_offset = max_offset(layout);
        let new_offset = match &self.mode {
            AnchorMode::Following => 0,
            AnchorMode::Reading => self.scroll_offset.min(max_offset),
            AnchorMode::Pinned(anchor) => {
                if is_fold_toggle {
                    // FoldToggle already computed the correct offset via AC7
                    // anchor-preservation formula. Just clamp; do NOT re-resolve
                    // the pinned anchor against the new layout (that would
                    // overwrite the AC7 formula result).
                    self.scroll_offset.min(max_offset)
                } else {
                    match resolve_pinned_impl(&layout.turn_top_offsets, anchor, max_offset) {
                        Some(off) => off,
                        None => {
                            self.mode = AnchorMode::Reading;
                            tracing::warn!(
                                "Pinned turn_id no longer present; degrading to Reading"
                            );
                            self.scroll_offset.min(max_offset)
                        }
                    }
                }
            }
        };

        self.scroll_offset = new_offset;
        self.scroll_offset
    }

    fn apply_event(&mut self, event: ViewEvent, layout: &LayoutMetrics) {
        match event {
            ViewEvent::Submit => {
                self.mode = AnchorMode::Following;
                self.scroll_offset = 0;
                self.pending_append_lines = 0;
            }
            ViewEvent::Scroll(delta) => self.apply_scroll(delta, layout),
            ViewEvent::JumpTurn { turn_id } => self.apply_jump_turn(turn_id, layout),
            ViewEvent::FoldToggle {
                turn_id: _,
                prev_focused_turn_top,
                prev_max_offset,
            } => self.apply_fold_toggle(prev_focused_turn_top, prev_max_offset, layout),
            ViewEvent::StreamAppend { appended_lines } => {
                self.apply_stream_append(appended_lines, layout);
            }
            ViewEvent::DropAnchorAndScroll { delta } => {
                // S16.8 AC15: Explicit drop — flip to Reading, then apply scroll.
                self.mode = AnchorMode::Reading;
                self.apply_scroll(delta, layout);
            }
        }
    }

    fn apply_scroll(&mut self, delta: ScrollDelta, layout: &LayoutMetrics) {
        let max_off = max_offset(layout);
        let viewport = layout.viewport_height;
        let max_off_signed = max_off.min(isize::MAX as usize) as isize;

        // Determine line increment (positive = scroll up, negative = scroll down).
        let increment: isize = match delta {
            ScrollDelta::LineUp => 1,
            ScrollDelta::HalfPageUp => (viewport / 2).max(1).min(isize::MAX as usize) as isize,
            ScrollDelta::FullPageUp => viewport.max(1).min(isize::MAX as usize) as isize,
            ScrollDelta::WheelUp(n) => n.max(1) as isize,
            ScrollDelta::Top => {
                // Jump to top — Reading at max_offset.
                self.mode = AnchorMode::Reading;
                self.scroll_offset = max_off;
                return;
            }
            ScrollDelta::LineDown => -1,
            ScrollDelta::HalfPageDown => -((viewport / 2).max(1).min(isize::MAX as usize) as isize),
            ScrollDelta::FullPageDown => -(viewport.max(1).min(isize::MAX as usize) as isize),
            ScrollDelta::WheelDown(n) => -(n.max(1) as isize),
            ScrollDelta::Bottom => {
                // Jump to bottom — Following at 0.
                self.mode = AnchorMode::Following;
                self.scroll_offset = 0;
                self.pending_append_lines = 0;
                return;
            }
        };

        let scroll_signed = self.scroll_offset.min(isize::MAX as usize) as isize;
        let new_offset = (scroll_signed + increment).clamp(0, max_off_signed) as usize;

        if increment > 0 {
            // Scrolling up.
            self.mode = AnchorMode::Reading;
            self.scroll_offset = new_offset.min(max_off);
        } else {
            // Scrolling down.
            if new_offset == 0 {
                self.mode = AnchorMode::Following;
                self.scroll_offset = 0;
                self.pending_append_lines = 0;
            } else if matches!(self.mode, AnchorMode::Pinned(_)) {
                // Hand-scrolling out of Pinned degrades to Reading.
                self.mode = AnchorMode::Reading;
                self.scroll_offset = new_offset.min(max_off);
            } else {
                // Already Reading or Following (but not at 0 for Reading).
                self.scroll_offset = new_offset.min(max_off);
            }
        }
    }

    fn apply_jump_turn(&mut self, turn_id: TurnId, layout: &LayoutMetrics) {
        let max_off = max_offset(layout);
        let anchor = AnchorRef {
            turn_id: turn_id.clone(),
            line_in_turn: 0,
        };

        match resolve_pinned_impl(&layout.turn_top_offsets, &anchor, max_off) {
            Some(offset) => {
                self.mode = AnchorMode::Pinned(anchor);
                self.scroll_offset = offset;
                self.focused_turn = Some(turn_id);
            }
            None => {
                // Pinned turn not found in layout — defensive degradation.
                self.mode = AnchorMode::Reading;
                tracing::warn!("Pinned turn_id no longer present; degrading to Reading");
                // Leave scroll_offset clamped (no further action).
            }
        }
    }

    fn apply_fold_toggle(
        &mut self,
        prev_focused_turn_top: Option<usize>,
        prev_max_offset: usize,
        layout: &LayoutMetrics,
    ) {
        // Anchor-preserved path for ALL modes (amended: Following also preserves anchor,
        // transitioning to Reading so the focused turn stays visible on screen).
        if let Some(t_old) = prev_focused_turn_top {
            if let Some(t_new) = layout.focused_turn_top {
                self.scroll_offset = fold_toggle_anchor_preserved(
                    t_old,
                    prev_max_offset,
                    self.scroll_offset,
                    t_new,
                    layout,
                );
                if matches!(self.mode, AnchorMode::Following) {
                    self.mode = AnchorMode::Reading;
                }
            } else {
                self.scroll_offset = self.scroll_offset.min(max_offset(layout));
            }
        } else {
            self.scroll_offset = self.scroll_offset.min(max_offset(layout));
        }
    }

    fn apply_stream_append(&mut self, appended_lines: usize, layout: &LayoutMetrics) {
        match &self.mode {
            AnchorMode::Following => {
                self.scroll_offset = 0;
                // pending_append_lines stays 0 in Following.
            }
            AnchorMode::Reading => {
                self.pending_append_lines =
                    self.pending_append_lines.saturating_add(appended_lines);
                // scroll_offset unchanged; clamp to new max.
                self.scroll_offset = self.scroll_offset.min(max_offset(layout));
            }
            AnchorMode::Pinned(anchor) => {
                self.pending_append_lines =
                    self.pending_append_lines.saturating_add(appended_lines);
                let max_off = max_offset(layout);
                match resolve_pinned_impl(&layout.turn_top_offsets, anchor, max_off) {
                    Some(off) => {
                        self.scroll_offset = off;
                    }
                    None => {
                        self.mode = AnchorMode::Reading;
                        tracing::warn!("Pinned turn_id no longer present; degrading to Reading");
                        self.scroll_offset = self.scroll_offset.min(max_off);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Maximum offset-from-bottom (0 = bottom, this = top).
fn max_offset(layout: &LayoutMetrics) -> usize {
    layout
        .total_content_height
        .saturating_sub(layout.viewport_height)
}

/// Resolve a pinned anchor to an offset-from-bottom.
///
/// Returns `None` if the pinned `turn_id` is not in `turn_top_offsets`.
fn resolve_pinned_impl(
    turn_top_offsets: &[(TurnId, usize)],
    anchor: &AnchorRef,
    max_offset: usize,
) -> Option<usize> {
    let t_lines_from_top = turn_top_offsets
        .iter()
        .find(|(id, _)| id == &anchor.turn_id)
        .map(|(_, off)| *off)?;

    let desired_top_line = t_lines_from_top.saturating_add(anchor.line_in_turn);
    let new_offset = max_offset.saturating_sub(desired_top_line);
    Some(new_offset.clamp(0, max_offset))
}

/// AC7 anchor-preservation formula (offset-from-bottom space).
///
/// Inputs:
/// - `t_old`: focused turn's top line (from top) BEFORE the fold toggle
/// - `prev_max_offset`: max_offset BEFORE the fold toggle
/// - `s_old`: current scroll_offset BEFORE the fold toggle
/// - `t_new`: focused turn's top line (from top) AFTER the fold toggle
/// - `layout_after`: the layout AFTER the fold toggle
///
/// Returns the new `scroll_offset` that keeps the focused turn's top edge
/// at the same screen row as before.
fn fold_toggle_anchor_preserved(
    t_old: usize,
    prev_max_offset: usize,
    s_old: usize,
    t_new: usize,
    layout_after: &LayoutMetrics,
) -> usize {
    // Top-of-viewport line index BEFORE the toggle.
    let top_visible_before = prev_max_offset.saturating_sub(s_old);

    // Focused turn's top edge, in screen rows from viewport top.
    let focused_screen_row = t_old.saturating_sub(top_visible_before);

    // Desired top-of-viewport line AFTER the toggle.
    let new_top_visible = t_new.saturating_sub(focused_screen_row);

    let new_max_offset = max_offset(layout_after);

    new_max_offset
        .saturating_sub(new_top_visible)
        .clamp(0, new_max_offset)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test builders
    // -----------------------------------------------------------------------

    /// Build a minimal `Turn` for collapse-policy tests.
    fn make_turn(
        id: &str,
        stop_reason: Option<crate::domain::models::stream::StopReason>,
        parts: Vec<TurnPart>,
    ) -> Turn {
        let mut turn = Turn::new("test-model".into(), 0);
        turn.id = TurnId(id.to_string());
        turn.stop_reason = stop_reason;
        // Clear default parts and insert ours.
        turn.parts = parts;
        turn
    }

    fn test_part_id() -> crate::domain::models::turn::PartId {
        crate::domain::models::turn::PartId(0)
    }

    /// Build a running turn (no stop_reason).
    fn running_turn(id: &str) -> Turn {
        make_turn(id, None, vec![])
    }

    /// Build a completed clean turn.
    fn completed_clean_turn(id: &str) -> Turn {
        make_turn(
            id,
            Some(crate::domain::models::stream::StopReason::EndTurn),
            vec![],
        )
    }

    /// Build a completed turn with a failed invocation.
    fn error_turn(id: &str) -> Turn {
        make_turn(
            id,
            Some(crate::domain::models::stream::StopReason::EndTurn),
            vec![TurnPart::ToolInvocation {
                id: test_part_id(),
                tool: "fail".into(),
                args: serde_json::Value::Null,
                status: InvocationStatus::Error,
                started_at: 0,
                ended_at: Some(0),
            }],
        )
    }

    /// Build a completed turn with a cancelled invocation.
    fn cancelled_turn(id: &str) -> Turn {
        make_turn(
            id,
            Some(crate::domain::models::stream::StopReason::Cancelled),
            vec![TurnPart::ToolInvocation {
                id: test_part_id(),
                tool: "cancel".into(),
                args: serde_json::Value::Null,
                status: InvocationStatus::Cancelled,
                started_at: 0,
                ended_at: Some(0),
            }],
        )
    }

    /// Build a `LayoutMetrics` from simplified args.
    fn make_layout(
        viewport_height: usize,
        total_content_height: usize,
        turns: &[(&str, usize)],
        focused_turn_top: Option<usize>,
    ) -> LayoutMetrics {
        let turn_top_offsets: Vec<(TurnId, usize)> = turns
            .iter()
            .map(|(id, off)| (TurnId(id.to_string()), *off))
            .collect();
        LayoutMetrics {
            viewport_height,
            total_content_height,
            turn_top_offsets,
            focused_turn_top,
        }
    }

    // -----------------------------------------------------------------------
    // AC1 — default state
    // -----------------------------------------------------------------------

    #[test]
    fn default_view_state_is_following_tier1_zero_offset_no_focus_zero_pending() {
        let vs = ViewState::default();
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.summary_tier, SummaryTier::Tier1);
        assert_eq!(vs.scroll_offset, 0);
        assert_eq!(vs.focused_turn, None);
        assert_eq!(vs.pending_append_lines, 0);
        assert!(vs.collapsed.is_empty());
    }

    // -----------------------------------------------------------------------
    // AC3 — summary tier toggle
    // -----------------------------------------------------------------------

    #[test]
    fn summary_tier_toggle_round_trips() {
        let mut vs = ViewState::default();
        assert_eq!(vs.summary_tier, SummaryTier::Tier1);
        vs.toggle_summary_tier();
        assert_eq!(vs.summary_tier, SummaryTier::Tier2);
        vs.toggle_summary_tier();
        assert_eq!(vs.summary_tier, SummaryTier::Tier1);
    }

    #[test]
    fn toggle_summary_tier_does_not_change_mode() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.toggle_summary_tier();
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.summary_tier, SummaryTier::Tier2);
    }

    // -----------------------------------------------------------------------
    // AC4 — collapse policy
    // -----------------------------------------------------------------------

    #[test]
    fn running_turn_overrides_user_collapse() {
        let mut vs = ViewState::default();
        let t = running_turn("t1");
        vs.collapsed.insert(t.id.clone(), true);
        // Running override → expanded.
        assert!(!vs.is_collapsed(&t));
    }

    #[test]
    fn error_invocation_overrides_user_collapse() {
        let mut vs = ViewState::default();
        let t = error_turn("t1");
        vs.collapsed.insert(t.id.clone(), true);
        // Error override → expanded.
        assert!(!vs.is_collapsed(&t));
    }

    #[test]
    fn cancelled_invocation_does_not_override_user_collapse() {
        let mut vs = ViewState::default();
        let t = cancelled_turn("t1");
        vs.collapsed.insert(t.id.clone(), true);
        // Cancelled is NOT failure — respects user collapse.
        assert!(vs.is_collapsed(&t));
    }

    #[test]
    fn completed_clean_turn_uses_user_state() {
        let mut vs = ViewState::default();
        let t = completed_clean_turn("t1");
        vs.collapsed.insert(t.id.clone(), true);
        // User toggled collapsed → collapsed.
        assert!(vs.is_collapsed(&t));
    }

    #[test]
    fn completed_clean_turn_default_expands() {
        let vs = ViewState::default();
        let t = completed_clean_turn("t1");
        // No map entry, completed turn → expanded (flipped default).
        assert!(!vs.is_collapsed(&t));
    }

    #[test]
    fn toggle_fold_round_trips() {
        let mut vs = ViewState::default();
        let tid = TurnId("t1".into());
        // First toggle of untouched turn → collapsed (true).
        vs.toggle_fold(&tid);
        assert_eq!(vs.collapsed.get(&tid), Some(&true));
        // Second toggle → expanded (false).
        vs.toggle_fold(&tid);
        assert_eq!(vs.collapsed.get(&tid), Some(&false));
    }

    #[test]
    fn toggle_fold_during_running_turn_persists_after_completion() {
        let mut vs = ViewState::default();
        let mut t = running_turn("t1");
        vs.toggle_fold(&t.id);
        // is_collapsed returns false due to running override.
        assert!(!vs.is_collapsed(&t));
        // Mutate to completed clean.
        t.stop_reason = Some(crate::domain::models::stream::StopReason::EndTurn);
        // User toggle is now visible (collapsed).
        assert!(vs.is_collapsed(&t));
    }

    #[test]
    fn expand_all_empty_slice_no_op() {
        let mut vs = ViewState::default();
        vs.expand_all(&[]);
        assert!(vs.collapsed.is_empty());
    }

    #[test]
    fn collapse_all_marks_every_turn_collapsed() {
        let mut vs = ViewState::default();
        let t1 = completed_clean_turn("t1");
        let t2 = completed_clean_turn("t2");
        vs.collapse_all(&[t1.clone(), t2.clone()]);
        assert_eq!(vs.collapsed.get(&t1.id), Some(&true));
        assert_eq!(vs.collapsed.get(&t2.id), Some(&true));
    }

    #[test]
    fn expand_all_marks_every_turn_expanded() {
        let mut vs = ViewState::default();
        let t1 = completed_clean_turn("t1");
        let t2 = completed_clean_turn("t2");
        // Set both collapsed first.
        vs.collapse_all(&[t1.clone(), t2.clone()]);
        vs.expand_all(&[t1.clone(), t2.clone()]);
        assert_eq!(vs.collapsed.get(&t1.id), Some(&false));
        assert_eq!(vs.collapsed.get(&t2.id), Some(&false));
    }

    // -----------------------------------------------------------------------
    // AC5 — purity / edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn reconcile_with_empty_turn_top_offsets_in_following_does_not_panic() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 5, &[], None);
        let result = vs.reconcile(None, &layout);
        assert_eq!(result, 0);
        assert_eq!(vs.mode, AnchorMode::Following);
    }

    #[test]
    fn reconcile_with_viewport_height_zero_does_not_panic() {
        let mut vs = ViewState::default();
        let layout = make_layout(0, 10, &[], None);
        let result = vs.reconcile(None, &layout);
        assert_eq!(result, 0);
    }

    #[test]
    fn scroll_lineup_with_content_smaller_than_viewport_clamps_to_zero() {
        let mut vs = ViewState::default();
        // total_content=5, viewport=10 → max_offset=0 (content fits in viewport).
        let layout = make_layout(10, 5, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.scroll_offset, 0);
        assert_eq!(vs.mode, AnchorMode::Reading);
    }

    #[test]
    fn pinned_with_empty_turn_top_offsets_degrades_to_reading() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("missing".into()),
            line_in_turn: 0,
        });
        let layout = make_layout(10, 30, &[], None);
        let result = vs.reconcile(None, &layout);
        // Should degrade to Reading.
        assert_eq!(vs.mode, AnchorMode::Reading);
        // scroll_offset was 0, clamped to 0.
        assert_eq!(result, 0);
    }

    #[test]
    fn reconcile_none_idempotence_across_modes() {
        let layout = make_layout(10, 30, &[("t1", 0), ("t2", 10), ("t3", 20)], Some(10));

        for (mode, initial_offset) in [
            (AnchorMode::Following, 0usize),
            (AnchorMode::Reading, 5usize),
            (
                AnchorMode::Pinned(AnchorRef {
                    turn_id: TurnId("t2".into()),
                    line_in_turn: 0,
                }),
                0usize,
            ),
        ] {
            let mut vs = ViewState::default();
            vs.mode = mode.clone();
            vs.scroll_offset = initial_offset;
            vs.pending_append_lines = 7;
            let r1 = vs.reconcile(None, &layout);
            let mode1 = vs.mode.clone();
            let pending1 = vs.pending_append_lines;
            let r2 = vs.reconcile(None, &layout);
            let mode2 = vs.mode.clone();
            let pending2 = vs.pending_append_lines;
            assert_eq!(r1, r2, "idempotent scroll_offset for {:?}", mode1);
            assert_eq!(mode1, mode2, "idempotent mode for {:?}", mode1);
            assert_eq!(pending1, pending2, "idempotent pending for {:?}", mode1);
        }
    }

    // -----------------------------------------------------------------------
    // AC6 — mode transitions (19 named tests)
    // -----------------------------------------------------------------------

    // 1. transition_any_on_submit_yields_following_at_zero
    #[test]
    fn transition_any_on_submit_yields_following_at_zero() {
        let layout = make_layout(10, 30, &[("t1", 0)], None);
        for (mode, initial_offset) in [
            (AnchorMode::Following, 0usize),
            (AnchorMode::Reading, 10usize),
            (
                AnchorMode::Pinned(AnchorRef {
                    turn_id: TurnId("t1".into()),
                    line_in_turn: 0,
                }),
                5usize,
            ),
        ] {
            let mut vs = ViewState::default();
            vs.mode = mode.clone();
            vs.scroll_offset = initial_offset;
            vs.pending_append_lines = 42;
            let r = vs.reconcile(Some(ViewEvent::Submit), &layout);
            assert_eq!(vs.mode, AnchorMode::Following);
            assert_eq!(vs.scroll_offset, 0);
            assert_eq!(r, 0);
            assert_eq!(vs.pending_append_lines, 0);
        }
    }

    // 2. transition_following_on_scroll_lineup_yields_reading
    #[test]
    fn transition_following_on_scroll_lineup_yields_reading() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 1);
    }

    // 3. transition_following_on_scroll_top_yields_reading_at_max
    #[test]
    fn transition_following_on_scroll_top_yields_reading_at_max() {
        let mut vs = ViewState::default();
        // max_offset = 50 - 10 = 40
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::Top)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 40);
    }

    // 4. transition_following_on_scroll_linedown_stays_following_at_zero
    #[test]
    fn transition_following_on_scroll_linedown_stays_following_at_zero() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineDown)), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.scroll_offset, 0);
    }

    // 5. transition_reading_on_scroll_lineup_stays_reading_increments_offset
    #[test]
    fn transition_reading_on_scroll_lineup_stays_reading_increments_offset() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 5;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 6);
    }

    // 6. transition_reading_on_scroll_linedown_landing_at_zero_yields_following
    #[test]
    fn transition_reading_on_scroll_linedown_landing_at_zero_yields_following() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 1;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineDown)), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.scroll_offset, 0);
    }

    // 7. transition_reading_on_scroll_linedown_not_at_zero_stays_reading
    #[test]
    fn transition_reading_on_scroll_linedown_not_at_zero_stays_reading() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 10;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineDown)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 9);
    }

    // 8. transition_reading_on_scroll_top_stays_reading_at_max
    #[test]
    fn transition_reading_on_scroll_top_stays_reading_at_max() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 5;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::Top)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 40);
    }

    // 9. transition_reading_on_scroll_bottom_yields_following
    #[test]
    fn transition_reading_on_scroll_bottom_yields_following() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 20;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::Bottom)), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.scroll_offset, 0);
    }

    // 10. transition_pinned_on_scroll_lineup_degrades_to_reading
    #[test]
    fn transition_pinned_on_scroll_lineup_degrades_to_reading() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[("t1", 0), ("t2", 20)], Some(20));
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("t2".into()),
            line_in_turn: 0,
        });
        vs.scroll_offset = 10;
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 11);
    }

    // 11. transition_pinned_on_scroll_linedown_degrades_to_reading
    #[test]
    fn transition_pinned_on_scroll_linedown_degrades_to_reading() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[("t1", 0), ("t2", 20)], Some(20));
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("t2".into()),
            line_in_turn: 0,
        });
        vs.scroll_offset = 10;
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineDown)), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 9);
    }

    // 12. transition_any_on_jumpturn_yields_pinned_with_top_at_viewport_top
    #[test]
    fn transition_any_on_jumpturn_yields_pinned_with_top_at_viewport_top() {
        let layout = make_layout(10, 50, &[("t1", 0), ("t2", 15), ("t3", 30)], Some(15));
        let max_off: usize = 50 - 10; // 40

        for (mode, initial_offset) in [
            (AnchorMode::Following, 0usize),
            (AnchorMode::Reading, 10usize),
            (
                AnchorMode::Pinned(AnchorRef {
                    turn_id: TurnId("t1".into()),
                    line_in_turn: 0,
                }),
                5usize,
            ),
        ] {
            let mut vs = ViewState::default();
            vs.mode = mode.clone();
            vs.scroll_offset = initial_offset;
            vs.reconcile(
                Some(ViewEvent::JumpTurn {
                    turn_id: TurnId("t2".into()),
                }),
                &layout,
            );
            assert!(
                matches!(vs.mode, AnchorMode::Pinned(_)),
                "mode should be Pinned for start mode {:?}",
                mode
            );
            // t2 at lines-from-top=15, viewport=10 → offset = 40 - 15 = 25
            assert_eq!(vs.scroll_offset, max_off.saturating_sub(15));
        }
    }

    // 13. transition_following_on_foldtoggle_stays_following_no_offset_change
    #[test]
    fn transition_following_on_foldtoggle_with_focus_transitions_to_reading_and_preserves_anchor() {
        // Amended: Following mode fold toggle transitions to Reading and preserves anchor.
        // t_old=20, s_old=0, prev_max_offset=40 → focused_screen_row = 0 (clamped)
        // t_new=20, new_max_offset=40 → new_top_visible=20, new_offset=40-20=20
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Following;
        vs.scroll_offset = 0;
        let layout = make_layout(10, 50, &[("t1", 0), ("t2", 20)], Some(20));
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: Some(20),
                prev_max_offset: 40,
            }),
            &layout,
        );
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 20);
    }

    // 14. transition_reading_on_foldtoggle_with_focus_preserves_focused_screen_row
    #[test]
    fn transition_reading_on_foldtoggle_with_focus_preserves_focused_screen_row() {
        // Worked example from module doc:
        // viewport=10, T_old=13, S_old=15, prev_max_offset=23,
        // T_new=6, max_after=16, expected=15
        let layout_after = make_layout(10, 26, &[("t1", 0), ("t2", 6)], Some(6));
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 15;
        vs.focused_turn = Some(TurnId("t2".into()));
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: Some(13),
                prev_max_offset: 23,
            }),
            &layout_after,
        );
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 15);
    }

    // 15. transition_reading_on_foldtoggle_without_focus_clamps_to_new_max
    #[test]
    fn transition_reading_on_foldtoggle_without_focus_clamps_to_new_max() {
        let layout_after = make_layout(10, 26, &[("t1", 0)], None);
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 20;
        // new max = 26 - 10 = 16; 20 > 16 → clamp to 16.
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: None,
                prev_max_offset: 30,
            }),
            &layout_after,
        );
        assert!(vs.scroll_offset <= 16);
        assert_eq!(vs.scroll_offset, 16);
    }

    // 16. transition_pinned_on_streamappend_stays_pinned_with_resolved_offset
    #[test]
    fn transition_pinned_on_streamappend_stays_pinned_with_resolved_offset() {
        let layout = make_layout(10, 50, &[("t1", 0), ("t2", 20)], Some(20));
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("t2".into()),
            line_in_turn: 0,
        });
        vs.scroll_offset = 10;
        let r = vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 3 }), &layout);
        // t2 at 20, max_offset = 40, offset = 40 - 20 = 20
        assert!(matches!(vs.mode, AnchorMode::Pinned(_)));
        assert_eq!(vs.scroll_offset, 20);
        assert_eq!(vs.pending_append_lines, 3);
        let _ = r;
    }

    // 17. transition_pinned_with_missing_turn_id_on_streamappend_degrades_to_reading_with_warn
    #[test]
    fn transition_pinned_with_missing_turn_id_on_streamappend_degrades_to_reading_with_warn() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("ghost".into()),
            line_in_turn: 0,
        });
        vs.scroll_offset = 7;
        let layout = make_layout(10, 50, &[("t1", 0)], None);
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 5 }), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.pending_append_lines, 5);
        // scroll_offset was 7, clamped against new max (40).
        assert_eq!(vs.scroll_offset, 7);
    }

    // 18. transition_following_on_streamappend_stays_at_zero
    #[test]
    fn transition_following_on_streamappend_stays_at_zero() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[("t1", 0)], None);
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 2 }), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.scroll_offset, 0);
        assert_eq!(vs.pending_append_lines, 0);
    }

    // 19. transition_reading_on_streamappend_keeps_offset_increments_pending
    #[test]
    fn transition_reading_on_streamappend_keeps_offset_increments_pending() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 5;
        let layout = make_layout(10, 50, &[("t1", 0)], None);
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 4 }), &layout);
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 5);
        assert_eq!(vs.pending_append_lines, 4);
    }

    // -----------------------------------------------------------------------
    // AC8 — submit transitions
    // -----------------------------------------------------------------------

    #[test]
    fn submit_from_reading_yields_following_at_bottom() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 30;
        vs.pending_append_lines = 99;
        let layout = make_layout(10, 50, &[], None);
        let r = vs.reconcile(Some(ViewEvent::Submit), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.scroll_offset, 0);
        assert_eq!(r, 0);
        assert_eq!(vs.pending_append_lines, 0);
    }

    #[test]
    fn submit_from_pinned_yields_following_at_bottom() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("t1".into()),
            line_in_turn: 0,
        });
        vs.scroll_offset = 15;
        vs.pending_append_lines = 55;
        let layout = make_layout(10, 50, &[("t1", 0)], None);
        let r = vs.reconcile(Some(ViewEvent::Submit), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.scroll_offset, 0);
        assert_eq!(r, 0);
        assert_eq!(vs.pending_append_lines, 0);
    }

    // -----------------------------------------------------------------------
    // AC9 — pending_append_lines accumulation and reset
    // -----------------------------------------------------------------------

    #[test]
    fn streamappend_in_following_keeps_pending_zero() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 3 }), &layout);
        assert_eq!(vs.pending_append_lines, 0);
    }

    #[test]
    fn streamappend_in_reading_accumulates_pending() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 10;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 3 }), &layout);
        assert_eq!(vs.pending_append_lines, 3);
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 5 }), &layout);
        assert_eq!(vs.pending_append_lines, 8);
    }

    #[test]
    fn streamappend_in_pinned_accumulates_pending() {
        let mut vs = ViewState::default();
        let layout = make_layout(10, 50, &[("t1", 0)], None);
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("t1".into()),
            line_in_turn: 0,
        });
        vs.reconcile(Some(ViewEvent::StreamAppend { appended_lines: 2 }), &layout);
        assert_eq!(vs.pending_append_lines, 2);
    }

    #[test]
    fn submit_resets_pending_to_zero() {
        let mut vs = ViewState::default();
        vs.pending_append_lines = 42;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Submit), &layout);
        assert_eq!(vs.pending_append_lines, 0);
    }

    #[test]
    fn scroll_bottom_from_reading_resets_pending() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 10;
        vs.pending_append_lines = 7;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::Bottom)), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.pending_append_lines, 0);
    }

    #[test]
    fn scroll_linedown_landing_at_zero_resets_pending() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 1;
        vs.pending_append_lines = 5;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineDown)), &layout);
        assert_eq!(vs.mode, AnchorMode::Following);
        assert_eq!(vs.pending_append_lines, 0);
    }

    #[test]
    fn scroll_lineup_in_reading_does_not_reset_pending() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 5;
        vs.pending_append_lines = 12;
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.pending_append_lines, 12);
    }

    // -----------------------------------------------------------------------
    // Additional AC6/AC7 fold-toggle tests
    // -----------------------------------------------------------------------

    #[test]
    fn fold_toggle_with_focused_turn_in_pinned_preserves_screen_row() {
        // Same worked example but mode is Pinned.
        let layout_after = make_layout(10, 26, &[("t1", 0), ("t2", 6)], Some(6));
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("t2".into()),
            line_in_turn: 0,
        });
        vs.scroll_offset = 15;
        vs.focused_turn = Some(TurnId("t2".into()));
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: Some(13),
                prev_max_offset: 23,
            }),
            &layout_after,
        );
        assert!(matches!(vs.mode, AnchorMode::Pinned(_)));
        assert_eq!(vs.scroll_offset, 15);
    }

    #[test]
    fn fold_toggle_without_focus_clamps_to_new_max() {
        let layout_after = make_layout(10, 20, &[("t1", 0)], None);
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 15;
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: None,
                prev_max_offset: 30,
            }),
            &layout_after,
        );
        // new max = 20 - 10 = 10. scroll_offset was 15 → clamp to 10.
        assert_eq!(vs.scroll_offset, 10);
    }

    #[test]
    fn fold_toggle_in_following_with_focus_transitions_to_reading_and_preserves_anchor() {
        // Amended: Following + FoldToggle → Reading with anchor preserved.
        // t_old=0, s_old=0, prev_max=40 → focused_screen_row=0, t_new=0,
        // new_max=40 → new_offset=40
        let layout = make_layout(10, 50, &[("t1", 0)], Some(0));
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Following;
        vs.scroll_offset = 0;
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: Some(0),
                prev_max_offset: 40,
            }),
            &layout,
        );
        assert_eq!(vs.mode, AnchorMode::Reading);
        assert_eq!(vs.scroll_offset, 40);
    }

    #[test]
    fn fold_toggle_with_prev_focused_turn_top_none_uses_viewport_top_fallback() {
        let layout_after = make_layout(10, 20, &[("t1", 0)], Some(5));
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 8;
        vs.reconcile(
            Some(ViewEvent::FoldToggle {
                turn_id: TurnId("t1".into()),
                prev_focused_turn_top: None,
                prev_max_offset: 25,
            }),
            &layout_after,
        );
        // new max = 20-10=10. scroll_offset 8 < 10, stays at 8.
        assert_eq!(vs.scroll_offset, 8);
    }

    // -----------------------------------------------------------------------
    // scroll_at_reading_clamp test (scroll up beyond max)
    // -----------------------------------------------------------------------

    #[test]
    fn scroll_lineup_beyond_max_clamps_to_max() {
        let mut vs = ViewState::default();
        vs.mode = AnchorMode::Reading;
        vs.scroll_offset = 39;
        // max_offset = 50 - 10 = 40. 39 + 1 = 40, at max.
        let layout = make_layout(10, 50, &[], None);
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.scroll_offset, 40);
        // Already at max, another scroll up should clamp.
        vs.reconcile(Some(ViewEvent::Scroll(ScrollDelta::LineUp)), &layout);
        assert_eq!(vs.scroll_offset, 40);
    }
}
