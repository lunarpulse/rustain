# Conformance Test Discipline

This directory holds **conformance tests** — tests that enforce architectural invariants and per-port/per-service contracts that must hold regardless of which adapter or implementation is in use.

## Two Kinds of Conformance Tests

### 1. Architecture conformance (structural)

**File:** `tests/conformance.rs` (existing, stable).

Scans source files to enforce:
- Domain layer imports no I/O crates (`crossterm`, `ratatui`, `tokio`, `reqwest`, `arc_swap`).
- Domain layer does not import from `crate::adapters` or `crate::infrastructure`.
- Hexagonal directory structure is present.
- No adapter-to-adapter cross-imports.
- No raw `env::var()` calls outside the approved utility.

These tests run as part of `cargo test` and enforce the Dependency Rule (Clean Architecture) at CI time.

### 2. Behavioral / per-service conformance (runtime)

**Files:** `tests/conformance_<subject>.rs` (one per domain service or port trait).

Each file holds the minimum set of behavioral assertions that every implementation of a given service / port must pass. For now these are primarily **skeletons** created with `#[ignore = "pending story X-Y"]` tests — they will be fleshed out as the corresponding stories are implemented.

## Current Inventory

| File | Subject | Source of truth | Status |
|---|---|---|---|
| `conformance.rs` | Clean Architecture invariants | Hexagonal Dependency Rule | ✅ active |
| `conformance_cancellation.rs` | CancellationToken tree + subprocess `kill_on_drop` | ADR-06-03 · Story 6-0a AC2, AC7 | 📋 skeleton |
| `conformance_toolcall_fsm.rs` | `ToolCall` 7-state FSM legal transitions | ADR-06-02 · Story 6-0b AC3 | 📋 skeleton |
| `conformance_approval_runtime.rs` | `ApprovalRuntime` pub/sub concurrency + fast-path + cancel-by-source | ADR-06-01 · Story 6-0c AC3, AC5, AC9 | 📋 skeleton |
| `conformance_plan_mode.rs` | Plan-mode workflow (slug determinism, gate, handoff) | ADR-06-10 · Story 6-0d AC1–AC9 | 📋 skeleton |

## Adding a New Conformance Test File

Convention:

```rust
//! Conformance tests for <subject>.
//!
//! Source of truth: <story or ADR>
//! Rationale: <why these invariants matter>
//!
//! Each test below corresponds to a story AC. Tests start `#[ignore]` with an
//! **empty body** until the implementing story is ready — so both
//! `cargo test` (skips them) and `cargo test -- --include-ignored` (runs them
//! as no-op passes) succeed on CI. When the story lands, the developer
//! removes `#[ignore]` and fills the body with real assertions.

/// Story X-Y AC1: <short rationale>.
/// When implemented: <what the test will assert>.
#[test]
#[ignore = "pending story X-Y AC1: <short description>"]
fn ac1_<short_name>() {}
```

Guidelines:

- **One test per AC** when the AC is scoped narrowly; combine closely-related ACs sparingly.
- Keep skeletons **body-empty** (not `todo!()`, not `unimplemented!()`) so that `cargo test -- --include-ignored` reports a no-op pass rather than a panic. The doc comment above the test documents the intended assertion.
- Keep skeletons **import-free of types that don't exist yet** — no imports of `rustain::domain::services::approval_runtime` etc. until the story starts.
- Use `#[ignore = "..."]` with a **reason string** that includes the story ID + AC number + short description. This string shows in CI output as the pointer to the implementing work.
- When a story lands and the test is implemented, **remove `#[ignore]` and add real assertions**. Remove or rewrite the "When implemented:" doc comment.

## Running Conformance Tests

```bash
# Default: architecture conformance only (skeletons ignored)
cargo test --test conformance

# Specific new skeleton:
cargo test --test conformance_cancellation -- --ignored    # runs even ignored tests
cargo test --test conformance_cancellation                 # skips ignored (CI-safe)

# All conformance tests in one go:
cargo test --tests conformance
```

## Relationship to Story ACs

Every AC that requires a runtime behavioral check should map to a conformance test. When authoring a new story:

1. List the ACs.
2. For each AC requiring behavioral verification, add a skeleton test to the relevant `conformance_*.rs` file (or create a new file).
3. Mark `#[ignore = "pending story X-Y ACN"]`.
4. During implementation, flip `#[ignore]` off as each AC lands.
5. All non-ignored conformance tests must pass before the story moves to `review`.

## Source of Truth

- ADRs: `_bmad-output/planning-artifacts/architecture/adr/`
- Research: `_bmad-output/planning-artifacts/research/technical-plan-mode-task-orchestration-research-2026-04-24.md`
- Sprint change proposal: `_bmad-output/planning-artifacts/sprint-change-proposal-2026-04-24.md`
- Story files: `_bmad-output/implementation-artifacts/`
