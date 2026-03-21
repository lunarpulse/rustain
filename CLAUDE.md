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

## Architecture

```
rustain/src/
├── main.rs              # Entry point (tokio, tracing to file, panic hook)
├── app.rs               # Event loop (tokio::select! on AppEvent channel)
├── core/                # Service layer (rustain's own, NOT rustycode's)
│   ├── provider.rs      # StreamingProvider trait + AnthropicStreamingProvider (SSE)
│   ├── service.rs       # Streaming service (tool execution loop, permission bridge)
│   └── registry.rs      # CapabilityRegistry — central coordinator for all protocols
├── tui/                 # ratatui rendering
│   ├── ui.rs            # Root layout (tab bar | chat+sidebar | status | input + overlays)
│   └── widgets/         # Widget modules (tool_call, thinking, diff, etc.)
└── types/               # All type definitions
    ├── app_state.rs     # AppState, AppMode, TabState, Focus, PermissionMode
    ├── capability.rs    # CapabilityProvider trait, Capability, CapabilityEvent, PermissionScope
    ├── conversation.rs  # Conversation, ChatMessage, ContentBlock, ForkSource, UsageInfo
    ├── event.rs         # AppEvent (Key, Resize, Stream, Permission, Tick)
    └── stream.rs        # TuiStreamEvent (16 variants for streaming render)
```

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
