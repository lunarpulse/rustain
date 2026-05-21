# CLAUDE.md

## Project Overview

Rustain — A fast, extensible, terminal-native AI coding agent. Standalone Rust binary using ratatui + crossterm, with selective dependencies on rustycode libraries. Supports three open interop standards: Agent Skills, MCP, A2A.

**Design Principle:** Use rustycode where it helps. Don't let rustycode hold rustain back. Unconstrained for Claudian-grade polish.

## Commands

```bash
cargo build           # Build
cargo run             # Run TUI
cargo check           # Type check
cargo test            # Run tests
cargo clippy          # Lint
cargo fmt             # Format
```

## Architecture (Hexagonal)

```
rustain/src/
├── main.rs                          # Composition root (only file naming concrete adapters)
├── lib.rs                           # Lib re-export for integration tests
├── domain/                          # Pure domain — NO I/O crate imports
│   ├── models/                      # Types/enums/structs (FocusState, AppConfig, NoticeLevel)
│   ├── ports/                       # Port trait definitions (Story 1.1b+)
│   ├── services/                    # Pure functions on domain types
│   │   └── tool_scheduler.rs        # ToolCall 7-state FSM + parallel/sequential batching
│   ├── events.rs                    # AppEvent, DomainInputEvent, ChunkAction
│   └── errors.rs                    # DomainError, ConfigError, EventError
├── adapters/                        # External system interfaces
│   ├── tui/                         # ratatui + crossterm (terminal rendering)
│   │   ├── terminal.rs              # setup/teardown/restore_terminal_raw
│   │   ├── layout.rs                # compute_layout (60x16 min, 80x24 standard)
│   │   ├── state.rs                 # TuiState (rendering state)
│   │   ├── app.rs                   # handle_input, convert_crossterm_event
│   │   └── widgets/                 # empty_state, chat_pane, status_bar, input_box
│   ├── cli/                         # CLI argument parsing (clap)
│   └── noop.rs                      # NoOp adapter stubs for all ports
├── infrastructure/                  # OS/runtime concerns
│   ├── runtime/                     # Orchestration
│   │   ├── event_loop.rs            # 4-branch tokio::select! loop
│   │   ├── app_state.rs             # Runtime state coordination
│   │   └── agent_core.rs            # Central runtime orchestrator
│   ├── startup.rs                   # Ordered startup sequence (<20ms to first frame)
│   ├── config.rs                    # Configuration loader
│   ├── logging.rs                   # tracing + rolling-file (10MB rotation)
│   ├── signals.rs                   # Panic hook + SIGTERM/SIGINT handlers
│   └── paths.rs                     # ~/.rustain/ data dir, crash logs
```

**Dependency rule:**
- `domain/` → imports NOTHING from adapters/ or infrastructure/
- `adapters/` → imports from domain/ ONLY
- `infrastructure/` → imports from domain/ ONLY
- `main.rs` → ONLY file naming concrete adapter types

## Capability Provider Architecture (CPA)

The core extensibility pattern. All interop protocols implement `CapabilityProvider` (4 methods per Decision Gate 3.1):

```
DISCOVER → REGISTER → INVOKE → RENDER
```

```rust
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Stable protocol identifier — used by `CapabilityId` namespace.
    fn protocol(&self) -> &str;
    /// Provider's static feature support, pattern-matched by ToolExposurePort (9.4).
    fn capabilities(&self) -> ProviderCapabilities;
    /// Discover capabilities. For McpProvider: reads cached_tools, no I/O on hot path.
    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError>;
    /// Invoke a capability with input + cancel token.
    async fn invoke(&self, id: &CapabilityId, input: serde_json::Value,
                    cancel: CancellationToken) -> Result<ToolResult, CapabilityError>;
}
```

**Adding a new protocol:** Implement `CapabilityProvider`, call `registry.register()`. Done — the autocomplete and TUI status panel pick up capabilities automatically.

**Current providers (shipped and upcoming):**

| Provider | Implementation | Story |
|----------|---------------|-------|
| `McpProvider` | Wraps `McpClientAdapter` (9.1+9.2) behind the trait | **Story 9.3a** (this story) |
| `BuiltinProvider` | Refactor of `ToolSetAdapter` behind the trait | Story 9.3b |
| `SkillsProvider` | Refactor of skill executor behind the trait | Story 9.3b |
| `A2aProvider` | Implements `CapabilityProvider` when A2A lands | Epic 14 |
| `SubagentProvider` | Implements `CapabilityProvider` for subagent dispatch | Epic 10 |

**CapabilityRegistry** is internal to `CompositeToolsetAdapter` — NO top-level `AppState.capability_registry`. Conformance test `tests/conformance_capability_registry.rs::test_no_capability_registry_on_app_state` enforces this.

**Namespace:** Registry ids use `::` (double-colon) separator: `mcp::postgres::query`, `builtin::bash`, `skill::review`. The LLM-wire `mcp__<server>__<tool>` shape (Story 9.2) is a SEPARATE namespace — bridge methods `CapabilityId::from_mcp_wire_name` / `to_mcp_wire_name` convert between them.

**`CapabilityEvent` stream:** `Registered` / `Deregistered` / `Updated` flow through the existing `AppEvent` bus (same `event_tx` channel as 9.1+9.2), surfacing reactively to autocomplete + adapter-status panel.

## ToolCall FSM (Story 6-0b)

Every tool invocation follows a strict 7-state lifecycle:

```
Validating → Scheduled → (AwaitingApproval)? → Executing → Success
     ↓            ↓              ↓                 ↓         Error
 Cancelled    Cancelled      Cancelled         Cancelled   Cancelled
```

- `Validating` — input schema checked via `ToolSetPort::validate_input`
- `Scheduled` — policy check (`mode_risk_outcome`) decides prompt vs auto-approve
- `AwaitingApproval` — waiting for user decision (foreground turn only)
- `Executing` — `ToolSetPort::execute` running
- `Success` / `Error` / `Cancelled` — terminal states

Parallel batching: when all tools in a batch have `parallel_safe == true`, `FuturesOrdered` runs them concurrently; otherwise sequential fallback. Cancellation uses per-call `CancellationToken::child_token()` so individual tools can be aborted without killing the whole batch.

`exit_plan_mode` is risk-Safe and never enters `AwaitingApproval` when in Plan mode — the PermissionChain short-circuits to `Allow` directly. `propose_plan` is also risk-Safe — it only emits a UI event, so it short-circuits to `Allow` in all modes without prompting.

## Ownership Topology

Rustain nodes have three relationship types:
- **Owned**: spawner has absolute authority (Kill, Pause, ChangeModel). Owned nodes MUST report, CANNOT refuse. On disconnect → retry → self-destruct.
- **Peer**: independent, mutual consent. Either can refuse.
- **Self**: the interactive TUI, root of ownership tree (the "One Ring").

Master has direct authority over entire tree (Option B with discovery). Spawn events propagate up.

## Dependencies on rustycode

Selective — provider adapters, tools, MCP only:

| Crate | What rustain uses |
|---|---|
| `domain` | Provider trait, Tool trait, shared types (building blocks only) |
| `adapters` | Tool implementations (Bash, Read, Write, Edit, Glob, Grep, WebFetch), McpClientAdapter |
| `llm-provider` | Provider configuration types |

**Not used:** `application` (request-response), DI container, `domain::Session`

## Key Design Decisions

### Async Lock Policy (Story 16-0 AC2)

- **`std::sync::RwLock` / `std::sync::Mutex` MUST NOT appear in `src/infrastructure/` or `src/adapters/`** unless explicitly tagged with `// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK` on every line, with a doc comment justifying the exception (see SecurityAdapter pattern below).
- **`tokio::sync::RwLock` / `tokio::sync::Mutex` are the default** for shared mutable state in async code. The guard types are `Send + Sync` and survive task spawning — `parking_lot` guards are `!Send` and would block the runtime under contention.
- **Always release write guards before `.await` points.** Write guard scope should be a tight block: `{ *state.skill_registry.write().await = new; }` — drop on block exit, before any subsequent `.await`. The AC4 consistency test verifies functional consistency after a write; a separate lint or manual review is required to enforce guard lifetime.
- **Pass shared state as `Arc<tokio::sync::RwLock<T>>` by value** (not by reference). Cloning the Arc is cheap (atomic refcount bump) and avoids lifetime entanglement.
- **Single `std::sync::RwLock` exception:** `SecurityAdapter::active_skill_dirs` (see `src/adapters/security_adapter.rs:20`). The lock is held only for ≤1µs critical sections (HashSet::contains / insert / remove on a small set) and is **never** held across `.await`. Converting to `tokio::sync::RwLock` would propagate `async fn` through `SecurityPort::check_workspace_access` into the synchronous tool-execution path — cost disproportionate to risk. Conformance test `test_no_std_sync_lock_in_async_module` enforces this exception via the `// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK` tag.

- **Own streaming provider:** `StreamingProvider` trait + Anthropic SSE client (rustycode is request-response)
- **Own conversation model:** Claudian-compatible types, not rustycode's `Session`/`Message`
- **Capability Provider Architecture:** Protocol-agnostic extensibility. Future protocols = implement one trait
- **Profile System:** Named TOML profiles compose one adapter per port dimension. See `docs/profiles.md` for built-in profiles and schema. See `docs/adapter-composition.md` for the per-port factory dispatch pattern.
- **Ownership topology:** Hierarchical ownership + peer networking + self-destruct-on-abandonment
- **Event loop:** `tokio::select!` on unified `AppEvent` channel
- **ApprovalRuntime pub/sub:** `tokio::sync::broadcast` channel between `ToolScheduler` and TUI event loop; `ApprovalRuntime` holds pending requests and session auto-allow set; resolves via `oneshot` per request
- **Tracing:** routed to `~/.rustain/rustain.log` (stdout owned by ratatui)
