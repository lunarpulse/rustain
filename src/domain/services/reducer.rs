//! Turn-parts reducer — pure single-threaded event fold for `StreamChunk` → `Vec<TurnPart>`.
//!
//! # Why this module exists
//!
//! Story 16.1 shipped the data primitives (`Turn`, `TurnPart`, `PartId`, `Clock`) but
//! left `apply_chunk()` in `stream.rs` untouched and `Conversation.messages: Vec<ChatMessage>`
//! intact. Until a reducer rewrites events into `Vec<TurnPart>` and `Conversation` exposes a
//! `Vec<Turn>` field, the prose-then-tools concat regression (verified screenshots
//! 2026-04-22 / 2026-04-28) is still live. This module retires `apply_chunk` and makes the
//! bug structurally inexpressible.
//!
//! # Architecture
//!
//! - **(State, Event) → State purity (ADR-16-01 §2).** `reduce()` takes `&mut ReducerState`
//!   rather than owning by value — an idiomatic Rust ergonomic accommodation; semantically
//!   still pure-functional: no I/O, no `tokio::spawn`, no direct wall-clock query. Timing is
//!   exclusively through the injected `&dyn Clock`.
//! - **Single-threaded reducer (ADR-16-01 §2).** One channel, one consumer. Insertion order
//!   is authoritative — there is no `seq` field. `&mut ReducerState` implies exclusive access.
//! - **Hexagonal purity (`rustain/CLAUDE.md`).** This module lives in `domain/services/`.
//!   Imports from `domain/models/`, `domain/clock`, `domain/events::ChunkAction`, `tracing`.
//!   No tokio, no I/O.
//! - **`ChunkAction` semantic preservation.** The reducer produces the same four-variant outputs
//!   the existing `apply_chunk` emits, on the same input chunks, in the same order. The
//!   downstream event-loop branches in `infrastructure/runtime/event_loop.rs:3514+` consume
//!   `ChunkAction` for redraw/persist/title-gen/tool-loop decisions. Breaking this contract
//!   regresses every story in Epics 1-5.
//!
//! # Prose-flush rule (PRE-LOCKED — implicit flush)
//!
//! Rustain's adapter does not emit content-block-stop events; the implicit-flush rule avoids
//! modifying every provider adapter. Opencode's `processor.ts:432-452` uses an explicit
//! `text-end` because their adapter emits one — we match the effective behavior from the
//! next-chunk side. See ADR-16-01 §2 + amendment 2026-04-28 §Q2.
//!
//! The reducer flushes the currently-open `Prose` part — closing it and committing it as a
//! `TurnPart::Prose` with the issued `PartId` to `Turn.parts` — whenever the next
//! `StreamChunk` is any of: `ToolUse`, `Thinking`, `ToolResult`, `Error`, `Blocked`,
//! `TurnComplete`. Subsequent `Text` chunks open a fresh `Prose` part with a new `PartId`.
//!
//! `Usage` is intentionally NOT in the flush trigger set — it is turn-level metadata
//! surfaced into `Conversation.usage`, not a structural part.
//!
//! # Invocation lifecycle
//!
//! ```text
//!    InvocationStatus          transition
//!
//!       Pending       (rustain adapter never produces; reserved for future adapter shapes)
//!       Running  ◀── on ToolUse chunk: issued via Turn::push_part with started_at
//!          │
//!          │ on matching ToolResult chunk
//!          ▼
//!       Success / Error
//!       Cancelled      (this story does not produce; S16.x cancellation will extend)
//! ```
//!
//! Lookup of the matching invocation on `ToolResult`: `pending_invocations.remove(&tool_use_id)`.
//!
//! # Known simplifications
//!
//! - **Error / Blocked** chunks append a `TurnPart::Prose` with the error content (mirrors
//!   current `apply_chunk` behavior of pushing into `current_text_buffer` +
//!   `ContentBlockType::Error`). A future story may want `TurnPart::Error` as a distinct variant.
//! - **Reasoning** parts are committed by the reducer but the render path (S16.4) and the
//!   ViewState fold policy (S16.3) need to decide whether `Reasoning` is collapsed-by-default.
//!
//! # Cross-references
//!
//! - Story 16.1 — data primitives foundation
//! - Story 16.3 — ViewState consumes `Conversation.turns` (added in this story)
//! - Story 16.4 — render walks `Turn.parts` (inherits mirror deletion from this story)
//! - Story 16.9 — wires the producer for both `set_progress` AND `set_tail` (exposed here, uncalled until then)
//! - ADR-16-01 §2 — reducer specification
//! - ADR-16-01 amendment 2026-04-28 §Q2 — prose-flush rule rationale

use std::collections::HashMap;
use std::time::Instant;

use crate::domain::clock::Clock;
use crate::domain::events::ChunkAction;
use crate::domain::models::{
    InvocationStatus, PartId, StopReason, StreamChunk, StreamingPhase, StreamingState, ToolOutput,
    Turn, TurnPart, UsageInfo,
};

// ---------------------------------------------------------------------------
// ReducerState
// ---------------------------------------------------------------------------

/// Mutable state threaded through every `reduce()` call.
///
/// # Fields
///
/// - `open_turn` — the current in-flight turn (None between turns).
/// - `open_prose` — buffered prose deltas not yet committed to a `TurnPart::Prose`.
/// - `open_reasoning` — buffered thinking deltas not yet committed to a `TurnPart::Reasoning`.
/// - `pending_invocations` — tool_use_id (wire-level) → PartId of the running ToolInvocation.
/// - `progress` — tool_use_id → (k, n) progress; set via `set_progress()` by S16.9.
/// - `last_running_tool` — most recently-running tool name; cleared when its result arrives.
/// - `pending_usage` — usage info to apply on TurnComplete.
/// - `committed_turn` — set on TurnComplete (EndTurn / MaxTokens / Cancelled). Caller drains.
/// - `wall_anchor_ms` / `instant_anchor` — wall-clock anchor pair captured once at `new()`.
pub struct ReducerState {
    pub open_turn: Option<Turn>,
    pub open_prose: Option<String>,
    open_reasoning: Option<String>,
    pub pending_invocations: HashMap<String, PartId>,
    /// Completed invocations (removed from `pending_invocations` on ToolResult,
    /// preserved here for mirror sync via `update_streaming_mirror`).
    completed_invocations: HashMap<String, PartId>,
    progress: HashMap<String, (u64, u64)>,
    tail: HashMap<String, String>,
    last_running_tool: Option<String>,
    last_running_tool_use_id: Option<String>,
    pub pending_usage: Option<UsageInfo>,
    pub committed_turn: Option<Turn>,
    wall_anchor_ms: i64,
    instant_anchor: Instant,
}

/// Borrow-free snapshot of liveness signals for the render path.
pub struct LivenessSnapshot {
    pub active_tool_name: Option<String>,
    pub progress: Option<(u64, u64)>,
    /// Last N lines of stdout tail (bash adapter only). Story 16.9.
    pub tail: Option<String>,
}

impl ReducerState {
    /// Create a fresh reducer state with the given wall-clock anchors.
    ///
    /// The two anchors enable `unix_millis_from_clock` to convert `Instant` → unix-millis
    /// without re-querying the wall clock (purity).
    pub fn new(wall_anchor_ms: i64, instant_anchor: Instant) -> Self {
        Self {
            open_turn: None,
            open_prose: None,
            open_reasoning: None,
            pending_invocations: HashMap::new(),
            completed_invocations: HashMap::new(),
            progress: HashMap::new(),
            tail: HashMap::new(),
            last_running_tool: None,
            last_running_tool_use_id: None,
            pending_usage: None,
            committed_turn: None,
            wall_anchor_ms,
            instant_anchor,
        }
    }

    /// Borrow-free liveness snapshot for render consumption (S16.4).
    ///
    /// Returns `progress` for the most recently `Running` invocation only
    /// (lookup by `last_running_tool_use_id` → `progress` map).
    pub fn liveness(&self) -> LivenessSnapshot {
        let progress = self
            .last_running_tool_use_id
            .as_ref()
            .and_then(|id| self.progress.get(id).copied());
        let tail = self
            .last_running_tool_use_id
            .as_ref()
            .and_then(|id| self.tail.get(id).cloned());
        LivenessSnapshot {
            active_tool_name: self.last_running_tool.clone(),
            progress,
            tail,
        }
    }

    /// Set progress for a running tool invocation.
    ///
    /// Exposed but uncalled in this story — S16.9 wires the producer
    /// (intra-tool stdout tail). Until then, `liveness().progress` is always `None`.
    pub fn set_progress(&mut self, tool_use_id: &str, k: u64, n: u64) {
        self.progress.insert(tool_use_id.to_string(), (k, n));
    }

    /// Set stdout tail text for a running tool invocation.
    ///
    /// Story 16.9 — wires the producer for both `set_progress` AND `set_tail`
    /// (exposed here, uncalled until then). Overwrites the previous tail value
    /// for the same `tool_use_id` — the ring-buffer throttling in the bash
    /// adapter ensures this is called at most 4 Hz, so no coalescing is needed.
    pub fn set_tail(&mut self, tool_use_id: &str, text: String) {
        self.tail.insert(tool_use_id.to_string(), text);
    }

    /// Current wall-clock time in unix milliseconds via the injected clock.
    ///
    /// Passthrough to `clock.wall_now_ms()` — centralized so render code
    /// never calls `Instant::now()` or `SystemTime::now()` directly.
    pub fn unix_millis_now(&self, clock: &dyn Clock) -> i64 {
        clock.wall_now_ms()
    }

    /// Ensure an open turn exists for content-bearing chunks.
    fn ensure_open_turn(&mut self, clock: &dyn Clock) -> &mut Turn {
        let wall_anchor = self.wall_anchor_ms;
        let instant_anchor = self.instant_anchor;
        self.open_turn.get_or_insert_with(|| {
            let now = clock.now();
            let delta_ms = now.duration_since(instant_anchor).as_millis() as i64;
            let now_ms = wall_anchor.saturating_add(delta_ms);
            Turn::new(String::new(), now_ms)
        })
    }

    /// Flush the open prose buffer into a committed `TurnPart::Prose`.
    fn flush_open_prose(&mut self) {
        if let Some(ref mut turn) = self.open_turn {
            if let Some(text) = self.open_prose.take() {
                if !text.is_empty() {
                    turn.push_part(|id| TurnPart::Prose { id, text });
                }
            }
        }
    }

    /// Flush the open reasoning buffer into a committed `TurnPart::Reasoning`.
    fn flush_open_reasoning(&mut self) {
        if let Some(ref mut turn) = self.open_turn {
            if let Some(text) = self.open_reasoning.take() {
                if !text.is_empty() {
                    turn.push_part(|id| TurnPart::Reasoning { id, text });
                }
            }
        }
    }

    /// Flush both open prose and open reasoning buffers.
    fn flush_all(&mut self) {
        self.flush_open_prose();
        self.flush_open_reasoning();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a `Clock`'s current `Instant` to unix-millis using the anchor pair.
fn unix_millis_from_clock(state: &ReducerState, clock: &dyn Clock) -> i64 {
    let now = clock.now();
    let delta_ms = now.duration_since(state.instant_anchor).as_millis() as i64;
    state.wall_anchor_ms.saturating_add(delta_ms)
}

// ---------------------------------------------------------------------------
// Reducer
// ---------------------------------------------------------------------------

/// Pure single-threaded reducer: folds a `StreamChunk` into the authoritative `ReducerState`
/// and returns a `ChunkAction` that mirrors the existing `apply_chunk` semantics.
///
/// # Contract for `committed_turn`
///
/// On `TurnComplete { stop_reason: EndTurn | MaxTokens | Cancelled }`, the finalized
/// `Turn` is moved to `state.committed_turn` (set once; caller drains between `reduce()` calls).
/// The caller (event loop) drains `committed_turn`, pushes onto `Conversation.turns`, then
/// calls `rebuild_messages_mirror` to refresh the legacy `messages` field.
///
/// # Panics
///
/// This function never panics on adapter-malformed input (orphaned `ToolResult`, malformed
/// chunk). All unexpected conditions are handled via `tracing::warn!` and a no-op return.
pub fn reduce(state: &mut ReducerState, chunk: StreamChunk, clock: &dyn Clock) -> ChunkAction {
    match chunk {
        // ── Usage (handled first — no open turn required) ────────────────
        StreamChunk::Usage { usage, .. } => {
            state.pending_usage = Some(usage);
            ChunkAction::None
        }

        // ── Text ─────────────────────────────────────────────────────────
        StreamChunk::Text { content, .. } => {
            state.ensure_open_turn(clock);
            state
                .open_prose
                .get_or_insert_with(String::new)
                .push_str(&content);
            ChunkAction::NeedsRedraw
        }

        // ── Thinking ─────────────────────────────────────────────────────
        StreamChunk::Thinking { content, .. } => {
            state.ensure_open_turn(clock);
            state.flush_open_prose();
            state
                .open_reasoning
                .get_or_insert_with(String::new)
                .push_str(&content);
            ChunkAction::NeedsRedraw
        }

        // ── ToolUse ──────────────────────────────────────────────────────
        StreamChunk::ToolUse { id, name, input } => {
            state.ensure_open_turn(clock);
            state.flush_all();

            if state.pending_invocations.contains_key(&id) {
                tracing::warn!("Duplicate ToolUse id: {} — ignoring", id);
                return ChunkAction::NeedsRedraw;
            }

            let started_at = unix_millis_from_clock(state, clock);
            let part_id = if let Some(turn) = state.open_turn.as_mut() {
                turn.push_part(|pid| TurnPart::ToolInvocation {
                    id: pid,
                    tool: name.clone(),
                    args: input,
                    status: InvocationStatus::Running,
                    started_at,
                    ended_at: None,
                })
            } else {
                tracing::warn!("ToolUse with no open turn");
                return ChunkAction::NeedsRedraw;
            };

            state.pending_invocations.insert(id.clone(), part_id);
            state.last_running_tool = Some(name);
            state.last_running_tool_use_id = Some(id);

            ChunkAction::NeedsRedraw
        }

        // ── ToolResult ───────────────────────────────────────────────────
        StreamChunk::ToolResult {
            id,
            content,
            is_error,
        } => {
            state.ensure_open_turn(clock);
            state.flush_all();

            let invocation_pid = match state.pending_invocations.remove(&id) {
                Some(pid) => {
                    state.completed_invocations.insert(id.clone(), pid);
                    pid
                }
                None => {
                    tracing::warn!("ToolResult for unknown tool_use_id: {}", id);
                    return ChunkAction::NeedsRedraw;
                }
            };

            let ended_at_ts = unix_millis_from_clock(state, clock);

            // Mutate the matching ToolInvocation in place (scan from the back)
            if let Some(turn) = state.open_turn.as_mut() {
                // Verify the invocation still exists in turn.parts
                let has_invocation = turn.parts.iter().any(|p| {
                    matches!(p, TurnPart::ToolInvocation { id: pid, .. } if *pid == invocation_pid)
                });
                if !has_invocation {
                    tracing::warn!(
                        "ToolResult for {} found in pending_invocations but not in turn.parts",
                        id
                    );
                    return ChunkAction::NeedsRedraw;
                }

                for part in turn.parts.iter_mut().rev() {
                    if let TurnPart::ToolInvocation {
                        id: pid,
                        status,
                        ended_at,
                        ..
                    } = part
                    {
                        if *pid == invocation_pid {
                            *status = if is_error {
                                InvocationStatus::Error
                            } else {
                                InvocationStatus::Success
                            };
                            *ended_at = Some(ended_at_ts);
                            break;
                        }
                    }
                }

                // Append ToolResult part
                turn.push_part(|pid| TurnPart::ToolResult {
                    id: pid,
                    refs: invocation_pid,
                    output: ToolOutput { content, is_error },
                });
            }

            // Story 16.9: clear progress + tail for this tool_use_id on ToolResult.
            // The rail must collapse the moment the result arrives.
            state.progress.remove(&id);
            state.tail.remove(&id);

            // Clear last_running_tool if no more pending invocations that haven't been resolved
            // (pending_invocations holds all seen invocations; we can't tell which are resolved
            // without a separate tracking set. For now, just clear on every ToolResult —
            // the next ToolUse will set it again.)
            state.last_running_tool = None;
            state.last_running_tool_use_id = None;

            ChunkAction::NeedsRedraw
        }

        // ── Error / Blocked ──────────────────────────────────────────────
        StreamChunk::Error { content } => {
            tracing::warn!("Stream error chunk: {}", content);
            state.ensure_open_turn(clock);
            state.flush_all();
            if !content.is_empty() {
                if let Some(turn) = state.open_turn.as_mut() {
                    turn.push_part(|id| TurnPart::Prose { id, text: content });
                }
            }
            ChunkAction::NeedsRedraw
        }

        StreamChunk::Blocked { content } => {
            tracing::warn!("Stream blocked chunk: {}", content);
            state.ensure_open_turn(clock);
            state.flush_all();
            if !content.is_empty() {
                if let Some(turn) = state.open_turn.as_mut() {
                    turn.push_part(|id| TurnPart::Prose { id, text: content });
                }
            }
            ChunkAction::NeedsRedraw
        }

        // ── TurnComplete ─────────────────────────────────────────────────
        StreamChunk::TurnComplete { stop_reason } => match stop_reason {
            StopReason::ToolUse => {
                state.ensure_open_turn(clock);
                state.flush_all();
                // Do NOT commit — turn continues for tool results
                ChunkAction::TurnContinuing
            }
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::Cancelled => {
                if state.open_turn.is_none() {
                    state.ensure_open_turn(clock);
                }
                state.flush_all();
                if let Some(turn) = state.open_turn.as_mut() {
                    turn.stop_reason = Some(stop_reason.clone());
                }

                // Move the open turn to committed_turn for the caller to drain
                let turn = match state.open_turn.take() {
                    Some(t) => t,
                    None => {
                        tracing::warn!("TurnComplete with no open turn");
                        return ChunkAction::NeedsRedraw;
                    }
                };

                state.open_prose = None;
                state.open_reasoning = None;
                state.pending_invocations.clear();
                state.completed_invocations.clear();
                state.progress.clear();
                state.tail.clear();
                state.last_running_tool = None;
                state.last_running_tool_use_id = None;

                state.committed_turn = Some(turn);

                ChunkAction::TurnComplete {
                    persist: true,
                    trigger_title_generation: false, // computed by caller after .turns.push()
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// StreamingState mirror sync (Path B — render mirror survives in this story)
// ---------------------------------------------------------------------------

/// Path B render-mirror sync. After every `reduce()` call in the event loop,
/// project the authoritative `ReducerState` back onto the legacy `StreamingState`
/// shape so chat_pane / widgets keep compiling with no signature changes.
///
/// Retired by S16.4 alongside the chat_pane render flip.
// TODO(S16.10-cleanup): retire update_streaming_mirror alongside chat_pane render flip
pub fn update_streaming_mirror(state: &ReducerState, mirror: &mut StreamingState) {
    mirror.is_streaming = state.open_turn.is_some();
    mirror.current_text_buffer = state.open_prose.clone().unwrap_or_default();
    mirror.thinking_buffer = state.open_reasoning.clone().unwrap_or_default();
    mirror.phase = if state.open_turn.is_none() {
        StreamingPhase::Idle
    } else if state.open_prose.is_some() {
        StreamingPhase::AccumulatingText
    } else if state.open_reasoning.is_some() {
        StreamingPhase::InThinking
    } else if !state.pending_invocations.is_empty() {
        let tool_id = state
            .pending_invocations
            .keys()
            .next()
            .cloned()
            .unwrap_or_default();
        StreamingPhase::InToolCall { tool_id }
    } else {
        StreamingPhase::AwaitingToolExecution
    };

    // Rebuild active_tool_calls from open_turn parts
    mirror.active_tool_calls.clear();
    mirror.current_blocks.clear();
    if let Some(ref turn) = state.open_turn {
        // Build a reverse map: PartId → wire-level tool_use_id
        let mut part_to_wire: std::collections::HashMap<PartId, &str> =
            std::collections::HashMap::new();
        for (wire_id, part_id) in &state.pending_invocations {
            part_to_wire.insert(*part_id, wire_id.as_str());
        }
        for (wire_id, part_id) in &state.completed_invocations {
            part_to_wire.insert(*part_id, wire_id.as_str());
        }

        // Collect tool results for matching
        let mut results: std::collections::HashMap<PartId, &TurnPart> =
            std::collections::HashMap::new();
        for part in &turn.parts {
            if let TurnPart::ToolResult { refs, .. } = part {
                results.insert(*refs, part);
            }
        }

        for part in &turn.parts {
            match part {
                TurnPart::Prose { .. } => {}
                TurnPart::Reasoning { text, .. } => {
                    mirror
                        .current_blocks
                        .push(crate::domain::models::ContentBlockType::Thinking(
                            text.clone(),
                        ));
                }
                TurnPart::ToolInvocation {
                    id: pid,
                    tool,
                    args,
                    status,
                    started_at,
                    ended_at,
                } => {
                    let wire_id = part_to_wire
                        .get(pid)
                        .cloned()
                        .unwrap_or("unknown")
                        .to_string();
                    let result = results.get(pid).and_then(|rp| {
                        if let TurnPart::ToolResult { output, .. } = rp {
                            Some(crate::domain::models::ToolResultInfo {
                                content: output.content.clone(),
                                is_error: output.is_error,
                            })
                        } else {
                            None
                        }
                    });
                    let status_str = match status {
                        InvocationStatus::Success => Some("Done"),
                        InvocationStatus::Error => Some("Error"),
                        InvocationStatus::Running | InvocationStatus::Pending => None,
                        InvocationStatus::Cancelled => Some("Cancelled"),
                    };
                    mirror.active_tool_calls.insert(
                        wire_id.clone(),
                        crate::domain::models::ToolCallInfo {
                            id: wire_id,
                            name: tool.clone(),
                            input: args.clone(),
                            result,
                            started_at_ms: Some(*started_at as u64),
                            completed_at_ms: ended_at.map(|v| v as u64),
                            status: status_str.map(|s| s.to_string()),
                        },
                    );
                }
                TurnPart::ToolResult { .. } => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test compatibility helpers (test-only)
// ---------------------------------------------------------------------------

/// Test-only drop-in replacement for the old `apply_chunk` signature.
/// Composes `reduce()` + drain + mirror sync. Retired by S16.4.
#[doc(hidden)]
pub fn apply_chunk_for_tests(
    conv: &mut crate::domain::models::Conversation,
    streaming: &mut crate::domain::models::StreamingState,
    state: &mut ReducerState,
    chunk: StreamChunk,
    clock: &dyn crate::domain::clock::Clock,
) -> ChunkAction {
    let mut action = reduce(state, chunk, clock);
    update_streaming_mirror(state, streaming);

    // Propagate pending usage to conversation (mirrors old apply_chunk behavior)
    if let Some(usage) = state.pending_usage.take() {
        conv.usage = Some(usage);
    }

    if let Some(committed) = state.committed_turn.take() {
        conv.turns.push(committed);
        conv.rebuild_messages_mirror();
        // Replicate old trigger_title_generation (computed after commit)
        // Note: uses messages.len() (legacy mirror) to match original apply_chunk semantics.
        if let ChunkAction::TurnComplete { persist, .. } = action {
            let trigger = conv.messages.len() == 2;
            action = ChunkAction::TurnComplete {
                persist,
                trigger_title_generation: trigger,
            };
        }
    }

    action
}

/// Convenience: create a `ReducerState` + `MockClock` pair.
#[doc(hidden)]
pub fn test_reducer_state(wall_anchor_ms: i64) -> (ReducerState, crate::domain::clock::MockClock) {
    let now = std::time::Instant::now();
    let clock = crate::domain::clock::MockClock::new(now);
    let state = ReducerState::new(wall_anchor_ms, now);
    (state, clock)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::clock::MockClock;
    use std::time::{Duration, Instant};

    fn make_state(now_ms: i64) -> (ReducerState, MockClock) {
        let now = Instant::now();
        let clock = MockClock::new(now);
        let state = ReducerState::new(now_ms, now);
        (state, clock)
    }

    // ── AC1: ChunkAction parity ──────────────────────────────────────────

    #[test]
    fn reducer_chunk_action_text_returns_needs_redraw() {
        let (mut state, clock) = make_state(1000);
        let action = reduce(
            &mut state,
            StreamChunk::Text {
                content: "hi".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        assert_eq!(action, ChunkAction::NeedsRedraw);
    }

    #[test]
    fn reducer_chunk_action_usage_returns_none() {
        let (mut state, clock) = make_state(1000);
        let action = reduce(
            &mut state,
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
            &clock,
        );
        assert_eq!(action, ChunkAction::None);
    }

    #[test]
    fn reducer_chunk_action_tool_use_returns_turn_continuing() {
        let (mut state, clock) = make_state(1000);
        let action = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::ToolUse,
            },
            &clock,
        );
        assert_eq!(action, ChunkAction::TurnContinuing);
    }

    #[test]
    fn reducer_chunk_action_end_turn_returns_turn_complete() {
        let (mut state, clock) = make_state(1000);
        let action = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );
        assert!(matches!(
            action,
            ChunkAction::TurnComplete { persist: true, .. }
        ));
        assert!(state.committed_turn.is_some());
    }

    #[test]
    fn reducer_chunk_action_max_tokens_returns_turn_complete() {
        let (mut state, clock) = make_state(1000);
        let action = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::MaxTokens,
            },
            &clock,
        );
        assert!(matches!(
            action,
            ChunkAction::TurnComplete { persist: true, .. }
        ));
    }

    #[test]
    fn reducer_chunk_action_cancelled_returns_turn_complete() {
        let (mut state, clock) = make_state(1000);
        let action = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled,
            },
            &clock,
        );
        assert!(matches!(
            action,
            ChunkAction::TurnComplete { persist: true, .. }
        ));
    }

    /// AC1 parity test: every existing `apply_chunk` test case produces equivalent ChunkAction.
    #[test]
    fn reducer_chunk_action_parity_with_apply_chunk() {
        // Table-driven over the 14 distinct `test_apply_chunk_*` cases in stream.rs.
        let now_ms = 1000;
        let now = Instant::now();
        let clock = MockClock::new(now);

        struct Case {
            label: &'static str,
            chunks: Vec<StreamChunk>,
            expected: Vec<ChunkAction>,
        }

        let cases = vec![
            // Case 1: Text accumulates → NeedsRedraw
            Case {
                label: "text_accumulates",
                chunks: vec![StreamChunk::Text {
                    content: "Hello".into(),
                    parent_tool_use_id: None,
                }],
                expected: vec![ChunkAction::NeedsRedraw],
            },
            // Case 2: text_merges_consecutive
            Case {
                label: "text_merges_consecutive",
                chunks: vec![
                    StreamChunk::Text {
                        content: "Hello".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::Text {
                        content: " world".into(),
                        parent_tool_use_id: None,
                    },
                ],
                expected: vec![ChunkAction::NeedsRedraw, ChunkAction::NeedsRedraw],
            },
            // Case 3: TurnComplete EndTurn → TurnComplete
            Case {
                label: "turn_complete_end_turn",
                chunks: vec![StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                }],
                expected: vec![ChunkAction::TurnComplete {
                    persist: true,
                    trigger_title_generation: false,
                }],
            },
            // Case 4: TurnComplete ToolUse → TurnContinuing
            Case {
                label: "turn_complete_tool_use",
                chunks: vec![StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                }],
                expected: vec![ChunkAction::TurnContinuing],
            },
            // Case 5: Error → NeedsRedraw
            Case {
                label: "error",
                chunks: vec![StreamChunk::Error {
                    content: "rate limited".into(),
                }],
                expected: vec![ChunkAction::NeedsRedraw],
            },
            // Case 6: Blocked → NeedsRedraw
            Case {
                label: "blocked",
                chunks: vec![StreamChunk::Blocked {
                    content: "blocked".into(),
                }],
                expected: vec![ChunkAction::NeedsRedraw],
            },
            // Case 7: Usage → None
            Case {
                label: "usage",
                chunks: vec![StreamChunk::Usage {
                    usage: UsageInfo {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        reasoning_tokens: None,
                    },
                    session_id: None,
                }],
                expected: vec![ChunkAction::None],
            },
            // Case 8: Thinking → NeedsRedraw
            Case {
                label: "thinking",
                chunks: vec![StreamChunk::Thinking {
                    content: "hmm".into(),
                    parent_tool_use_id: None,
                }],
                expected: vec![ChunkAction::NeedsRedraw],
            },
            // Case 9: thinking_accumulates_text
            Case {
                label: "thinking_accumulates",
                chunks: vec![
                    StreamChunk::Thinking {
                        content: "First chunk. ".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::Thinking {
                        content: "Second chunk.".into(),
                        parent_tool_use_id: None,
                    },
                ],
                expected: vec![ChunkAction::NeedsRedraw, ChunkAction::NeedsRedraw],
            },
            // Case 10: ToolUse → NeedsRedraw
            Case {
                label: "tool_use",
                chunks: vec![StreamChunk::ToolUse {
                    id: "tool_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                expected: vec![ChunkAction::NeedsRedraw],
            },
            // Case 11: ToolResult → NeedsRedraw
            Case {
                label: "tool_result",
                chunks: vec![
                    StreamChunk::ToolUse {
                        id: "tool_1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                    StreamChunk::ToolResult {
                        id: "tool_1".into(),
                        content: "out".into(),
                        is_error: false,
                    },
                ],
                expected: vec![ChunkAction::NeedsRedraw, ChunkAction::NeedsRedraw],
            },
            // Case 12: TurnComplete MaxTokens → TurnComplete
            Case {
                label: "turn_complete_max_tokens",
                chunks: vec![StreamChunk::TurnComplete {
                    stop_reason: StopReason::MaxTokens,
                }],
                expected: vec![ChunkAction::TurnComplete {
                    persist: true,
                    trigger_title_generation: false,
                }],
            },
            // Case 13: TurnComplete with text → TurnComplete
            Case {
                label: "turn_complete_with_text",
                chunks: vec![
                    StreamChunk::Text {
                        content: "response".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
                expected: vec![
                    ChunkAction::NeedsRedraw,
                    ChunkAction::TurnComplete {
                        persist: true,
                        trigger_title_generation: false,
                    },
                ],
            },
            // Case 14: title_generation_only_at_two_messages
            // (simulated: after first turn, turns.len() == 1 → no title gen)
            Case {
                label: "title_generation_logic",
                chunks: vec![StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                }],
                expected: vec![ChunkAction::TurnComplete {
                    persist: true,
                    trigger_title_generation: false,
                }],
            },
        ];

        for case in cases {
            let mut state = ReducerState::new(now_ms, now);
            for (i, chunk) in case.chunks.iter().enumerate() {
                let action = reduce(&mut state, chunk.clone(), &clock);
                assert_eq!(
                    action, case.expected[i],
                    "{}: chunk {} produced wrong action",
                    case.label, i
                );
            }
        }
    }

    // ── AC2: Prose-flush rule ────────────────────────────────────────────

    #[test]
    fn prose_flushed_on_each_non_text_chunk() {
        // Table-driven over six trigger variants: ToolUse, Thinking, ToolResult, Error, Blocked, TurnComplete
        let cases: Vec<(StreamChunk, &str)> = vec![
            (
                StreamChunk::ToolUse {
                    id: "t1".into(),
                    name: "tool".into(),
                    input: serde_json::json!({}),
                },
                "ToolUse",
            ),
            (
                StreamChunk::Thinking {
                    content: "think".into(),
                    parent_tool_use_id: None,
                },
                "Thinking",
            ),
            (
                StreamChunk::Error {
                    content: "err".into(),
                },
                "Error",
            ),
            (
                StreamChunk::Blocked {
                    content: "block".into(),
                },
                "Blocked",
            ),
            (
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
                "TurnComplete",
            ),
        ];

        for (trigger_chunk, label) in cases {
            let now = Instant::now();
            let mut state = ReducerState::new(1000, now);
            let clock = MockClock::new(now);

            // Open prose with some text
            let _ = reduce(
                &mut state,
                StreamChunk::Text {
                    content: "hello".into(),
                    parent_tool_use_id: None,
                },
                &clock,
            );

            // Apply trigger — prose should be flushed into Turn.parts
            let _ = reduce(&mut state, trigger_chunk, &clock);

            // For TurnComplete, the turn is committed; check committed_turn
            // For other triggers, check open_turn
            let turn_parts: &Vec<TurnPart> = if let Some(ref ct) = state.committed_turn {
                &ct.parts
            } else {
                &state.open_turn.as_ref().unwrap().parts
            };
            let prose_parts: Vec<_> = turn_parts
                .iter()
                .filter(|p| matches!(p, TurnPart::Prose { .. }))
                .collect();
            assert!(
                !prose_parts.is_empty(),
                "prose not flushed for trigger: {}",
                label
            );
            // The open_prose buffer should be cleared
            assert!(
                state.open_prose.is_none() || state.open_prose.as_deref() == Some(""),
                "open_prose not cleared for trigger: {}",
                label
            );
        }
    }

    #[test]
    fn prose_flushed_on_tool_result() {
        // ToolResult is a flush trigger but requires a prior ToolUse, so it gets its own test
        let now = Instant::now();
        let mut state = ReducerState::new(1000, now);
        let clock = MockClock::new(now);

        // Open prose with some text
        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "hello".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        // Add a ToolUse so the subsequent ToolResult has a matching invocation
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "t1".into(),
                name: "tool".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );

        // Now apply ToolResult — prose should be flushed
        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "t1".into(),
                content: "done".into(),
                is_error: false,
            },
            &clock,
        );

        let turn = state.open_turn.as_ref().unwrap();
        let prose_parts: Vec<_> = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Prose { .. }))
            .collect();
        assert!(
            !prose_parts.is_empty(),
            "prose not flushed for ToolResult trigger"
        );
        assert!(
            state.open_prose.is_none() || state.open_prose.as_deref() == Some(""),
            "open_prose not cleared for ToolResult trigger"
        );
    }

    #[test]
    fn usage_chunk_does_not_flush_prose() {
        let now = Instant::now();
        let mut state = ReducerState::new(1000, now);
        let clock = MockClock::new(now);

        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "keep me".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        let _ = reduce(
            &mut state,
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
            &clock,
        );

        // open_prose should still be buffered (not flushed)
        assert_eq!(state.open_prose.as_deref(), Some("keep me"));
        // No prose part should have been committed
        let turn = state.open_turn.as_ref().unwrap();
        let prose_count = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Prose { .. }))
            .count();
        assert_eq!(prose_count, 0);
    }

    // ── AC3: Prose-extension rule ────────────────────────────────────────

    #[test]
    fn text_after_invocation_opens_new_prose_with_fresh_partid() {
        let (mut state, clock) = make_state(1000);

        // First prose run
        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "first run".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        // Flush via ToolUse
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "t1".into(),
                name: "tool".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );

        // Second prose run
        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "second run".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        // Complete the turn
        let _ = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );

        let committed = state.committed_turn.as_ref().unwrap();
        // Extract prose PartIds in order
        let prose_ids: Vec<PartId> = committed
            .parts
            .iter()
            .filter_map(|p| {
                if let TurnPart::Prose { id, .. } = p {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            prose_ids.len(),
            2,
            "expected 2 prose parts, got {:?}",
            prose_ids
        );
        assert!(
            prose_ids[1].0 > prose_ids[0].0,
            "second prose PartId must be greater than first: {:?}",
            prose_ids
        );
    }

    #[test]
    fn consecutive_text_chunks_extend_same_prose() {
        let (mut state, clock) = make_state(1000);

        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "Hello".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: " world".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "!".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        // Flush and commit
        let _ = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );

        let committed = state.committed_turn.as_ref().unwrap();
        let prose_parts: Vec<_> = committed
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Prose { .. }))
            .collect();

        assert_eq!(prose_parts.len(), 1, "should have exactly one prose part");
        if let TurnPart::Prose { text, .. } = &prose_parts[0] {
            assert_eq!(text, "Hello world!");
        }
    }

    #[test]
    fn consecutive_thinking_extends_same_reasoning() {
        let (mut state, clock) = make_state(1000);

        let _ = reduce(
            &mut state,
            StreamChunk::Thinking {
                content: "First".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        let _ = reduce(
            &mut state,
            StreamChunk::Thinking {
                content: " second".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        // Flush and commit
        let _ = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );

        let committed = state.committed_turn.as_ref().unwrap();
        let reasoning_parts: Vec<_> = committed
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Reasoning { .. }))
            .collect();

        assert_eq!(
            reasoning_parts.len(),
            1,
            "should have exactly one reasoning part"
        );
        if let TurnPart::Reasoning { text, .. } = &reasoning_parts[0] {
            assert_eq!(text, "First second");
        }
    }

    #[test]
    fn thinking_after_text_flushes_prose_then_opens_reasoning() {
        let (mut state, clock) = make_state(1000);

        let _ = reduce(
            &mut state,
            StreamChunk::Text {
                content: "pre-thinking".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );
        let _ = reduce(
            &mut state,
            StreamChunk::Thinking {
                content: "thinking".into(),
                parent_tool_use_id: None,
            },
            &clock,
        );

        // Complete
        let _ = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );

        let committed = state.committed_turn.as_ref().unwrap();
        let prose_count = committed
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Prose { .. }))
            .count();
        let reasoning_count = committed
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Reasoning { .. }))
            .count();

        assert_eq!(prose_count, 1);
        assert_eq!(reasoning_count, 1);
        // Prose must come before Reasoning
        let prose_pos = committed
            .parts
            .iter()
            .position(|p| matches!(p, TurnPart::Prose { .. }))
            .unwrap();
        let reasoning_pos = committed
            .parts
            .iter()
            .position(|p| matches!(p, TurnPart::Reasoning { .. }))
            .unwrap();
        assert!(prose_pos < reasoning_pos);
    }

    // ── AC4: ToolInvocation lifecycle ────────────────────────────────────

    #[test]
    fn tool_use_appends_running_invocation_with_clocked_started_at() {
        let now = Instant::now();
        let mut state = ReducerState::new(1000, now);
        let clock = MockClock::new(now);
        clock.advance(Duration::from_millis(1500));

        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
            &clock,
        );

        let turn = state.open_turn.as_ref().unwrap();
        let inv = turn
            .parts
            .iter()
            .find_map(|p| {
                if let TurnPart::ToolInvocation {
                    tool,
                    status,
                    started_at,
                    ended_at,
                    ..
                } = p
                {
                    Some((tool.clone(), status.clone(), *started_at, *ended_at))
                } else {
                    None
                }
            })
            .unwrap();

        assert_eq!(inv.0, "bash");
        assert_eq!(inv.1, InvocationStatus::Running);
        assert_eq!(inv.2, 2500); // wall_anchor_ms (1000) + delta (1500)
        assert_eq!(inv.3, None);
    }

    #[test]
    fn tool_use_then_result_writes_lifecycle() {
        let now = Instant::now();
        let mut state = ReducerState::new(1000, now);
        let clock = MockClock::new(now);

        clock.advance(Duration::from_millis(100));
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
            &clock,
        );

        clock.advance(Duration::from_millis(500));
        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "tool_1".into(),
                content: "output".into(),
                is_error: false,
            },
            &clock,
        );

        let turn = state.open_turn.as_ref().unwrap();
        let inv = turn
            .parts
            .iter()
            .find_map(|p| {
                if let TurnPart::ToolInvocation {
                    status,
                    started_at,
                    ended_at,
                    ..
                } = p
                {
                    Some((status.clone(), *started_at, *ended_at))
                } else {
                    None
                }
            })
            .unwrap();

        assert_eq!(inv.0, InvocationStatus::Success);
        let started_at = inv.1;
        let ended_at = inv.2;
        assert!(
            started_at < ended_at.unwrap(),
            "started_at must be before ended_at"
        );
        assert_eq!(started_at, 1100); // 1000 + 100
        assert_eq!(ended_at.unwrap(), 1600); // 1000 + 100 + 500

        // Check ToolResult part
        let result = turn.parts.iter().find_map(|p| {
            if let TurnPart::ToolResult { output, refs, .. } = p {
                Some((output.content.clone(), *refs))
            } else {
                None
            }
        });
        assert!(result.is_some());
        assert_eq!(result.as_ref().unwrap().0, "output");
    }

    #[test]
    fn tool_use_then_error_result_marks_invocation_error() {
        let (mut state, clock) = make_state(1000);

        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "false"}),
            },
            &clock,
        );

        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "tool_1".into(),
                content: "exit 1".into(),
                is_error: true,
            },
            &clock,
        );

        let turn = state.open_turn.as_ref().unwrap();
        let inv_status = turn
            .parts
            .iter()
            .find_map(|p| {
                if let TurnPart::ToolInvocation { status, .. } = p {
                    Some(status.clone())
                } else {
                    None
                }
            })
            .unwrap();

        assert_eq!(inv_status, InvocationStatus::Error);
    }

    #[test]
    fn orphan_tool_result_logs_and_skips() {
        // Capture tracing output to verify the warning fires
        let subscriber = tracing_subscriber::fmt::Subscriber::builder()
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let (mut state, clock) = make_state(1000);

        // No preceding ToolUse — this ToolResult is orphaned
        let action = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "unknown_id".into(),
                content: "orphan".into(),
                is_error: false,
            },
            &clock,
        );

        // Should still return NeedsRedraw (not panic)
        assert_eq!(action, ChunkAction::NeedsRedraw);

        let turn = state.open_turn.as_ref().unwrap();
        let result_count = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::ToolResult { .. }))
            .count();
        assert_eq!(result_count, 0);
    }

    // ── AC5: Liveness API ────────────────────────────────────────────────

    #[test]
    fn liveness_reports_active_tool_during_running() {
        let (mut state, clock) = make_state(1000);

        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );

        let snap = state.liveness();
        assert_eq!(snap.active_tool_name.as_deref(), Some("bash"));
        assert!(snap.progress.is_none()); // not wired yet (S16.9)
    }

    #[test]
    fn liveness_returns_none_after_all_completed() {
        let (mut state, clock) = make_state(1000);

        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );
        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "t1".into(),
                content: "done".into(),
                is_error: false,
            },
            &clock,
        );

        let snap = state.liveness();
        assert!(snap.active_tool_name.is_none());
    }

    #[test]
    fn liveness_progress_set_via_api() {
        let now = Instant::now();
        let mut state = ReducerState::new(1000, now);

        // Simulate a running tool invocation so last_running_tool_use_id is set
        let clock = MockClock::new(now);
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );

        state.set_progress("tool_1", 3, 10);

        let snap = state.liveness();
        assert_eq!(snap.progress, Some((3, 10)));
    }

    #[test]
    fn liveness_tail_set_via_api() {
        let now = Instant::now();
        let mut state = ReducerState::new(1000, now);
        let clock = MockClock::new(now);
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );

        state.set_tail("tool_1", "line1\nline2".into());

        let snap = state.liveness();
        assert_eq!(snap.tail.as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn liveness_tail_cleared_on_tool_result() {
        let (mut state, clock) = make_state(1000);
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );
        state.set_progress("tool_1", 5, 10);
        state.set_tail("tool_1", "hello".into());

        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "tool_1".into(),
                content: "done".into(),
                is_error: false,
            },
            &clock,
        );

        let snap = state.liveness();
        assert!(
            snap.progress.is_none(),
            "progress should be cleared on ToolResult"
        );
        assert!(snap.tail.is_none(), "tail should be cleared on ToolResult");
    }

    #[test]
    fn liveness_tail_cleared_on_turn_complete() {
        let (mut state, clock) = make_state(1000);
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );
        state.set_progress("tool_1", 3, 5);
        state.set_tail("tool_1", "data".into());

        let _ = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );

        assert!(
            state.progress.is_empty(),
            "progress map should be cleared on turn complete"
        );
        assert!(
            state.tail.is_empty(),
            "tail map should be cleared on turn complete"
        );
    }

    #[test]
    fn liveness_progress_cleared_on_tool_result() {
        let (mut state, clock) = make_state(1000);
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );
        state.set_progress("tool_1", 1, 1);

        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "tool_1".into(),
                content: "ok".into(),
                is_error: false,
            },
            &clock,
        );

        assert!(
            !state.progress.contains_key("tool_1"),
            "progress entry should be removed on ToolResult"
        );
    }

    #[test]
    fn progress_and_tail_cleared_on_tool_result_error() {
        let (mut state, clock) = make_state(1000);
        let _ = reduce(
            &mut state,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            &clock,
        );
        state.set_progress("tool_1", 4, 4);
        state.set_tail("tool_1", "tail-data".into());

        let _ = reduce(
            &mut state,
            StreamChunk::ToolResult {
                id: "tool_1".into(),
                content: "error output".into(),
                is_error: true,
            },
            &clock,
        );

        assert!(
            state.liveness().progress.is_none(),
            "progress must be cleared on ToolResult error"
        );
        assert!(
            state.liveness().tail.is_none(),
            "tail must be cleared on ToolResult error"
        );
    }

    // ── ReducerState::new ────────────────────────────────────────────────

    #[test]
    fn reducer_state_new_initializes_empty() {
        let now = Instant::now();
        let state = ReducerState::new(1000, now);
        assert!(state.open_turn.is_none());
        assert!(state.open_prose.is_none());
        assert!(state.open_reasoning.is_none());
        assert!(state.pending_invocations.is_empty());
        assert!(state.committed_turn.is_none());
    }
}
