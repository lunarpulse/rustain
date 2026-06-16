# Rustain

> A composable AI agent platform in the terminal — one binary that adapts
> to whatever you need it to be.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-edition_2024-orange.svg)](Cargo.toml)
[![Status](https://img.shields.io/badge/status-v0.1--alpha-yellow.svg)](#project-status--roadmap)

Rustain is a standalone Rust terminal application. By default it's an AI coding
agent — streaming responses, tool execution, collapsible content blocks,
vim-style navigation, all terminal-native. Underneath, a hexagonal port
architecture lets the same binary act as a personal assistant, a DevOps monitor,
or any custom role — by switching a profile.

No vendor lock-in. Four LLM providers (Anthropic, OpenAI, Google, OpenRouter).
Three open interop standards (Agent Skills, MCP, A2A). 27 port traits. 137k
lines of Rust. One binary.

---

## Table of contents

- [Why Rustain exists](#why-rustain-exists)
- [The big idea](#the-big-idea)
- [Architecture at a glance](#architecture-at-a-glance)
- [Pick your door](#pick-your-door)
  - [Run Rustain](#-run-rustain)
  - [Develop Rustain](#-develop-rustain)
  - [Understand Rustain](#-understand-rustain)
- [Repository layout](#repository-layout)
- [Feature highlights](#feature-highlights)
- [Profile system](#profile-system)
- [Four-layer extensibility](#four-layer-extensibility)
- [Project status & roadmap](#project-status--roadmap)
- [Contributing](#contributing)
- [License](#license)

---

## Why Rustain exists

The 2026 agent landscape is a fragmented archipelago. Every tool picks a lane
and stays there:

| Approach | Example | What it costs you |
|---|---|---|
| **Vendor-locked** | Claude Code, Cursor | One model provider, one role (coding). Rich UX but no composability. |
| **Multi-provider, thin UI** | Aider | Model freedom, but a minimal interface and no plugin architecture. |
| **Plugin-rich, loose contracts** | OpenClaw | Extensible, but TypeScript-only with runtime type confusion. |
| **IDE-embedded** | Claudian (Obsidian), Copilot | Locked to the host app. Can't run headless, can't serve as a network agent. |

The shared failure: **identity is hardcoded.** A coding agent is always a coding
agent. A personal assistant is always a personal assistant. If you want both, you
install two tools.

Rustain answers differently: **identity is composed from ports.** The same binary
with a different profile file becomes a different agent — different persona,
different memory strategy, different tools, different channels — without
recompiling.

## The big idea

Rustain separates **what an agent is** (ports and profiles) from **what an agent
can do** (tools, skills, protocols).

- **27 port traits** define the behavioral dimensions: persona, memory, session,
  tools, channels, scheduler, context, security, approval, clipboard, and more.
  Each port is a Rust trait. Adapters implement ports. Profiles compose adapters
  into agent roles.
- **Three open standards** define external reach: Agent Skills (knowledge), MCP
  (tools), A2A (agent-to-agent delegation). All governed by one unified
  permission system.
- **Profiles are TOML files, not code.** `coding` is the default. Switch to
  `personal-assistant` and the same binary gains daily-log memory, a Telegram
  channel, and a cron scheduler. Create your own profile — no compilation
  required.
- **Runtime swapping** lets you change identity mid-session. Hot swap (instant:
  persona, tools, context), warm swap (brief pause: memory, session), cold swap
  (agent loop restart: channels, scheduler). The TUI stays responsive throughout.

## Architecture at a glance

Hexagonal architecture with strict dependency rules:

```
┌──────────────────────────────────────────────────────────────┐
│                        main.rs                               │
│              (composition root — only file                    │
│               naming concrete adapter types)                  │
└──────────────┬───────────────────────────────┬───────────────┘
               │                               │
    ┌──────────▼──────────┐         ┌──────────▼──────────┐
    │      adapters/      │         │   infrastructure/   │
    │                     │         │                     │
    │  tui/ (ratatui)     │         │  runtime/           │
    │  cli/ (clap)        │         │    event_loop.rs    │
    │  anthropic/         │         │    app_state.rs     │
    │  openai/            │         │    agent_core.rs    │
    │  ollama/            │         │  startup.rs         │
    │  mcp/               │         │  config.rs          │
    │  channel/           │         │  logging.rs         │
    │  daemon/            │         │  signals.rs         │
    │  vector_search/     │         │  paths.rs           │
    │  scheduler/         │         └─────────────────────┘
    │  ...40+ adapters    │
    └──────────┬──────────┘
               │ imports from
    ┌──────────▼──────────┐
    │      domain/        │
    │                     │
    │  models/            │  ← Pure types, enums, structs
    │  ports/             │  ← 27 trait definitions
    │  services/          │  ← Pure functions (tool FSM, frontmatter)
    │  events.rs          │  ← AppEvent, DomainInputEvent
    │  errors.rs          │  ← DomainError hierarchy
    │                     │
    │  NO I/O imports     │
    └─────────────────────┘
```

**Dependency rule:** `domain/` imports nothing from `adapters/` or
`infrastructure/`. Adapters and infrastructure import from `domain/` only.
`main.rs` is the sole file that names concrete types.

The runtime is a four-branch `tokio::select!` event loop processing a unified
`AppEvent` channel. Startup targets < 20ms to first frame render.

## Pick your door

### ▶️ Run Rustain

**Prerequisites:** Rust toolchain (edition 2024), an LLM API key.

```sh
# Build and run
cargo build --release
cargo run --release

# Or with a specific provider
ANTHROPIC_API_KEY=sk-... cargo run --release
OPENROUTER_API_KEY=sk-... cargo run --release
```

First-run creates `~/.rustain/` for configuration, logs, and session data. No
config files required for the basic flow — just an API key in the environment
or stored via `rustain auth login <provider>`.

**As a daemon** (personal assistant mode):

```sh
rustain daemon start       # start background daemon
rustain daemon attach      # connect TUI to running daemon
rustain daemon status      # check daemon state
rustain daemon stop        # shut down
```

Daemon mode enables always-on channels (Telegram, scheduled tasks) that persist
across TUI sessions. Systemd and launchd service templates are in `dist/`.

**Authentication** (credential management):

```sh
rustain auth login anthropic   # store API key (masked entry + validation)
rustain auth login openai      # store OpenAI key
```

### 🛠️ Develop Rustain

```sh
cargo build                # debug build
cargo check                # type check (fastest feedback)
cargo test                 # run the test suite (~180 test files)
cargo clippy               # lint
cargo fmt                  # format

# Feature-gated builds
cargo build --no-default-features --features anthropic     # minimal
cargo build --features telegram                            # with Telegram channel
cargo build --features vector-search                       # with local embeddings
```

**Default features:** `anthropic`, `openai`, `ollama`, `clipboard`, `mcp`,
`meta-search`.

The test suite includes 47 conformance tests that enforce architectural
invariants (dependency rules, lock policies, namespace conventions, port
contracts). These run on every change and are the primary guard against
architectural drift.

**Tracing:** logs route to `~/.rustain/rustain.log` (10MB rolling rotation)
because stdout is owned by the ratatui terminal. Set `RUST_LOG=debug` for
verbose output.

### 📚 Understand Rustain

- **Architecture** — hexagonal ports, adapter composition, event loop:
  [`docs/`](docs/) and [`CLAUDE.md`](CLAUDE.md)
- **Profiles** — named adapter compositions, built-in profiles, schema:
  [`docs/profiles.md`](docs/profiles.md) and [`profiles/`](profiles/)
- **Adapter composition** — per-port factory dispatch pattern:
  [`docs/adapter-composition.md`](docs/adapter-composition.md)
- **MCP integration** — client adapter, server management:
  [`docs/mcp.md`](docs/mcp.md)
- **Daemon mode** — lifecycle, supervision, crash recovery:
  [`docs/daemon.md`](docs/daemon.md)
- **Configuration** — layered config, provider setup:
  [`docs/configuration.md`](docs/configuration.md)
- **Testing** — strategy, conformance tests, E2E harness:
  [`TESTING.md`](TESTING.md)
- **Planning artifacts** — PRD, architecture, epics, sprint status:
  [`prd.md`](prd.md) (local copy)

## Repository layout

```
rustain/
├── src/
│   ├── main.rs                 # composition root
│   ├── lib.rs                  # lib re-export for integration tests
│   ├── domain/                 # pure domain — NO I/O crate imports
│   │   ├── models/             # types, enums, structs
│   │   ├── ports/              # 27 port trait definitions
│   │   ├── services/           # pure functions (tool FSM, frontmatter parser)
│   │   ├── events.rs           # unified event types
│   │   └── errors.rs           # domain error hierarchy
│   ├── adapters/               # 40+ adapter implementations
│   │   ├── tui/                # ratatui + crossterm terminal UI
│   │   ├── anthropic/          # Anthropic SSE streaming provider
│   │   ├── openai/             # OpenAI-compatible provider
│   │   ├── ollama/             # Local model provider
│   │   ├── mcp/                # MCP client adapter
│   │   ├── channel/            # Telegram, socket channels
│   │   ├── daemon/             # daemon lifecycle, crash recovery
│   │   ├── vector_search/      # fastembed + cosine similarity
│   │   ├── scheduler/          # cron task scheduling
│   │   └── ...                 # memory, skills, security, sandbox, etc.
│   └── infrastructure/         # OS/runtime concerns
│       ├── runtime/            # tokio event loop, app state, agent core
│       ├── startup.rs          # ordered startup (< 20ms to first frame)
│       ├── config.rs           # layered configuration loader
│       └── signals.rs          # panic hook + signal handlers
├── tests/                      # 180 test files inc. 47 conformance tests
├── profiles/                   # built-in profile definitions (TOML)
│   ├── base.toml
│   ├── coding.toml
│   └── personal-assistant.toml
├── docs/                       # architecture, adapter composition, MCP, profiles
├── dist/                       # systemd + launchd service templates
└── Cargo.toml                  # edition 2024, MIT license
```

## Feature highlights

**Terminal UI** — ratatui 0.30 + crossterm 0.29. Multi-tab streaming chat,
collapsible tool blocks, vim-style navigation (`j`/`k`/`i`), command palette,
@-mention autocomplete, context meter with cache breakdown, plan mode overlay.

**Tool execution** — seven-state FSM per tool call (`Validating → Scheduled →
AwaitingApproval → Executing → Success/Error/Cancelled`). Parallel batching for
safe tools, sequential fallback otherwise. Per-call cancellation tokens.

**Capability Provider Architecture** — protocol-agnostic extensibility. All
interop protocols implement one trait (`CapabilityProvider`: discover, register,
invoke, render). Adding a new protocol = implement one trait, call
`registry.register()`.

**Persistent memory** — project-scoped memory, daily logs, long-term memory with
hybrid retrieval (BM25 + vector cosine via fastembed, RRF fusion with
time-decay). Context assembly with windowing (turn grouping + relevance-weighted
LRU trim).

**Daemon mode** — background operation with systemd/launchd supervision, crash
recovery (latest-only crash journal + capped logs), multi-client attach via Unix
socket, foreground detection with boot-id veto and process nonce verification.

**Shell completions** — tab-completion scripts generated on demand for bash, zsh,
fish, and PowerShell via `rustain completions <shell>`. See
[docs/shell-completions.md](docs/shell-completions.md) for per-shell install paths.

## Profile system

Profiles are named TOML files that compose one adapter per port dimension:

```toml
# profiles/personal-assistant.toml
extends = "base"

[persona]
adapter = "custom"
system_prompt_file = "prompts/assistant.md"

[memory]
adapter = "daily-log"

[session]
adapter = "daemon"

[channel]
adapter = "telegram"

[scheduler]
adapter = "cron"
```

Three built-in profiles ship with the binary:

| Profile | Persona | Memory | Session | Channel | Scheduler |
|---|---|---|---|---|---|
| **base** | minimal | project-scoped | local | TUI only | none |
| **coding** | coding agent | project-scoped + long-term | local | TUI only | none |
| **personal-assistant** | assistant | daily-log + long-term | daemon | Telegram | cron |

Switch at runtime via the TUI or CLI. Custom profiles are TOML files in
`~/.rustain/profiles/` or `.rustain/profiles/`.

## Four-layer extensibility

```
Layer -1: Ports          What the agent IS (27 port traits, composed by profiles)
Layer  0: Built-in tools What the agent ships with (bash, read, write, edit, glob, grep, web fetch)
Layer  1: Agent Skills   Markdown-based procedural knowledge (agentskills.io standard, 30+ tools)
Layer  2: MCP            Tool-level interop via external servers (stdio/SSE/HTTP)
Layer  3: A2A            Agent-to-agent delegation (localhost/LAN/internet)
```

- **Agent Skills** follow the open standard at agentskills.io — any skill from
  Claude Code, Codex, Gemini CLI, or Cursor works in Rustain without
  modification. Discovery paths: `.agents/skills/`, `.rustain/skills/`,
  `.claude/skills/`, `~/.agents/skills/`.
- **MCP** extends the tool set via sandboxed server processes. Supports stdio,
  SSE, and HTTP transports.
- **A2A** enables agent-to-agent delegation. Rustain acts as both client (discover
  and delegate to remote agents) and server (accept tasks from external agents).

All layers are governed by one unified permission system with three modes:
Normal (prompt for each tool), YOLO (auto-approve safe tools), and Plan
(propose-then-execute).

## Project status & roadmap

**Status: v0.1-alpha, actively developed.** Currently in **Epic 12** — daemon
mode, multi-channel TUI, and cron scheduler. 12 epics completed to date covering
the full agent lifecycle from streaming chat through memory, skills, MCP,
sub-agents, context assembly, and daemon supervision.

| Phase | Milestone | Status |
|---|---|---|
| **v0.1** | Streaming chat, tool execution, permission system, session persistence | Shipped (Epics 1–4) |
| **v0.5** | Agent Skills, MCP, sub-agents, plan mode, multi-provider | Shipped (Epics 5–10) |
| **v0.75** | Memory tiers, context assembly, windowing, vector search | Shipped (Epic 11) |
| **v1.0** | Daemon mode, Telegram channel, cron scheduler, multi-client attach | In progress (Epic 12) |
| **v1.5** | A2A client, multi-agent orchestration | Planned (Epic 14) |
| **v2.0** | A2A server, dynamic adapter loading, community hub | Planned |

Detailed sprint tracking in
[`sprint-status.yaml`](../_bmad-output/implementation-artifacts/sprint-status.yaml).

## Contributing

Rustain is built under the **BMad Method** — planning artifacts (PRD,
architecture, epics, stories) drive implementation, and conformance tests
enforce architectural invariants on every change.

### Getting started

1. Clone and build:
   ```sh
   git clone https://github.com/lunarpulse/rustain.git
   cd rustain
   cargo build
   ```

2. Run the test suite:
   ```sh
   cargo test
   cargo clippy --all-targets
   cargo fmt --check
   ```

3. Read the architectural docs in [`docs/`](docs/) and the conformance test
   README in [`tests/CONFORMANCE_README.md`](tests/CONFORMANCE_README.md).

### Guidelines

- **Hexagonal discipline:** domain imports nothing external. Adapters import
  only from domain. `main.rs` is the sole composition root.
- **Conformance tests:** 47 tests enforce architectural invariants (dependency
  rules, async lock policy, namespace conventions). They must pass before merge.
- **Async lock policy:** `tokio::sync::RwLock` / `Mutex` only in adapters and
  infrastructure. Release write guards before `.await` points. Exceptions require
  explicit `CONFORMANCE_EXCEPTION_STD_SYNC_LOCK` tags.
- **Port-first design:** new capabilities start as a port trait in `domain/ports/`,
  with a noop stub and a conformance test, before any adapter is written.

## License

Licensed under the [MIT License](LICENSE).
