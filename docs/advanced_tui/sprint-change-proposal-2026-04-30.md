# Sprint Change Proposal: Plan Approval Modal Overlay

**Date:** 2026-04-30  
**Project:** rustain  
**Trigger:** Systematic UX architecture issue — plan cards disappear from chat scrollback seconds after creation due to `rebuild_messages_mirror()` discarding dangling assistant messages.  
**Scope Classification:** Major (Fundamental replan with PM/Architect involvement)  
**Proposed By:** Bob (Scrum Master), synthesizing roundtable input from Sally (UX), Winston (Architect), John (PM)  

---

## Section 1: Issue Summary

### Problem Statement
When the AI proposes a plan via the `propose_plan` tool, a PlanCard is rendered inline in the chat scrollback. However, the card **disappears seconds after creation** because:

1. `PlanProposed` fires during streaming and creates a `ChatMessage` with a **random nanoid**
2. When the assistant turn commits, `rebuild_messages_mirror()` drains all assistant messages and rebuilds them **only from committed turns**
3. The plan card message's random ID doesn't match any turn ID, so `old_blocks.remove(&turn.id.0)` returns `None`
4. The `ContentBlockType::PlanCard` block is silently lost

A **tactical fix** was applied (using the current turn's ID for plan card messages so they survive rebuild), but this is fighting the S16.4 architecture migration. The systematic issue is deeper: **chat scrollback is for history, not pending decisions.**

### Evidence
- `tests_tui/test_story_6_1a_plan_card.py::test_yolo_auto_approve_plan` — FAILED (plan invisible in scrollback)
- `tests_tui/test_story_6_1a_plan_card.py::test_snapshot_plan_card_60x16` — FAILED (PlanCard header absent)
- Code path: `event_loop.rs:4032` → `conversation.rs:135-269` (rebuild destroys unmatched assistant messages)

### Root Cause Classification
This is a **pattern-architecture mismatch**, not a rendering bug. Pending approvals are actionable decision points that should not live in a scrollable log.

---

## Section 2: Impact Analysis

### Epic Impact

| Epic | Impact | Severity |
|------|--------|----------|
| **Epic 16** (Readable Agent Transcripts) | **Direct.** Moving blocking overlays out of scrollback aligns with S16.4 (render flip) and unblocks S16.10-cleanup (delete messages mirror). | High |
| **Epic 6** (Plan Mode) | **Indirect.** Story 6.1a (Plan Card) and 6.0d (Plan Mode) need forward-pointer notes. No reopening required — tactical fix holds. | Low |
| **Epic 5** (Permissions) | **Future opportunity.** Modal infrastructure (Story 16.4.6) enables permission prompt migration later, but explicitly out of scope for now. | Low |

### Story Impact

| Story | Change Required |
|-------|----------------|
| **16.4** (Render flip) | Scope **reduced** — no PlanCard dispatch arm needed in `render_expanded_turn` if PlanCard extraction lands first |
| **16.4.5** (Heuristic labeler) | Unchanged |
| **16.4.6** (NEW — Modal Foundation) | **Create.** Builds bottom-pane overlay primitive |
| **16.4.7** (NEW — PlanCard Migration) | **Create.** Migrates pending PlanCards from inline to modal |
| **16.5** (Virtual-scroll height cache) | Dependency update: now waits on 16.4.7. Cache simplifies — no pending PlanCard height measurement needed |
| **16.10-cleanup** (Delete messages mirror) | **Unblocked** by 16.4.7. `rebuild_messages_mirror()` no longer needed for PlanCard preservation |
| **6.1a** (Plan Card inline) | Add forward-pointer: "Pending PlanCard migrates to modal in 16.4.7. This story implements domain model and resolved-plan inline rendering" |
| **6.0d** (Plan Mode) | Add forward-pointer: "Plan approval overlay migrates to bottom-pane modal in 16.4.7" |

### Artifact Conflicts

| Artifact | Conflict | Resolution |
|----------|----------|------------|
| `docs/ARCHITECTURE_REVIEW_PLAN_MODE_ORCHESTRATION.md` | Proposes centered `plan_approval.rs` overlay | Update to reference bottom-pane modal approach (16.4.6/16.4.7) |
| `src/infrastructure/runtime/event_loop.rs` | PlanProposed handler pushes `PlanCard` into `messages` | Migrate to `bottom_overlay` state population |
| `src/adapters/tui/widgets/chat_pane/mod.rs` | Inline PlanCard rendering in two places (height + render) | Remove pending PlanCard paths; keep resolved-plan inline |
| `tests_tui/test_story_6_1a_plan_card.py` | Asserts inline PlanCard visibility | Rewrite to assert bottom-pane modal presence |

### Technical Impact

- **New modules:** `plan_approval_overlay.rs`, `overlay_manager.rs`
- **Modified modules:** `event_loop.rs`, `chat_pane/mod.rs`, `app.rs`, `state.rs`, `layout.rs`
- **Deleted paths:** Centered `plan_approval.rs` overlay (replaced by bottom-pane), inline pending PlanCard rendering
- **Data model:** `ContentBlockType::PlanCard` removed for pending plans; preserved for resolved plans (or removed entirely if rendered from `conversation.plans`)

---

## Section 3: Recommended Approach

### Chosen Path: Direct Adjustment Within Epic 16

Do **not** create a separate epic. The modal overlay is a transcript readability fix — same class as S16.4's interleaved parts and turn-based folding.

**Two new stories inserted between 16.4.5 and 16.5:**

```
16.1 → 16.2 → 16.3 → 16.4 → 16.4.5 → 16.4.6 → 16.4.7 → 16.5 → ... → 16.10-cleanup
```

### Rationale

| Alternative | Why Rejected |
|-------------|-------------|
| **Separate "Approval UX" Epic** | Fragments TUI refactor narrative; forces cross-epic dependencies at the worst time |
| **Keep inline (Winston Option B)** | Fixes the bug structurally but leaves the UX problem: users scrolled up miss pending plans |
| **Full view-stack (Codex-style)** | Over-engineered for current needs. Rustain has no NavigationStack foundation; 1-deep overlay + queue is sufficient |

### Effort Estimate

| Component | Estimate | Risk |
|-----------|----------|------|
| 16.4.6 Modal Foundation | 3 dev-days (M) | Medium — new render layer, focus capture |
| 16.4.7 PlanCard Migration | 2-3 dev-days (S-M) | Low — mostly wiring existing state |
| Test migration + snapshots | 2 dev-days | Medium — TUI snapshot tests sensitive to geometry |
| **Total** | **7-8 dev-days (~1.5 sprints)** | |

### Timeline Impact

- **Sprint N:** 16.4.6 + 16.4.7 land in parallel with S16.4 critical-fix pass
- **Sprint N+1:** 16.5 proceeds with simplified height cache (no pending PlanCards)
- **S16.10-cleanup:** Unblocked — messages mirror deletion proceeds

### Runway

The tactical fix (turn-ID preservation) buys **2-3 sprints** of runway. Modal work must ship by Sprint N+2 or the tactical fix becomes fragile under aggressive scroll/resize.

---

## Section 4: Detailed Change Proposals

### Story 16.4.6 — Bottom-Pane Modal Overlay Foundation

**Goal:** Build the view-stack primitive that rustain currently lacks.

**Acceptance Criteria:**

1. **Given** the TUI render loop, **when** a modal is requested, **then** it renders in a bottom-pane overlay anchored to the terminal bottom, consuming ~40% of height (min 6 lines, max 50% of viewport).
2. **Given** the overlay is open, **when** the user presses `Esc`, **then** it dismisses with configurable cancel action.
3. **Given** the overlay is open, **when** keyboard input arrives, **then** events route to the overlay, not to chat/input beneath it (focus capture).
4. **Given** multiple overlay requests arrive simultaneously, **then** a queue holds at most 1 pending overlay; excess requests emit a domain event for graceful degradation.
5. **Given** the render pipeline, **when** an overlay is active, **then** it renders **after** all other widgets (highest z-index), with subtle background dimming.
6. **Given** terminal resize, **when** dimensions change, **then** the overlay re-anchors correctly without flicker.
7. **Given** snapshot tests, **then** overlay states are locked at 80×24 and 120×40.

**Scope Boundaries (What's OUT):**

| Out of Scope | Rationale |
|-------------|-----------|
| Tool permission prompt migration | Epic 5 territory. 16.4.6's generic overlay makes this a trivial follow-up |
| AskUserQuestion inline cards | Part of turn flow by design, not blocking decisions |
| Full NavigationStack with push/pop history | 1-deep stack + queue sufficient for v1 |
| Animations / transitions | Snap open/closed only |

**New Modules:**

```
adapters/tui/widgets/plan_approval_overlay.rs   # bottom-pane widget
adapters/tui/widgets/overlay_manager.rs         # stateless key dispatch router
```

**State Addition:**

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
    // Future: Permission, SkillTrust, etc.
}
```

---

### Story 16.4.7 — Plan Approval Modal Migration

**Goal:** Migrate pending plan approvals out of inline transcript and into the bottom-pane modal.

**Acceptance Criteria:**

1. **Given** a pending plan (PlanCard or PlanApproval), **when** it renders, **then** it appears in the bottom-pane modal overlay (16.4.6), **not** inline in chat scrollback.
2. **Given** a plan has been resolved (approved/rejected/completed/cancelled), **when** it appears in chat history, **then** it renders as read-only inline card (double border, status footer) using turn-based render pipeline from 16.4.
3. **Given** YOLO mode, **when** a plan is generated, **then** no modal appears; instead a toast notification shows `ℹ Plan auto-approved: [title] — N tasks` (3s hold).
4. **Given** a pending plan modal is open, **when** time passes, **then** the modal never auto-dismisses — waits indefinitely for user response.
5. **Given** the modal is open, **when** user presses `[y]`, `[e]`, `[n]`, or `[a]`, **then** corresponding action executes and modal closes immediately.
6. **Given** the modal is open, **when** user presses `Esc`, **then** modal closes with equivalent of `[n] Reject`.
7. **Given** the modal is open, **then** chat pane remains visible above but is inactive (keyboard input does not scroll or interact with chat).
8. **Given** plan deviation reapproval (Story 6.4), **when** it triggers, **then** it uses the same bottom-pane modal overlay.
9. **Given** `tests_tui` snapshots, **then** these states are locked: pending modal, resolved inline history, YOLO toast, deviation reapproval modal.

**Migration Steps:**

| Step | Action | Files |
|------|--------|-------|
| 1 | Stop producing `ContentBlockType::PlanCard` for pending plans | `event_loop.rs`, `chat_pane/mod.rs` |
| 2 | Wire `PlanProposed` → `bottom_overlay` population | `event_loop.rs` |
| 3 | Migrate `y/a/n/e` key routing from inline to modal | `app.rs`, `event_loop.rs` |
| 4 | Render resolved plans inline from `conversation.plans` (not `content_blocks`) | `chat_pane/mod.rs` |
| 5 | Delete centered `plan_approval.rs` overlay; replace with bottom-pane | `plan_approval.rs` |
| 6 | Clean up `pending_plan_card` param from `chat_pane::render_with_search` | `chat_pane/mod.rs`, tests |

**YOLO Mode Behavior:**

```rust
// event_loop.rs PlanProposed handler
if security.current_mode() == PermissionMode::Yolo {
    // Skip modal; emit toast only
    domain_tx.send(AppEvent::StatusFlash {
        level: NoticeLevel::Info,
        message: format!("Plan auto-approved: {} — {} tasks", plan_title, task_count),
        duration_seconds: 3,
    });
    domain_tx.send(AppEvent::PlanExecutionStarted { ... });
} else {
    // Populate modal overlay
    state.bottom_overlay = Some(BottomOverlayState {
        kind: BottomOverlayKind::PlanCard { pending: PendingPlanCard { ... } },
        prior_focus: state.focus,
    });
    state.focus = FocusState::Overlay(OverlayKind::BottomPane);
}
```

---

### Architecture Updates

**Rendering Integration:**

No z-index or layer system needed. Add `reserve_bottom_overlay()` to `AppLayout` (mirrors `reserve_search_bar()` and `reserve_bookmark_panel()`). Overlay renders **last** in frame closure with `Clear`, overwriting input area.

```rust
// app.rs render closure
let overlay_area = app_layout.reserve_bottom_overlay(height);
if let Some(ref overlay) = state.bottom_overlay {
    frame.render_widget(Clear, overlay_area);
    overlay_manager::render(overlay, frame, overlay_area, theme);
}
```

**Focus Management:**

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

**Event Loop Changes:**

```rust
// PlanProposed handler — NEW behavior
AppEvent::PlanProposed { conversation_id, plan } => {
    // ... store plan in conversation.plans ...
    
    if security.current_mode() == PermissionMode::Yolo {
        // Auto-approve + toast
    } else {
        // Populate modal overlay instead of messages.content_blocks
        state.bottom_overlay = Some(BottomOverlayState {
            kind: BottomOverlayKind::PlanCard { 
                pending: PendingPlanCard { conversation_id, plan_id, plan_snapshot: plan }
            },
            prior_focus: state.focus,
        });
        state.focus = FocusState::Overlay(OverlayKind::BottomPane);
    }
    state.needs_redraw = true;
}

// PlanCardResolved handler — NEW behavior
AppEvent::PlanCardResolved { decision } => {
    // ... update plan status ...
    state.bottom_overlay = None;
    state.focus = state.bottom_overlay.as_ref()
        .map(|o| o.prior_focus)
        .unwrap_or(FocusState::Input);
    state.needs_redraw = true;
}
```

---

## Section 5: Implementation Handoff

### Scope Classification: **Major**

This is a fundamental replan requiring PM/Architect involvement. The change:
- Introduces a new TUI primitive (bottom-pane overlay)
- Migrates two existing approval flows (PlanCard + PlanApproval)
- Unblocks S16.10-cleanup (messages mirror deletion)
- Modifies Epic 16 roadmap with two new stories

### Handoff Recipients

| Recipient | Responsibility |
|-----------|---------------|
| **PM (John)** | Update Epic 16 roadmap; create Story 16.4.6 and 16.4.7 in backlog; add forward-pointers to Story 6.1a and 6.0d |
| **Architect (Winston)** | Review `AppLayout` changes; validate overlay focus capture doesn't break existing `FocusState` consumers; approve `BottomOverlayState` schema |
| **Developer (Amelia)** | Implement 16.4.6 (Modal Foundation) → 16.4.7 (PlanCard Migration). Coordinate MO-3 merge with S16.4 to reduce S16.4 scope |
| **QA (Quinn)** | Rewrite `test_story_6_1a_plan_card.py` for modal assertions; snapshot tests at 80×24, 60×16, 120×40 |
| **Scrum Master (Bob)** | Sprint N: Run Track A (S16.4 critical-fix) + Track B (MO-1/MO-2) in parallel. Coordinate MO-3/S16.4 merge order |

### Success Criteria

1. **Plan Card is out of chat scrollback** — `ContentBlockType::PlanCard` no longer rendered inline for pending plans
2. **Plan Approval is bottom-pane** — `PendingPlanApproval` renders in bottom modal, not centered overlay
3. **Infrastructure is reusable** — `AppLayout::reserve_bottom_overlay`, `BottomOverlayState`, and `OverlayType::Modal` exist and documented
4. **No regressions** — `test_story_6_0d_plan_mode.py`, `test_story_6_1a_plan_card.py`, Epic 16 regression fixture pass
5. **Help overlay documents modal keys** — `?` shows `[y] approve`, `[n] reject`, `[e] edit`, `Esc dismiss` when modal active
6. **S16.10 unblocked** — `rebuild_messages_mirror()` can be deleted; no pending-plan consumers remain

### Dependency Coordination

```
MO-1 (16.4.6) ──► MO-2 (16.4.7)
    │                  │
    │                  └── Coordinate with S16.4 merge
    │                      (if MO-3 lands first → S16.4 drops PlanCard handling)
    │
    └── Parallel with S16.4 critical-fix pass (no overlap)
```

**Critical Merge Order:**
- Preferred: MO-3 (PlanCard extraction) lands **before** S16.4 merges → S16.4 scope shrinks
- Fallback: If S16.4 ready first, delay merge by 2-3 days for MO-3 extraction

---

## Appendix A: Peer Tool Comparison

| Tool | Plan Display | Approval UI | Auto-Approve | Timeout |
|------|-------------|-------------|--------------|---------|
| **Codex** | Inline stream | Bottom-pane modal (`ApprovalOverlay`) | `--dangerously-bypass-approvals` | None |
| **Claudian** | Inline DOM | Inline HTML injection | None | AbortSignal-driven |
| **Gemini CLI** | Plan Mode tools | ACP `requestPermission` RPC (IDE modal) | Policy engine (`YOLO`, `AUTO_EDIT`) | None |
| **Opencode** | File-based | Event-bus questions/permissions | Ruleset-based | None |
| **rustain (proposed)** | Modal (pending) + Inline (resolved) | Bottom-pane composer replacement | YOLO mode toast | None |

## Appendix B: Risk Matrix

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Modal foundation takes longer than expected | Medium | High | Cap at 1-deep stack + queue. MVP = MO-1 + MO-3 only |
| S16.4 merges before MO-3 | Medium | Medium | Coordinate merge order. If S16.4 first, implement PlanCard in new render path temporarily |
| Focus-state race with chat scroll | Low | Medium | Modal holds `FocusState::Overlay(BottomPane)`; scroll events route to chat only when `focus == Chat` |
| Terminal < 60×16 can't fit modal | Low | Low | Fallback to centered overlay below 60×16; delete fallback once validated |
| Test snapshot churn | Medium | Medium | Rewrite assertions to inspect `TuiState::bottom_overlay` instead of buffer-cell scanning |

---

*This Sprint Change Proposal was produced by the BMAD Correct Course workflow. Do not launch implementation until PM and Architect have approved the story structure and merge coordination plan.*
