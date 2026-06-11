# Architecture: Plan Approval Overlay System

**Author:** Winston (🏗️ System Architect)  
**Date:** 2026-04-30  
**Scope:** Migrate pending plan approval from inline chat scrollback to a bottom-pane modal overlay (Sally's UX recommendation).  
**Status:** Draft — ready for dev handoff  

---

## 0. Context & Constraints

- **Existing overlay pattern:** `plan_approval.rs` (Story 6-0d) already renders a *centered* overlay for Plan mode exit approval. We are **NOT** replacing that.
- **Target:** `PendingPlanCard` (Story 6-1a) currently renders inline in `chat_pane` via `ContentBlockType::PlanCard`. We move it to a **bottom-pane overlay** that replaces the composer.
- **No view-stack exists.** Every overlay today is a boolean flag + ad-hoc focus branch.
- **Constraint:** Keep changes local to `adapters/tui/` and `event_loop.rs` input routing. Do not touch `domain/models/plan.rs` or `conversation.plans`.

---

## 1. Component Design

### 1.1 New Modules

```
 adapters/tui/
 ├── widgets/
 │   ├── plan_approval_overlay.rs      # NEW — bottom-pane overlay widget
 │   └── overlay_manager.rs            # NEW — focus & lifecycle coordinator
 │   ├── plan_approval.rs              # EXISTING — centered 6-0d overlay (unchanged)
 │   ├── plan_card.rs                  # EXISTING — inline card renderer (kept for history)
 │   └── ...
 ├── state.rs                          # MODIFIED — add OverlayKind + overlay_state field
 └── app.rs                            # MODIFIED — route overlay keys via manager
```

#### `widgets/plan_approval_overlay.rs`
Renders the pending plan in the **bottom pane** (where the input box normally sits). Unlike the centered `plan_approval.rs`, this is a full-width, fixed-height panel.

```rust
pub fn render(
    frame: &mut Frame,
    area: Rect,
    pending: &PendingPlanCard,
    theme: &Theme,
);

/// Compute height: min(12, max(6, terminal_height / 4))
pub fn overlay_height(terminal_height: u16) -> u16;
```

**Visual structure:**
```
┌─ Plan: "heroic-owl" — [y] Approve  [a] Approve & Auto-Edit  [n] Reject  [e] Edit ─┐
│  1. Add error handling to ingest.rs                                               │
│  2. Wire ApprovalRuntime into event_loop.rs                                         │
│  ...                                                                                │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

#### `widgets/overlay_manager.rs`
A **stateless router** (not a widget). It answers two questions:
1. *Is an overlay currently intercepting input?*
2. *Which `InputAction` should this key produce while an overlay is active?*

```rust
pub enum OverlayKind {
    None,
    PlanApproval,        // 6-0d centered overlay (existing)
    PlanCardBottomPane,  // NEW — 6-1a bottom-pane overlay
    Permission,
    SkillTrust,
    // ... future
}

impl OverlayKind {
    pub fn from_state(state: &TuiState) -> Self;
    pub fn to_focus_state(&self) -> Option<FocusState>;
    pub fn route_key(&self, c: char, state: &TuiState) -> InputAction;
    pub fn dismiss(&self, state: &mut TuiState) -> Option<FocusState>;
}
```

**Why a manager instead of a view stack?**  
Ratatui renders imperatively every frame. A "stack" would imply push/pop semantics that we don't need yet. The only overlays that can appear simultaneously are:
- Cross-search + sidebar (already handled)
- Bookmark panel + chat pane (already handled)
- **Plan card bottom pane** is **mutually exclusive** with input focus. No stacking required.

### 1.2 Integration with Existing `PendingPlanCard`

`PendingPlanCard` already carries:
- `conversation_id: String`
- `plan_id: String`
- `plan_snapshot: Plan`

**Reuse it.** The overlay widget receives `&PendingPlanCard` and renders `plan_snapshot` directly. No new domain struct needed.

### 1.3 Focus Interception System

Current state (ad-hoc):
```rust
// app.rs — scattered guards
if state.pending_plan_card.is_some() { ... }
if state.focus == Overlay(Confirmation(PlanApproval)) { ... }
```

**Replace with a single guard in `app.rs::handle_char_input`:**

```rust
let overlay = overlay_manager::OverlayKind::from_state(state);
if overlay != OverlayKind::None {
    return overlay.route_key(c, state);
}
```

This collapses ~12 scattered `if` blocks in `app.rs` into one dispatch table. **Risk mitigation:** do this in Step 2 (overlay build), not Step 1 (extraction), so we don't break existing key routing mid-migration.

---

## 2. State Management

### 2.1 Where Overlay State Lives

**Add to `TuiState` (`adapters/tui/state.rs`):**

```rust
/// Which bottom-pane overlay is active, if any.
/// Mutually exclusive with `input_buffer` focus.
pub bottom_overlay: Option<BottomOverlayState>,
```

```rust
pub struct BottomOverlayState {
    pub kind: BottomOverlayKind,
    /// Focus to restore when overlay is dismissed (typically Input).
    pub prior_focus: FocusState,
}

pub enum BottomOverlayKind {
    PlanCard { pending: PendingPlanCard },
    // Future: AskUserQuestionCompact, PermissionCompact, etc.
}
```

**Why not put this in `OverlayState` at the domain layer?**  
Bottom-pane overlays are a **TUI presentation concern**. The domain layer (`conversation.plans`) already knows the plan is `PlanStatus::Pending`. Whether we render that pending plan inline or in an overlay is an adapter decision. Keep it in `TuiState`.

### 2.2 How the Event Loop Knows an Overlay Is Active

Two signals, both in `TuiState`:

1. **`state.bottom_overlay.is_some()`** — drives rendering (replace composer with overlay).
2. **`state.focus == FocusState::Overlay(...)`** — drives input routing.

**Event loop invariant:**  
When `bottom_overlay` is `Some(PlanCard)`, focus MUST be `Overlay(PlanCardBottomPane)` (or a new dedicated variant). The handler that populates `bottom_overlay` (see §4.3) is responsible for setting both.

### 2.3 Keybinding Routing When Overlay Is Present

In `app.rs`, replace the existing `pending_plan_card` inline intercept:

```rust
// BEFORE (inline)
if state.pending_plan_card.is_some() {
    match c { 'y' => PlanCardApprove, 'n' => PlanCardReject, 'e' => PlanCardEdit, _ => {} }
}

// AFTER (overlay)
if let Some(ref overlay) = state.bottom_overlay {
    match overlay.kind {
        BottomOverlayKind::PlanCard { .. } => match c {
            'y' => InputAction::PlanCardApprove,
            'a' => InputAction::PlanCardApproveAutoEdit, // NEW action
            'n' => InputAction::PlanCardReject,
            'e' => InputAction::PlanCardEdit,
            _ => InputAction::Consumed,
        },
    }
}
```

**Esc handling:**  
`Esc` currently clears autocomplete, search, etc. Add a branch in `app.rs::handle_key_event`:

```rust
KeyCode::Esc => {
    if let Some(overlay) = state.bottom_overlay.take() {
        state.focus = overlay.prior_focus;
        state.needs_redraw = true;
        return InputAction::Consumed;
    }
    // ... existing Esc handlers
}
```

Dismissing the overlay does **not** resolve the plan — it just returns focus to the composer. The plan remains pending. This matches Sally's "Esc focus back" requirement.

---

## 3. Rendering Integration

### 3.1 "Replace Composer with Overlay" — Sufficient

We do **NOT** need a z-index or render layer system. Ratatui paints in declaration order. The bottom-pane overlay is rendered **last** in the frame closure, after the input area, with `Clear` to overwrite whatever was there.

**Layout changes (`layout.rs`):**

Add a new reservation method, mirroring `reserve_search_bar` and `reserve_bookmark_panel`:

```rust
impl AppLayout {
    /// Reserve the bottom N rows of `chat_pane` for a bottom overlay.
    /// The overlay replaces the input area, so this actually shrinks
    /// `input_area` to zero and shifts the overlay into that space.
    pub fn reserve_bottom_overlay(&mut self, height: u16) {
        if height == 0 || self.input_area.height == 0 {
            return;
        }
        // The overlay occupies the input area + up to `height` rows from chat_pane
        let overlay_height = height.min(self.input_area.height + self.chat_pane.height / 4);
        let overlay = Rect {
            x: self.chat_pane.x,
            y: self.status_bar.y + self.status_bar.height - overlay_height,
            width: self.chat_pane.width,
            height: overlay_height,
        };
        // Shrink chat_pane upward; hide input_area
        self.chat_pane = Rect {
            x: self.chat_pane.x,
            y: self.chat_pane.y,
            width: self.chat_pane.width,
            height: self.chat_pane.height.saturating_sub(overlay_height.saturating_sub(self.input_area.height)),
        };
        self.input_area.height = 0;
        self.bottom_overlay = Some(overlay); // NEW field on AppLayout
    }
}
```

**Why zero out `input_area`?**  
Because the overlay *replaces* the composer. The user cannot type while a pending plan needs approval. This is the core of Sally's UX: the plan is the only actionable UI element.

### 3.2 Render Order in `event_loop.rs::render()`

Current order (simplified):
1. Tab bar
2. Sidebar
3. Chat pane
4. Search bar
5. Bookmark panel
6. Bottom prompts (permission, skill trust, etc.)
7. Input box

**Insertion point:** After step 7 (input box), add:

```rust
// NEW: Bottom-pane overlay replaces composer
if let Some(ref overlay_state) = state.bottom_overlay {
    if let Some(overlay_area) = app_layout.bottom_overlay {
        frame.render_widget(Clear, overlay_area);
        match overlay_state.kind {
            BottomOverlayKind::PlanCard { ref pending } => {
                plan_approval_overlay::render(frame, overlay_area, pending, theme);
            }
        }
    }
}
```

**Chat pane is unaffected.** The scrollback continues to render behind the overlay. Virtual scroll, auto-scroll, and height cache all work unchanged because the chat pane is simply resized upward — same as when the bookmark panel is open.

### 3.3 Resolved Plans Stay Inline

When the user approves/rejects/edits the plan:
1. Event loop handler consumes `InputAction::PlanCardApprove` (etc).
2. Handler updates `conversation.plans[plan_id].status = Approved|Rejected|Editing`.
3. Handler sets `state.bottom_overlay = None` and `state.focus = FocusState::Input`.
4. On next render, the chat pane returns to full height.
5. The plan card now renders as **history** via `plan_card.rs` with `is_pending = false` (determined by `plan.status != Pending`).

**No migration of `content_blocks` needed for resolved plans.** They naturally stop being "pending" and become historical.

---

## 4. Migration Path

### Step 1: Extract Plan Cards from `content_blocks` (Winston's Option B)

**Goal:** Stop using `ContentBlockType::PlanCard` as the source of truth for pending plans.

**Files to touch:**
- `domain/models/message.rs` — keep `ContentBlockType::PlanCard` for serialization backward-compat, but stop producing it for new pending plans.
- `adapters/tui/widgets/chat_pane/mod.rs` — in `render_message`, skip rendering `PlanCard` blocks when `plan.status == Pending`. Instead, render a **placeholder hint**: "*(Pending plan shown below)*" or nothing.
- `infrastructure/runtime/event_loop.rs` — in the `PlanProposed` event arm, stop appending a `ContentBlockType::PlanCard` to `message.content_blocks`. Just set `conversation.plans[plan_id].status = Pending` and populate `state.pending_plan_card`.

**Tests to update:**
- `tests/conformance_plan_card.rs` — assertions that check inline rendering of pending cards will need to assert on `state.pending_plan_card` instead of chat pane buffer content.
- `tests/chat_pane.rs` — add a test: "pending plan card is NOT rendered in scrollback when bottom overlay is active."

### Step 2: Build Overlay System

**New files:**
- `adapters/tui/widgets/plan_approval_overlay.rs` (~150 lines)
- `adapters/tui/widgets/overlay_manager.rs` (~120 lines)

**Modified files:**
- `adapters/tui/state.rs` — add `BottomOverlayState`, `BottomOverlayKind`, `bottom_overlay` field.
- `adapters/tui/layout.rs` — add `bottom_overlay: Option<Rect>` to `AppLayout`, add `reserve_bottom_overlay()`.
- `adapters/tui/widgets/mod.rs` — expose new modules.

**No event loop changes yet.** The overlay is render-only; it won't be triggered because nothing populates `bottom_overlay`.

### Step 3: Migrate Plan Approval to Overlay

**Modified files:**
- `adapters/tui/app.rs` — replace inline `pending_plan_card` key intercept with `overlay_manager::OverlayKind::from_state(state).route_key(c, state)`.
- `infrastructure/runtime/event_loop.rs` —
  - In `PlanProposed` handler: set `state.bottom_overlay = Some(BottomOverlayState { kind: PlanCard { pending: ... }, prior_focus: FocusState::Input })`; set `state.focus = FocusState::Overlay(OverlayType::PlanCardBottomPane)`.
  - In `PlanCardApprove`/`Reject`/`Edit` handlers: set `state.bottom_overlay = None`; restore focus.
  - In `render()`: add the bottom overlay render branch.
- `domain/models/visual.rs` — add `OverlayType::PlanCardBottomPane`.

**New `InputAction` variants:**
```rust
// adapters/tui/app.rs or domain/events.rs
PlanCardApproveAutoEdit,  // 'a' key — approve AND switch to AutoEdit mode
```

### Step 4: Clean Up Inline Pending Rendering

**Files to touch:**
- `adapters/tui/widgets/chat_pane/mod.rs` — remove the `pending_plan_card` parameter from `render_with_search()` and all `is_pending` logic inside the render loop. Plan cards are now either:
  - Pending → rendered in bottom overlay (not in chat pane)
  - Resolved → rendered inline as history (using `plan.status` to determine styling)
- `adapters/tui/state.rs` — remove `pending_plan_card: Option<PendingPlanCard>` (or keep it as a cache; see §5.1).

**Tests to update:**
- `tests/e2e_plan_overlay.rs` — new test file verifying:
  - Plan proposed → composer hidden, overlay visible
  - `y` → overlay dismissed, plan status = Approved, composer restored
  - `Esc` → overlay dismissed, plan still pending, composer restored
  - Resolved plan appears in scrollback with correct styling

---

## 5. Risk Assessment

### 5.1 Biggest Technical Risk: `PendingPlanCard` Dual-Use

`PendingPlanCard` is currently read by:
1. **Chat pane renderer** — to highlight the inline card (`is_pending`)
2. **Event loop handlers** — to know which plan_id to approve/reject
3. **`app.rs`** — to intercept `y`/`e`/`n` keys

If we remove it in Step 4 while Step 3 handlers still reference it, we break the build. **Mitigation:** Keep `TuiState::pending_plan_card` as a **read-only cache** even after the overlay launches. Populate it alongside `bottom_overlay`. The event loop handlers continue to read `pending_plan_card.plan_id`. Deprecate it in a follow-up story (Epic 6 cleanup).

### 5.2 What Could Break Existing Tests

| Test File | Breakage Risk | Mitigation |
|-----------|---------------|------------|
| `tests/conformance_plan_card.rs` | High — asserts inline rendering | Update assertions to check `TuiState::bottom_overlay` instead of buffer cells |
| `tests/tui_render.rs` | Medium — golden/layout tests | Add `bottom_overlay: None` to test fixtures; no layout change when inactive |
| `tests/e2e_help_overlay.rs` | Low — help overlay z-order | Ensure bottom overlay renders AFTER help overlay so help can still appear on top |
| `tests/chat_pane.rs` | Medium — `render_with_search` signature | Remove `pending_plan_card` param in Step 4; update call sites in tests |
| `tests/scroll.rs` | Low — scroll offset calculations | `reserve_bottom_overlay` shrinks chat pane; test with overlay active to verify clamping |

### 5.3 Minimizing Scope While Achieving Sally's UX Goals

**Sally's hard requirements:**
- ✅ Bottom pane replaces composer
- ✅ `y`/`a`/`n`/`e` keys
- ✅ `Esc` returns focus
- ✅ Resolved plans stay inline
- ✅ YOLO mode toast
- ✅ No timeout

**Scope cuts that still deliver:**

1. **No view-stack.** We only need one bottom overlay at a time. If a permission prompt arrives while a plan overlay is up, queue it (existing `PermissionQueue` pattern) or dismiss the plan overlay. **Decision:** queue permissions; plans are higher priority than permissions in the approval hierarchy.

2. **No generic overlay framework.** The `overlay_manager.rs` is a thin router, not a plugin system. Future overlays (AskUserQuestion, Permission compact) add a variant to `BottomOverlayKind` and a branch in `route_key()`. This is ~20 lines per new overlay.

3. **Keep `plan_approval.rs` (centered) untouched.** Story 6-0d's Plan mode exit approval is a different workflow with different keys (`y`/`a`/`n`/`e` for plan file approval vs `y`/`a`/`n`/`e` for plan card approval — actually the same keys, but different contexts). The centered overlay is for **plan mode exit** (file-based). The bottom overlay is for **inline plan cards** (task-based). They coexist.

4. **YOLO mode = one-line status bar flash.** Instead of a toast widget, emit `AppEvent::StatusFlash { message: "Plan approved — YOLO mode active", duration_ms: 2000 }` and reuse the existing status-bar flash infrastructure. **Zero new UI code.**

---

## 6. File-Level Implementation Checklist

### New Files
- [ ] `src/adapters/tui/widgets/plan_approval_overlay.rs`
- [ ] `src/adapters/tui/widgets/overlay_manager.rs`

### Modified Files (in dependency order)
- [ ] `src/domain/models/visual.rs` — add `OverlayType::PlanCardBottomPane`
- [ ] `src/adapters/tui/state.rs` — add `BottomOverlayState`, `BottomOverlayKind`, `bottom_overlay`
- [ ] `src/adapters/tui/layout.rs` — add `bottom_overlay: Option<Rect>`, `reserve_bottom_overlay()`
- [ ] `src/adapters/tui/widgets/mod.rs` — re-export new modules
- [ ] `src/adapters/tui/app.rs` — `OverlayKind` routing; Esc dismiss; new `PlanCardApproveAutoEdit` action
- [ ] `src/infrastructure/runtime/event_loop.rs` — populate `bottom_overlay` on `PlanProposed`; render branch; handler updates
- [ ] `src/adapters/tui/widgets/chat_pane/mod.rs` — Step 1: skip pending plan cards in scrollback; Step 4: remove `pending_plan_card` param

### Test Files
- [ ] `tests/conformance_plan_card.rs` — migrate assertions to overlay state
- [ ] `tests/chat_pane.rs` — add "pending plan not in scrollback" test
- [ ] `tests/e2e_plan_overlay.rs` — **new** end-to-end overlay lifecycle test
- [ ] `tests/tui_render.rs` — verify layout with `bottom_overlay` active

---

## 7. Open Questions for Dev Handoff

1. **Height policy:** Should the bottom overlay be fixed at 6 rows, or dynamic based on plan task count? *Recommendation:* cap at 8 rows; scroll if more tasks.
2. **Markdown rendering:** `plan_approval.rs` uses plain text. Should the bottom overlay use `markdown::render_lines()` for plan descriptions? *Recommendation:* yes, reuse `markdown` module for consistency with chat pane.
3. **Auto-open on plan propose:** Currently `auto_open_on_task_plan` controls the Tasks sidebar. Should proposing a plan also force `FocusState::Overlay(PlanCardBottomPane)` even if the user is in `FocusState::Chat`? *Recommendation:* yes, plans are blocking; steal focus.
4. **Concurrent pending plan + permission:** If a `PermissionRequest` arrives while the plan overlay is active, should it queue or should the plan overlay auto-dismiss? *Recommendation:* queue permission in `permission_queue`; plans are the user's explicit intent and should not be interrupted.

---

## 8. Summary

The architecture is **incremental and low-risk**:

- **One new concept:** `BottomOverlayState` in `TuiState`.
- **One new widget:** `plan_approval_overlay.rs` (bottom pane).
- **One new router:** `overlay_manager.rs` (collapses scattered `if` blocks).
- **Layout change:** `reserve_bottom_overlay()` mirrors existing `reserve_search_bar()` / `reserve_bookmark_panel()`.
- **No domain changes.** `conversation.plans` and `PlanStatus` are untouched.
- **No event loop refactoring.** The 8,405-line file gets ~40 new lines in `render()` and ~10 lines in the `PlanProposed` handler.

This keeps Sally's UX intact while respecting the existing codebase's patterns and minimizing blast radius.
