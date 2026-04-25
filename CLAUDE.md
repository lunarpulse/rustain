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

The core extensibility pattern. All interop protocols implement `CapabilityProvider`:

```
DISCOVER → ACTIVATE → EXECUTE → RENDER → GOVERN
```

```rust
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    fn protocol(&self) -> &str;
    fn mention_category(&self) -> MentionCategory;
    async fn discover(&self, config: &WorkspaceConfig) -> Result<Vec<Capability>>;
    async fn activate(&self, cap: &Capability, session: &SessionContext) -> Result<ActivatedCapability>;
    async fn execute(&self, activated: &ActivatedCapability, input: CapabilityInput,
                     tx: &mpsc::UnboundedSender<CapabilityEvent>) -> Result<()>;
    fn permission_scope(&self, cap: &Capability) -> PermissionScope;
}
```

**Adding a new protocol:** Implement `CapabilityProvider`, call `registry.register()`. Done — @mentions, permissions, TUI rendering work automatically.

**Current providers (to implement):**
- `AgentSkillsProvider` — `.agents/skills/`, `.claude/skills/` (Knowledge)
- `McpProvider` — `.claude/mcp.json` (Tools)
- `A2aProvider` — `.claude/a2a.json` + spawn/despawn (Agents)

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

- **Own streaming provider:** `StreamingProvider` trait + Anthropic SSE client (rustycode is request-response)
- **Own conversation model:** Claudian-compatible types, not rustycode's `Session`/`Message`
- **Capability Provider Architecture:** Protocol-agnostic extensibility. Future protocols = implement one trait
- **Ownership topology:** Hierarchical ownership + peer networking + self-destruct-on-abandonment
- **Event loop:** `tokio::select!` on unified `AppEvent` channel
- **Permission bridge:** `oneshot` channel between streaming task and UI
- **Tracing:** routed to `~/.rustain/rustain.log` (stdout owned by ratatui)
