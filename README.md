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
rustain auth status            # show configured providers, source, validation time
rustain auth status --json     # scriptable status envelope
rustain auth list              # list all providers, auth methods, signup URLs
rustain auth list --json       # structured provider catalog
rustain auth accounts openai   # stored accounts, the active one, what serves
rustain auth use openai --id 1 # make stored account #1 active
rustain auth logout openai --id 2  # remove one stored account
```

Env vars still win over `~/.rustain/auth.json`; `auth status` reports that
winning source without probing the network. `auth list` shows the full provider
catalog including unconfigured and keyless providers.

**Sessions** (conversation history):

```sh
rustain session list              # show saved sessions in current workspace
rustain session list --json       # structured output for scripts
rustain session list --all        # merge sessions across registered workspaces
rustain session list --all --json # same, with absolute workspace addresses

rustain session delete <id>       # delete by exact id or unique prefix
rustain session delete <id> --force --dry-run
rustain session delete --all      # delete all sessions in this workspace
rustain session delete --all-workspaces --force --dry-run
```

`session delete` is the first destructive CLI operation: it removes sessions
permanently with no trash/undo. It deletes by stable `id` only; the positional
`#` index from `list` is not accepted. The command is interactive by default;
use `--force` for scripts and `--dry-run` to preview what would be removed. An
in-use guard refuses to delete a session held by a running daemon (name and pid
are shown); `--force` skips prompts but does **not** bypass a confirmed holder.
For a guaranteed deletion, stop the daemon first with `rustain daemon stop`.

Sessions are stored per workspace under `.claude/sessions/`. `session list --all`
uses a hint registry at `~/.rustain/workspaces.json`, populated on the first
successful save per workspace. The `*` marker in the human table indicates the
session that a bare `rustain` command resumes by default (most recent in the
current workspace). The `id` column is the stable address; use it with
`rustain session delete <id>` instead of positional indexes. In JSON, every row
carries an always-present absolute `workspace` field.

For full semantics, guards, exit codes, and bulk-delete behavior see
[`docs/sessions.md`](docs/sessions.md).

### 🛠️ Develop Rustain

```sh
cargo build                # debug build
cargo check                # type check (fastest feedback)
cargo test                 # run the test suite (~255 test files)
cargo clippy               # lint
cargo fmt                  # format

# Feature-gated builds
cargo build --no-default-features --features anthropic     # minimal
cargo build --features telegram                            # with Telegram channel
cargo build --features vector-search                       # with local embeddings
cargo build --features a2a,p2p                             # federation from source
```

A2A and P2P are off by default for `cargo install`. **Published release assets up to and including `v0.1.3` do not contain them; `v0.2.0` and later do** — source builds need `--features a2a,p2p`. See [`docs/a2a.md`](docs/a2a.md) and [`docs/p2p.md`](docs/p2p.md).

### TUI peer-send quick start

For the headless twin, start a daemon in the configured workspace and run `rustain team send [--type <type>] [--subject <label>] [--stories <key>[=<alias>],…] <peer-id>[,<peer-id>…] <text…>`. It needs the daemon's free writer slot; flags precede peers and everything after peers is message text. See [the CLI send guide](docs/a2a.md#rustain-team-send).

1. Build the feature: `cargo build --features a2a`.
2. Configure a roster ID and URL in `.rustain/a2a.json`, for example `"reviewer": {"url": "http://127.0.0.1:8080"}` (the recipient started in the next step).
3. Start the recipient first; if it is rustain, use `rustain --serve-a2a=127.0.0.1:8080 daemon start`.
4. Start rustain in the configured workspace; peers are discovered at startup. ⚠ Discovery runs once at boot — a peer that is down when you start may be refused until you restart with it up (on-demand refresh is deferred; see `docs/a2a.md`).
5. In the TUI, run `/team send reviewer review this patch`; inspect delivery with `/team log`.

**Default features:** `anthropic`, `openai`, `ollama`, `clipboard`, `mcp`,
`meta-search`, `models-dev`.

The test suite includes 96 conformance tests that enforce architectural
invariants (dependency rules, lock policies, namespace conventions, port
contracts). The `ci.yml` lanes name the targets they run, so a conformance
file is only a guard once a lane lists it — see the Check job's explicit
enumeration. They are the primary guard against architectural drift.

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
- **Session management** — listing, deleting, in-use guard, exit codes:
  [`docs/sessions.md`](docs/sessions.md)
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
├── tests/                      # 255 test files inc. 96 conformance tests
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
read-only tool headers with a local tool inspector, vim-style navigation (`j`/`k`/`i`), command palette,
@-mention autocomplete, context meter with cache breakdown, plan mode overlay.
Long fenced code is capped on the Stage; focus a code block or table and press
`[Enter] Inspect` to open it in a block viewer (columns, line numbers, wrap,
rendered or source, copy of the exact source).
Text is laid out by terminal cells and grapheme clusters, not bytes or code
points: CJK text, emoji sequences (such as ZWJ families) and combining marks
wrap, clip, pad and edit as whole glyphs, and the composer's height, caret and
scrolling follow the same wrapped rows. Agent markdown renders as CommonMark
(nested lists, quotes, headings, literal HTML and escapes, code whitespace,
soft breaks as spaces) through one pipeline whose height always equals its
painted rows, and your own messages are shown exactly as typed. The supported
terminal floor is unchanged (60×16). Notices appear as short toasts that expire on their own
(errors within 60 s) and stay reviewable in the Notification Log
(`Ctrl+X n`, `/notifications`); a `Log !N` badge counts warnings and errors you
have not looked at yet. Prompts that need an answer wait in the Needs-you queue;
an automatically raised decision never steals the keyboard while you type.
`Ctrl+X q` opens the list (`Needs you (N)` in the status row), where you can
select a decision and open its frame. `Esc` defers it without deciding.

**Mission Control** (`Ctrl+X` then a tab key) is one window with a tab row over every
terminal size from 60×16: `Ctrl+X s` Agents, `t` Tasks, `l` Messages (the A2A log, both
local and attached sessions), `r` Room (also `/room`), `e` Artifacts; `Left`/`Right` switch
tabs, `j`/`k`, `Ctrl+D`/`Ctrl+U`, `PgUp`/`PgDn`, `g`/`G` scroll, `/` filters a list tab and
`Esc` goes back to exactly where you were. `Enter` on an agent, task or artifact row opens
its own window (transcript, task detail, artifact body; `c` copies the original). Attached
sessions show Messages only and answer the other keys with one remedy. The Room is read-only
replay ("as of" the last read, never live) and is re-checked once a second only while it is
the window you are looking at. History (`Ctrl+X h`), Adapters (`Ctrl+X a`) and Usage
(`Ctrl+X u`) are windows of the same kind; there is no sidebar and no density setting. On
terminals at least 120 columns wide `Ctrl+X w` toggles a read-only monitor column that paints
the current Mission Control tab beside the Stage (never a focus or key target). The generated
key tables are in `docs/keyboard.md`.

One muted **mission strip** row sits directly above the status row at every terminal size, in
local and attached sessions: `Agents 1/3 · Tasks 4/7 · Decisions 2` (running of total subagents,
finished of total tasks in the active conversation's plan, decisions waiting; `A 1/3 · T 4/7 · D 2`
on narrow terminals, a read-only mirror shows `Writer decisions N`, and an attached session shows
decisions only). Parts that do not apply are left out; when nothing applies it shows the key hint
that opens Mission Control (`^X s`, attached `^X l`). It has no focus and no keys, never sits
under a window or toast, and gives its row back to the Stage when keeping it would leave the
Stage under five rows (a long draft at 60×16).

**Tour.** `/tour` starts a two-minute walk through the five things you actually do: send a
message, inspect a tool call, answer a decision, open Mission Control (or the command palette)
and open help. Each card names the real key, and a step finishes only when you really do it;
`n` skips a step and `Esc` leaves. A team can have the tour offered on each launch until it
is finished (`[onboarding] offer_tour = true`, see `docs/configuration.md`).

Local sessions and `rustain daemon attach` share the composer, caret editing
(`Ctrl+A/E/K`, arrows and paste), transcript view state and window compositor.
Read-only attachments start in Input for local slash commands; ordinary text
still cannot start a model turn. Help, the palette, notifications and quit
confirmation share one keyboard-owning stack: `Esc` or `Ctrl+C` backs out one
window, and cancelling `Ctrl+Q` returns to the exact interrupted window.
Search `Esc` restores the pre-search position; Enter in navigation keeps the
match. `/` filters Help and the Notification Log without moving the transcript.
Windows leave the status row visible, fully covered `team-log` rows are not
counted as presented, and all input is inert below the 60×16 terminal floor.
Co-pending prompts retain their payloads, but only the keyboard owner paints
answer keys. Streaming indicators do not mark overwritten log rows as read.
Attached pending sends keep the draft's tail and caret visible; implicit resume
following preserves scroll intent, while an explicit bottom jump resets it.

In local Chat, `▸` marks the focused tool header; `[Enter] Inspect` appears only
when the complete hint fits. `Tab` cycles calls in its expanded multi-tool turn
(including when no turn was focused), bringing an off-screen header into view.
`Enter` opens Output, Input, Raw and, for a successful Edit/Write, Diff; `[`/`]`
step within the same turn without moving Stage focus. Inspector `/` edits a
case-insensitive logical-line search; Enter commits it and `n`/`N` move between
matches. While editing, `Esc` clears the search; after accepting, reopen `/`
before `Esc` to clear it, or press `Esc` directly to close the inspector.
Wrap defaults on; `w` toggles clipping with `→` on overflowing rows, keeping the
top logical line anchored. `c` copies the active tab's original source, not its
sanitized display or wrap markers (the live tail while Output is running).
Copies up to and including 8 MiB of UTF-8 source are exact; larger sources are
refused with `Copy failed`, never silently truncated. Empty source reports
`Nothing to copy`. Display sanitization removes terminal/bidi controls, not
secrets: tool arguments and results can contain sensitive data, and exact copy
preserves it. Copy confirmation remains in Notifications (`Ctrl+X n`), even
when the opaque inspector covers its toast; `Esc` returns to the sheet.
The tool inspector and its Stage hint are unavailable in attached sessions.

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

**Session management** — list sessions per workspace or across all registered
workspaces; delete by stable id with an in-use daemon guard, typed-count
confirmation for bulk deletes, `--dry-run`, and a versioned `--json` envelope.

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

**Subagent delegation** (`task` tool, `@Agents/<name>`, plan delegation) requires
the `composite` tools adapter, which only **`coding`** selects by default. `base`
and custom profiles that extend it compose a plain builtin tools adapter and will
**not** expose the `task` tool — `rustain doctor` reports this on the `Profiles`
line. To enable delegation in any profile, set `[tools] adapter = "composite"`.

## Four-layer extensibility

```
Layer -1: Ports          What the agent IS (27 port traits, composed by profiles)
Layer  0: Built-in tools What the agent ships with (bash, read, write, edit, glob, grep, web fetch)
Layer  1: Agent Skills   Markdown-based procedural knowledge (agentskills.io standard, 30+ tools)
Layer  2: MCP            Tool-level interop via external servers (stdio/Streamable HTTP)
Layer  3: A2A            Agent-to-agent delegation (localhost/LAN/internet)
```

- **Agent Skills** follow the open standard at agentskills.io — any skill from
  Claude Code, Codex, Gemini CLI, or Cursor works in Rustain without
  modification. Discovery paths: `.agents/skills/`, `.rustain/skills/`,
  `.claude/skills/`, `~/.agents/skills/`.
- **MCP** extends the tool set via sandboxed server processes and remote
  servers. Supports **stdio** and **Streamable HTTP**; legacy SSE is rejected
  per ADR-06-08 (the MCP spec deprecated it on 2025-03-26) — use a proxy such as
  `mcp-proxy`, or a server that speaks Streamable HTTP.
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
| **v1.5** | A2A client, P2P peer transport, multi-agent orchestration | Shipped (Epics 17–18) |
| **v2.0** | A2A server, dynamic adapter loading, community hub | A2A server shipped; dynamic loading and hub planned |

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
