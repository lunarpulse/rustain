# CLAUDE.md

## Project Overview

Rustain — A Claudian-like terminal UI for AI-assisted development. Standalone Rust binary using ratatui + crossterm, with selective dependencies on rustycode libraries for LLM providers, tools, and MCP.

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
├── main.rs              # Entry point (tokio runtime, tracing, App::run)
├── app.rs               # Event loop (tokio::select! on AppEvent channel)
├── core/                # Service layer (rustain's own, NOT rustycode's use cases)
│   ├── provider.rs      # StreamingProvider trait + AnthropicStreamingProvider
│   └── service.rs       # Streaming service (tool execution loop, permission bridge)
├── tui/                 # ratatui rendering
│   ├── ui.rs            # Root layout (tab bar | chat+sidebar | status | input + overlays)
│   └── widgets/         # Widget modules (tool_call, thinking, diff, etc.)
└── types/               # All type definitions
    ├── app_state.rs     # AppState, AppMode, TabState, Focus, PermissionMode
    ├── conversation.rs  # Conversation, ChatMessage, ContentBlock, ForkSource, UsageInfo
    ├── event.rs         # AppEvent (Key, Resize, Stream, Tick)
    └── stream.rs        # TuiStreamEvent (15 variants for streaming render)
```

## Dependencies on rustycode

Selective — provider adapters, tools, MCP only:

| Crate | What rustain uses |
|---|---|
| `domain` | Provider trait, Tool trait, shared types (building blocks only) |
| `adapters` | Tool implementations (Bash, Read, Write, Edit, Glob, Grep, WebFetch), McpClientAdapter |
| `llm-provider` | Provider configuration types |

**Not used:** `application` (request-response use cases), DI container, `domain::Session` (replaced by own Conversation model)

## Key Design Decisions

- **Own streaming provider:** Rustain has its own `StreamingProvider` trait + Anthropic SSE client. Rustycode's `AnthropicProviderAdapter` is request-response; rustain needs token-by-token streaming.
- **Own conversation model:** Claudian-compatible types (`Conversation`, `ChatMessage`, `ContentBlock`) — not rustycode's simpler `Session`/`Message`.
- **Own session storage:** `.claude/sessions/{id}.meta.json` (Claudian-compatible), independent of rustycode's `FileSystemSessionRepository`.
- **Event loop:** `tokio::select!` on unified `AppEvent` channel. Dedicated crossterm reader task + tick timer + stream events from background tasks.
- **Permission bridge:** Background streaming task sends `PermissionRequest` with `oneshot::Sender`, blocks until UI responds with y/a/n/Esc.
