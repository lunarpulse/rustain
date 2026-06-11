# Modal Overlay UX Brainstorm: From Inline Chaos to Unified Confirmation Surface

**Date:** 2026-04-30  
**Participants:** Sally (UX Designer), Winston (Architect), John (PM), Bob (Scrum Master)  
**Scope:** rustain TUI — bottom-pane modal overlay foundation and migration opportunities  

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [The Triggering Problem](#2-the-triggering-problem)
3. [Roundtable Decisions](#3-roundtable-decisions)
4. [Current State: A UI Inventory](#4-current-state-a-ui-inventory)
5. [The Modal Foundation (Story 16.4.6)](#5-the-modal-foundation-story-1646)
6. [Migration Backlog](#6-migration-backlog)
7. [Architectural Prerequisites](#7-architectural-prerequisites)
8. [Risk Register](#8-risk-register)
9. [Appendix: Peer Comparison](#9-appendix-peer-comparison)

---

## 1. Executive Summary

Rustain's TUI currently manages **15+ interactive overlay types** using **4 inconsistent rendering patterns**. The most severe symptom — plan cards disappearing from chat scrollback seconds after creation — exposed a deeper architectural issue: **chat scrollback is for history, not pending decisions**.

The team decided to build a **bottom-pane modal overlay foundation** (Story 16.4.6) and migrate plan approvals to it (Story 16.4.7). Once that foundation exists, it unlocks a backlog of **18 additional UX improvements** ranging from "fix invisible focus traps" to "add rich diff previews."

This document captures the full brainstorm: the problem, the decisions, the architecture, and the prioritized backlog.

---

## 2. The Triggering Problem

### 2.1 Symptom: Disappearing Plan Cards

When the AI proposes a plan via the `propose_plan` tool, a PlanCard renders inline in the chat scrollback — then vanishes seconds later.

**Test failures:**
- `test_story_6_1a_plan_card.py::test_yolo_auto_approve_plan` — plan invisible in scrollback
- `test_story_6_1a_plan_card.py::test_snapshot_plan_card_60x16` — PlanCard header absent

### 2.2 Root Cause

```
1. PlanProposed fires during streaming
   └── Creates ChatMessage with RANDOM nanoid ID

2. Assistant turn commits
   └── rebuild_messages_mirror() drains ALL assistant messages
   └── Rebuilds ONLY from conversation.turns
   └── Looks up content_blocks by turn ID: old_blocks.remove(&turn.id.0)
   └── Random ID ≠ turn ID → returns None → PlanCard LOST
```

**Code paths:**
- `src/infrastructure/runtime/event_loop.rs:4032` — PlanProposed handler
- `src/domain/models/conversation.rs:135-269` — rebuild_messages_mirror()

### 2.3 The Systematic Issue

The disappearing card is not a rendering bug. It is a **pattern-architecture mismatch**:

> **Chat scrollback is for history. Pending approvals are actionable decision points that demand attention.**

When a user is scrolled up reviewing prior conversation, a plan proposed at the bottom is invisible. When `rebuild_messages_mirror()` runs, it is structurally destroyed.

---

## 3. Roundtable Decisions

### 3.1 UX Decision (Sally)

**Move pending plan approval to a bottom-pane modal overlay. Keep resolved plans inline as read-only history.**

| State | Location | Rationale |
|-------|----------|-----------|
| **Pending** | Bottom-pane modal (replaces composer) | Always visible, commands attention, cannot scroll away |
| **Resolved** | Inline in chat scrollback | Historical record of what was approved/rejected |
| **YOLO mode** | Brief status-bar toast | User opted into auto-approve; no interruption |

**Keyboard contract:**
- `y` — Approve
- `a` — Approve & AutoEdit
- `n` — Reject
- `e` — Edit / Revise
- `Esc` — Dismiss (equivalent to Reject)
- **No timeout** — waits indefinitely

### 3.2 Architectural Decision (Winston)

**No view-stack needed.** Ratatui's imperative render model supports a simple "replace composer" approach. Two new modules:

```
adapters/tui/widgets/plan_approval_overlay.rs   # bottom-pane widget
adapters/tui/widgets/overlay_manager.rs         # key dispatch router
```

**State addition:**

```rust
// adapters/tui/state.rs
pub bottom_overlay: Option<BottomOverlayState>

pub struct BottomOverlayState {
    pub kind: BottomOverlayKind,
    pub prior_focus: FocusState,
}

pub enum BottomOverlayKind {
    PlanCard { pending: PendingPlanCard },
    PlanApproval { pending: PendingPlanApproval },
    // Extensible for future confirmation types
}
```

**Rendering:** Add `reserve_bottom_overlay()` to `AppLayout` (mirrors existing `reserve_search_bar()` and `reserve_bookmark_panel()`). Overlay renders **last** in the frame closure with `Clear`, overwriting the input area.

### 3.3 PM Decision (John)

**Keep the work in Epic 16.** Do not create a separate epic. The modal overlay is a transcript readability fix — same class as S16.4's interleaved parts and turn-based folding.

**New stories inserted between 16.4.5 and 16.5:**

```
16.1 → 16.2 → 16.3 → 16.4 → 16.4.5 → 16.4.6 → 16.4.7 → 16.5 → ... → 16.10-cleanup
```

| Story | Title | Size |
|-------|-------|------|
| **16.4.6** | Bottom-Pane Modal Overlay Foundation | M |
| **16.4.7** | Plan Approval Modal Migration | S-M |

### 3.4 Sprint Decision (Bob)

**1.5 sprints total** (~7-10 dev-days). Tactical fix buys 2-3 sprints of runway.

**Recommended sequencing:**
- **Week 1:** 16.4.6 + PlanApproval migration (parallel with S16.4 critical-fix pass)
- **Week 2:** PlanCard migration + tests (coordinate with S16.4 merge)

**Critical merge order:** PlanCard extraction (16.4.7) should land **before** S16.4 merges. This **shrinks S16.4 scope** — the new parts-aware render path won't need a PlanCard dispatch arm.

---

## 4. Current State: A UI Inventory

Rustain has **15+ overlay types** using **4 inconsistent rendering patterns**.

### 4.1 Pattern 1: Bottom-of-Chat Overlays (Ad-Hoc)

Manually compute `y = chat_pane.y + chat_pane.height - height` for each. No shared layout reservation.

| Element | Interaction |
|---------|-------------|
| Permission prompt | `y/s/a/n/f` |
| AskUserQuestion | Type → Enter |
| SkillTrust | `y/n/i` + Inspect mode |
| ForkConfirmation | `y/n` |
| RewindConfirmation | `y/f/n` |

### 4.2 Pattern 2: Centered Modals

Manual centered rect math. Each reimplements the same geometry.

| Element | Size |
|---------|------|
| PlanApproval | 80% × 70% |
| CommandPalette | 60% × 50% |
| HelpOverlay | 80% × 90% (full-screen) |

### 4.3 Pattern 3: Chat-Pane Replacement

Sets `skip_chat_render = true` and draws directly into the chat pane buffer. **Conversation history becomes invisible.**

| Element | Trigger |
|---------|---------|
| PlanDeviation | Plan changed mid-execution |
| CancelPlanConfirm | `!cancel-plan` command |
| TaskSkipCascade | Task failure with downstream deps |

### 4.4 Pattern 4: Status-Bar Only (Ghost Overlays)

Sets focus to `Overlay(Confirmation(...))` but has **zero render code**. User must guess what `y` will do.

| Element | Current Behavior |
|---------|-----------------|
| DeleteConfirmation | Status flash + invisible `y/n` |
| ExportOverwrite | Status flash + invisible `y/n` |

### 4.5 Pattern 5: Inline / Non-Blocking

Renders as part of the chat stream. Can scroll away while still interactive.

| Element | Problem |
|---------|---------|
| Inline PlanCard (pending) | **Disappears on rebuild** |
| FeedbackBlock (Error) | Buried in scrollback; user misses `[r] Retry` |
| ImageSizeWarning | Inline with `[y] Attach`; context lost if scrolled |

### 4.6 Pattern 6: Floating / Tier-2

Lightweight, dismissible by other interactions.

| Element | Dismiss |
|---------|---------|
| ReverseSearch | `Esc`, `Ctrl+P`, `Ctrl+X` |
| Autocomplete popup | `Esc`, `Tab` |
| WhichKey bar | Any key |
| ToolBlock peek | Any key |

---

## 5. The Modal Foundation (Story 16.4.6)

### 5.1 Design Goals

1. **Unify** all confirmation flows into a single visual pattern
2. **Preserve** chat context — never replace the entire chat pane
3. **Promote** critical decisions — never bury them in scrollback
4. **Extend** easily — new confirmation types add a variant, not a new render branch

### 5.2 Layout Integration

```rust
// app.rs render closure
let overlay_area = app_layout.reserve_bottom_overlay(height);
if let Some(ref overlay) = state.bottom_overlay {
    frame.render_widget(Clear, overlay_area);
    overlay_manager::render(overlay, frame, overlay_area, theme);
}
```

`reserve_bottom_overlay()` mirrors existing patterns:
- `reserve_search_bar()` — top of chat pane
- `reserve_bookmark_panel()` — bottom of chat pane, above input
- `reserve_bottom_overlay()` — replaces input area entirely

### 5.3 Focus Management

```rust
// Input routing in event_loop.rs
match state.focus {
    FocusState::Overlay(OverlayKind::BottomPane) => {
        if let Some(ref mut overlay) = state.bottom_overlay {
            overlay_manager::handle_key(overlay, key, &domain_tx);
            return; // Do NOT route to chat/input
        }
    }
    _ => { /* normal routing */ }
}
```

### 5.4 Event Loop Integration

```rust
// PlanProposed handler — NEW behavior
AppEvent::PlanProposed { conversation_id, plan } => {
    // ... store plan in conversation.plans ...
    
    if security.current_mode() == PermissionMode::Yolo {
        // Auto-approve + toast
        domain_tx.send(AppEvent::StatusFlash {
            level: NoticeLevel::Info,
            message: format!("Plan auto-approved: {} — {} tasks", plan_title, task_count),
            duration_seconds: 3,
        });
    } else {
        // Populate modal overlay
        state.bottom_overlay = Some(BottomOverlayState {
            kind: BottomOverlayKind::PlanCard { pending: PendingPlanCard { ... } },
            prior_focus: state.focus,
        });
        state.focus = FocusState::Overlay(OverlayKind::BottomPane);
    }
    state.needs_redraw = true;
}

// Resolution handler
AppEvent::PlanCardResolved { decision } => {
    // ... update plan status ...
    state.bottom_overlay = None;
    state.focus = state.bottom_overlay.as_ref()
        .map(|o| o.prior_focus)
        .unwrap_or(FocusState::Input);
    state.needs_redraw = true;
}
```

### 5.5 YOLO Mode Behavior

No modal. Brief status-bar toast:

```
ℹ Plan auto-approved: Refactor Auth Module — 5 tasks
```

3-second hold, then dismisses. No user interruption.

---

## 6. Migration Backlog

### 6.1 P1 — Must-Do (Ride with 16.4.6 / 16.4.7)

These fix **actively broken** UX or **destroy conversation context**.

| # | Item | Effort | Problem | Fix |
|---|------|--------|---------|-----|
| **1** | **DeleteConfirmation** modal | XS | **No visual card.** Status flash + invisible `y/n` focus trap. | 4-line bottom modal: "Delete conversation: *Title*?", `[y] Confirm  [Esc] Cancel` |
| **2** | **ExportOverwrite** modal | XS | **No visual card.** User guesses what `y` overwrites. | 4-line modal with filename + metadata |
| **3** | **PlanDeviation** → modal | S | **Replaces entire chat pane.** User loses all context when plan changes. | 8-row bottom modal with task diff summary, chat remains visible |

### 6.2 P2 — Should-Do (Next Sprint)

Batch these while the pattern is fresh.

| # | Item | Effort | Value |
|---|------|--------|-------|
| **4** | **CancelPlanConfirm** → modal | S | Same chat-pane replacement as PlanDeviation |
| **5** | **TaskSkipCascade** → modal | S | Blocks plan execution; high visibility when triggered |
| **6** | **Permission prompt** → unified modal | S | **Highest-frequency** confirmation. Biggest consistency win. Add risk badge: `🛡 Safe` / `⚠ Standard` / `🔴 Elevated` |
| **7** | **AskUserQuestion** → modal | S | Bottom-of-chat ad-hoc → intentional modal |
| **8** | **SkillTrust** → modal | S | Completes confirmation family unification |

### 6.3 P3 — Backlog (Future Sprints)

| # | Item | Effort | Note |
|---|------|--------|------|
| **9** | ForkConfirmation → modal | S | Works today; polish. Lower frequency. |
| **10** | RewindConfirmation → scrollable modal | S | Needs scrollable sub-area inside modal. |
| **11** | Error FeedbackBlock → sticky modal | M | Requires new extraction mechanism to promote errors out of scrollback. |
| **12** | ImageSizeWarning → modal | M | Bundle with #11 when feedback-block promotion pipeline exists. |
| **13** | Rewind + diff preview | M | Dead code (`render_diff_lines`) exists. Needs diff content pipeline. |
| **14** | Tool peek → persistent modal | M | Changes "any key dismiss" contract. |
| **15** | Permission + argument preview | S–M | **Nearly free if done during #6.** Log as AC for permission modal. |
| **16** | CommandPalette → bottom | M | Centered is standard convention. Low ROI. |
| **17** | HelpOverlay | — | **Keep full-screen.** Bottom-pane would be unreadable for reference content. |
| **18** | BookmarkList | S | Already uses proper `reserve_bookmark_panel`. Works well. |

---

## 7. Architectural Prerequisites

Before tackling P2/P3, the foundation needs these capabilities:

| Prerequisite | Needed For | Effort |
|-------------|-----------|--------|
| **Scrollable sub-areas inside bottom modal** | TaskSkipCascade, Rewind, PlanApproval, rich previews | S |
| **Dynamic height with clamping** | Rewind (file list), PlanApproval (scrollable content) | XS |
| **Risk badge rendering** | Permission prompt | XS |

### 7.1 Scrollable Sub-Areas

The foundation sketch (`reserve_bottom_overlay(height) -> Rect`) assumes fixed height. Rich previews and PlanApproval need **internal scrolling** or **dynamic height with clamping**.

**Approach:** Add a `ScrollableModal` helper that manages an internal `scroll_offset` and clamps content to the reserved height:

```rust
pub struct ScrollableModal<'a> {
    content: Vec<Line<'a>>,
    scroll_offset: usize,
    max_visible_rows: usize,
}

impl<'a> ScrollableModal<'a> {
    pub fn scroll_down(&mut self) { self.scroll_offset += 1; }
    pub fn scroll_up(&mut self) { self.scroll_offset = self.scroll_offset.saturating_sub(1); }
    pub fn visible_lines(&self) -> Vec<Line<'a>> { /* clamp */ }
}
```

### 7.2 Dynamic Height

For overlays with variable content (Rewind file list, PlanApproval content):

```rust
pub fn reserve_bottom_overlay_dynamic(
    &mut self,
    desired_height: u16,
    max_ratio: f32, // e.g., 0.4
) -> Rect {
    let max_height = (self.chat_pane.height as f32 * max_ratio) as u16;
    let height = desired_height.min(max_height).max(4); // min 4 rows
    // Reserve from bottom up
}
```

---

## 8. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Modal foundation takes longer than expected | Medium | High | Cap at 1-deep stack + queue. MVP = 16.4.6 + P1 items only. |
| S16.4 merges before PlanCard extraction | Medium | Medium | Coordinate merge order. If S16.4 first, temporarily implement PlanCard in new render path. |
| Focus-state race with chat scroll | Low | Medium | Modal holds `FocusState::Overlay(BottomPane)`. Scroll routes to chat only when `focus == Chat`. |
| Terminal < 60×16 can't fit modal | Low | Low | Fallback to centered overlay below 60×16. Delete fallback once validated. |
| Chat-replacing cards feel wrong as bottom modals | Medium | Medium | **Product decision required.** If intent is "hard interrupt," keep `skip_chat_render`. If intent is "preserve context," migrate to modal. |
| Test snapshot churn | Medium | Medium | Rewrite assertions to inspect `TuiState::bottom_overlay` instead of buffer-cell scanning. |

---

## 9. Appendix: Peer Comparison

How other TUI/CLI tools handle plan display and approval:

| Tool | Plan Display | Approval UI | Auto-Approve | Timeout |
|------|-------------|-------------|--------------|---------|
| **Codex** (OpenAI) | Inline stream | Bottom-pane modal (`ApprovalOverlay`) | `--dangerously-bypass-approvals` | None |
| **Claudian** (Obsidian) | Inline DOM | Inline HTML injection | None | AbortSignal-driven |
| **Gemini CLI** | Plan Mode tools | ACP `requestPermission` RPC (IDE modal) | Policy engine (`YOLO`, `AUTO_EDIT`) | None |
| **Opencode** | File-based | Event-bus questions/permissions | Ruleset-based | None |
| **rustain (proposed)** | Modal (pending) + Inline (resolved) | Bottom-pane composer replacement | YOLO mode toast | None |

**Key insight from peers:** None show pending approvals inline in chat scrollback. All separate decision surfaces from history.

---

*This document was produced following a BMAD roundtable brainstorm. The tactical fix for disappearing plan cards is in production. The modal foundation and migration backlog are documented for future implementation.*
