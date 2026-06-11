# dev_ws

A multi-project research workspace for building, studying, and evolving AI coding agents.
The workspace spans terminal-native agents, multi-channel assistants, orchestration platforms,
and protocol specifications — all under active development using the
[BMad Method](https://github.com/bmadcode/BMAD-METHOD) for sprint planning and story delivery.

## Workspace Map

### Original Projects

| Project | Stack | Description |
|---------|-------|-------------|
| [**rustain**](rustain/) | Rust (ratatui) | Fast, extensible, terminal-native AI coding agent with TUI, Agent Skills, MCP, and A2A protocol support. Hexagonal architecture. **Primary project** — currently in Epic 12. |
| [**rustain-a2a-protocol**](rustain-a2a-protocol/) | Rust | Standalone Rustain Agent Protocol (RAP) and A2A v1.0 compatibility adapter. Pre-implementation with full architecture spec and ADRs. |
| [**rustycode**](rustycode/) | Rust | AI coding assistant with hexagonal architecture; 9 built-in tools, 4 LLM providers (Anthropic, OpenAI, OpenRouter, Google Gemini). Production-ready CLI. |
| [**maos**](maos/) | Rust (40+ crates) | Multi-Agent Operating Substrate — a runtime for safe, auditable multi-agent systems. Founding-sprint scaffold with CI-enforced build discipline. |

### Reference Implementations (Third-Party)

Open-source agent projects cloned for study, feature comparison, and architectural reference.

| Project | Stack | Origin | Description |
|---------|-------|--------|-------------|
| [**ironclaw**](ironclaw/) | Rust (25+ crates) | [NEAR AI](https://github.com/nearai/ironclaw) | Secure personal AI assistant with WASM sandbox, multi-channel access (Telegram, Discord, Slack), dynamic tool building, persistent memory with hybrid search. |
| [**codex**](codex/) | JS / Rust | [OpenAI](https://github.com/openai/codex) | OpenAI Codex CLI — locally-run coding agent with ChatGPT plan integration and IDE plugin support. |
| [**gemini-cli**](gemini-cli/) | TypeScript | [Google](https://github.com/google-gemini/gemini-cli) | Open-source Gemini 3 AI agent for the terminal. Free tier (60 req/min), built-in tools, MCP support. |
| [**hermes-agent**](hermes-agent/) | Python / TS | [Nous Research](https://github.com/NousResearch/hermes-agent) | Self-improving AI agent with a built-in learning loop; autonomous skill creation, session search, multi-channel (Telegram, Discord, Slack, WhatsApp). |
| [**openclaw**](openclaw/) | TypeScript | [OpenClaw](https://github.com/openclaw/openclaw) | Personal AI assistant for 20+ channels (WhatsApp, Telegram, Signal, iMessage). Security-first, local data, gateway-based architecture. |
| [**opencode**](opencode/) | TS / Rust | [Anomaly](https://github.com/anomalyco/opencode) | Open-source AI coding agent with 20+ language docs, desktop app, clean TUI. |
| [**paperclip**](paperclip/) | TypeScript | [Paperclip AI](https://github.com/paperclipai/paperclip) | Open-source orchestration for AI agent companies. Coordinates teams toward shared goals with budget tracking, org charts, and task management. |
| [**oh-my-pi**](oh-my-pi/) | TS / Rust / Bun | [can1357](https://github.com/can1357/oh-my-pi) | Coding agent with the IDE wired in. 40+ providers, 32 built-in tools, 13 LSP ops, 27 DAP ops. |
| [**claudian**](claudian/) | TypeScript | — | Claude Code embedded in Obsidian sidebar. Full agentic capabilities (file I/O, bash, search), vision support, skills, MCP, plan mode. |
| [**KIMI**](KIMI/) | Python / Rust | [Moonshot AI](https://github.com/nichochar/kimi-cli) | Kimi Code CLI (Python) + Rust rewrite. Terminal AI agent with ACP support, IDE integration (Zed, JetBrains). |

### Support Directories

| Directory | Purpose |
|-----------|---------|
| `_bmad/` | BMad Method module — skills, scripts, templates, and configuration for the sprint-driven workflow. |
| `_bmad-output/` | Planning and implementation artifacts: PRD, architecture, epics, sprint status, research reports. |
| `docs/` | Project knowledge — Claudian TUI replication specs, architecture blueprints, implementation guides. |
| `evidences/` | Screenshots and visual evidence captured during development and debugging. |
| `graphify-out/` | Knowledge graph output (275k nodes, 1M edges) — auto-generated structural index of the workspace. |
| `.devcontainer/` | Dev container config — Rust, Python, PostgreSQL 15 with pgvector. |
| `agent/` | Kimi CLI runtime data (SQLite databases, sessions, config). Not a source project. |

## Architecture Themes

Patterns recurring across the original projects:

- **Hexagonal architecture** (ports & adapters) — `rustain`, `rustycode`, `maos`
- **Multi-provider LLM support** — Anthropic, OpenAI, Google, OpenRouter, Moonshot
- **MCP (Model Context Protocol)** — tool exposure and capability management
- **Terminal-first TUI** — ratatui-based rendering with streaming support
- **Persistent memory systems** — SQLite, vector search (fastembed + cosine), hybrid retrieval
- **Sprint-driven delivery** — BMad Method with epics, stories, acceptance criteria, and gates

## Development

### Prerequisites

- Rust (edition 2024) with cargo
- Node.js / Bun (for TypeScript reference projects)
- Python 3.x (for KIMI, hermes-agent, BMad scripts)
- Optional: VS Code with Dev Containers extension

### Dev Container

```bash
cp .devcontainer/.env_example .devcontainer/.env
# Reopen in Container via VS Code
```

The container provides Rust, Python, PostgreSQL 15 with pgvector, and all development tooling pre-configured.

### BMad Workflow

This workspace uses the [BMad Method](https://github.com/bmadcode/BMAD-METHOD) for planning and delivery.
Key commands (via Claude Code skills):

| Command | Purpose |
|---------|---------|
| `/bmad-help` | Get guidance on next steps |
| `/bmad-sprint-status` | View current sprint progress |
| `/bmad-create-story` | Create the next implementation story |
| `/bmad-dev-story` | Implement a story file |
| `/bmad-code-review` | Run adversarial code review |

### Knowledge Graph

The workspace includes a [graphify](https://github.com/nichochar/graphify) knowledge graph for structural navigation:

```bash
# Query relationships
graphify query "how does rustain's MCP adapter work?"
graphify path "TurnDriver" "ContextAssemblerPort"
graphify explain "WindowingAssembler"

# Update after code changes (AST-only, no API cost)
graphify update .
```

## Project Status

**rustain** is the primary active project, currently in **Epic 12** (daemon mode, multi-channel TUI, cron scheduler).
See [`_bmad-output/implementation-artifacts/sprint-status.yaml`](_bmad-output/implementation-artifacts/sprint-status.yaml) for detailed sprint tracking.

## License

Individual projects carry their own licenses. See each project's `LICENSE` file for details.
