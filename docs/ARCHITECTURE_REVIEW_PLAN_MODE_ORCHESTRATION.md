# Architecture Review: Plan Mode & Task Orchestration

## Cross-Codebase Analysis & Improvement Recommendations for Rustain

**Review Date:** 2026-04-23  
**Reference Codebases:** Codex (Rust), Kimi CLI (Python), Gemini CLI (TypeScript), OpenCode (TypeScript)  
**Reviewer:** Winston (System Architect persona)

---

## Executive Summary

Rustain's hexagonal foundation is solid — clean ports/adapters separation, a working event loop, and a permission system with four modes. But compared to the four reference implementations, rustain is missing **three critical architectural layers**:

1. **Plan Mode as a Workflow** — Rustain has `PermissionMode::Plan` but no plan file lifecycle, prompt injection, or approval handoff.
2. **Task/Subagent Runtime** — No background tasks, no subagent isolation, no child session model.
3. **Structured Tool Orchestration** — Tools execute sequentially in a raw loop; no state machine, no parallelization, no retry semantics.

This document breaks down what each reference does well, maps it to rustain's current gaps, and proposes **pragmatic, incremental improvements** that respect rustain's Rust-native, TUI-first identity.

---

## 1. Plan Mode: From Permission Flag to First-Class Workflow

### 1.1 What the References Do

| Tool | Plan Mode Approach | Key Insight |
|------|-------------------|-------------|
| **Codex** | Collaboration mode (`ModeKind::Plan`) + streaming `<proposed_plan>` XML parser + dedicated `PlanStreamController` + approval popup with "implement this / clear context / stay planning" | **Plan is a mode, not a tool.** The agent is *forbidden* from mutating actions. Plan blocks are extracted from the stream and rendered separately. |
| **Kimi CLI** | Dynamic injection of system reminders every N turns + dedicated plan file (`~/.kimi/plans/<hero-slug>.md`) + `ExitPlanMode` tool presents `QuestionRequest` with Approve/Reject/Revise options | **Injection over tool hiding.** Instead of dynamically removing tools from the schema (expensive), tools check `plan_mode_checker()` at call time. Plan file is the *only* writable path during plan mode. |
| **Gemini CLI** | Approval mode enum value + Policy Engine TOML rules (`plan.toml`) + `exit_plan_mode` tool triggers `ExitPlanModeDialog` | **Policy-driven gating.** Plan mode is just another approval mode. Rules are loaded from TOML and matched by priority. Model auto-switches from Pro (planning) to Flash (implementation). |
| **OpenCode** | Agent permissions — `plan` agent has `edit: { "*": "deny", ".opencode/plans/*.md": "allow" }` + synthetic `system-reminder` injection | **Agent-as-permission-carrier.** Plan mode is implemented through permission rules rather than a separate code path. Custom read-only agents are trivial to add. |

### 1.2 Rustain's Current State

- `PermissionMode::Plan` exists in `security_adapter.rs` (mode = 2)
- `/mode plan` command switches the mode
- **But:** No plan file path, no prompt injection, no plan approval flow, no handoff to implementation
- The permission chain (`domain/services/permission_chain.rs`) likely gates writes in Plan mode, but without a plan file target, the agent has nowhere to write its plan

### 1.3 Recommended Architecture

**Adopt Kimi CLI's approach** (fits rustain's Rust/TUI stack best) with OpenCode's agent-permissions flavor:

```
┌─────────────────────────────────────────────────────────────┐
│  Plan Mode Workflow                                         │
├─────────────────────────────────────────────────────────────┤
│  1. ENTER: /mode plan  →  set PermissionMode::Plan          │
│     - Allocate plan_session_id (UUID, survives restarts)    │
│     - Compute plan_file path: ~/.rustain/plans/<slug>.md    │
│     - Inject plan-mode system reminder into next turn       │
│                                                             │
│  2. EXECUTE: Agent streams, reads files, explores           │
│     - Permission chain denies all Write/Bash (except plan)  │
│     - Dynamic re-injection every 5 assistant turns          │
│     - Agent writes plan to <plan_file>                      │
│                                                             │
│  3. EXIT: Agent calls ExitPlanMode (or user /mode normal)   │
│     - Read plan file from disk                              │
│     - Present QuestionRequest dialog:                       │
│       [Approve] [Approve & AutoEdit] [Reject] [Revise]      │
│     - On Approve: inject synthetic user message:            │
│       "The plan at <path> has been approved. Execute it."   │
│     - Switch mode to Normal or AutoEdit                     │
└─────────────────────────────────────────────────────────────┘
```

**Key Components to Add:**

| Component | File Path | Description |
|-----------|-----------|-------------|
| `PlanManager` | `domain/services/plan_manager.rs` | Session-stable plan state, slug generation, file path resolution |
| `PlanModeInjector` | `domain/services/plan_mode_injector.rs` | Scans message history, injects reminders at turn boundaries |
| `ExitPlanMode` tool | `adapters/toolset_adapter.rs` | Tool callable only in Plan mode; triggers approval dialog |
| Plan approval UI | `adapters/tui/widgets/plan_approval.rs` | Dialog showing plan content with action buttons |

**Decision:** Use **hero-name slugs** (Kimi's pattern) over UUIDs — users can find plans in `~/.rustain/plans/` without `ls -la`. Rust crate `petname` generates these.

---

## 2. Task Orchestration: From Sequential Loop to Structured Runtime

### 2.1 What the References Do

| Tool | Orchestration Approach | Key Insight |
|------|----------------------|-------------|
| **Codex** | `ToolOrchestrator` — Approval → Sandbox Selection → Execution → Retry on Denial. Centralized pipeline with hook precedence. | **Layered approval stack:** Hooks → Guardian subagent → User modal. Each tool call is a pipeline, not a function call. |
| **Kimi CLI** | `BackgroundTaskManager` + `BackgroundTaskStore` (filesystem persistence) + `ApprovalRuntime` (pub/sub hub). Bash tasks spawn subprocess workers; agent tasks run async in same process. | **Filesystem-as-queue** for bash tasks makes them resilient to CLI restarts. `ContextVar` for approval source tracking propagates through `asyncio.create_task()` automatically. |
| **Gemini CLI** | `Scheduler` class with `CoreToolCallStatus` state machine: Validating → Scheduled → Executing → AwaitingApproval → Success/Error/Cancelled. Event-driven `MessageBus` for confirmations. | **State machine per tool call.** Batches parallelizable calls. Tail calls for sandbox expansion retries. |
| **OpenCode** | `Runner` pattern (Effect-TS) with `Idle → Running → Idle` and `Shell → ShellThenRun` states. Child sessions with `parentID` for subagent isolation. | **Child sessions as task isolation.** Each subagent gets its own SQLite session. Resumable via `task_id`. |

### 2.2 Rustain's Current State

- `run_turn()` in `infrastructure/runtime/turn.rs` — simple `loop { stream → collect → execute sequentially → loop }`
- Tool execution: `for tc in &tool_calls { ... execute one by one ... }`
- No background tasks, no subagents, no parallel execution
- `TurnQueue` exists for queuing user messages during streaming, but nothing for task orchestration

### 2.3 Recommended Architecture

**Phase 1: Introduce a ToolCall State Machine** (Gemini-inspired, fits rustain's event loop)

Replace the raw `for tc in &tool_calls` loop with a `ToolScheduler` that owns state:

```rust
// domain/services/tool_scheduler.rs
pub enum ToolCallStatus {
    Validating,        // Params built, awaiting policy/confirmation
    Scheduled,         // Cleared policy, ready to execute
    Executing,         // Tool running
    AwaitingApproval,  // Waiting for user
    Success,
    Error,
    Cancelled,
}

pub struct ToolCallState {
    pub id: String,
    pub status: ToolCallStatus,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub result: Option<ToolResult>,
    pub checkpoint: CheckpointId,
    // Gemini-style: track live output, PID, duration
}
```

**Phase 2: Parallelize Independent Tool Calls**

Codex and Gemini both parallelize by default. In rustain:

```rust
// Within a turn, group tool calls by dependency
// Batch 1: All reads/globs (parallel)
// Batch 2: Writes that depend on Batch 1 reads (sequential after)
```

For simplicity, start with **all tool calls in a single turn are parallelizable** (they have no intra-turn deps). Use `futures::future::join_all`:

```rust
let results = futures::future::join_all(
    tool_calls.into_iter().map(|tc| execute_with_approval(tc))
).await;
```

**Phase 3: Background Task Runtime** (Kimi-inspired)

```
┌─────────────────────────────────────────────────────────────┐
│  BackgroundTaskManager                                      │
├─────────────────────────────────────────────────────────────┤
│  - create_bash_task(command, timeout) → task_id             │
│  - create_agent_task(subagent_type, prompt) → task_id       │
│  - kill_task(task_id)                                       │
│  - list_tasks() → Vec<TaskSummary>                          │
├─────────────────────────────────────────────────────────────┤
│  Persistence: ~/.rustain/tasks/<task_id>/                   │
│    - spec.json    (immutable)                               │
│    - runtime.json (heartbeats, status)                      │
│    - control.json (kill signals)                            │
│    - output.log   (stdout/stderr or wire messages)          │
└─────────────────────────────────────────────────────────────┘
```

**New Tools:**
- `TaskList` — list background tasks
- `TaskOutput` — read output with offset-based paging
- `TaskStop` — request approval, then kill

**Phase 4: Subagent Runtime** (OpenCode-inspired)

```rust
// domain/services/subagent_runner.rs
pub async fn run_subagent(
    agent_type: &str,        // "explore", "coder", etc.
    prompt: &str,
    parent_conversation_id: &str,
) -> Result<SubagentResult, SubagentError> {
    // 1. Create child conversation with parentID link
    // 2. Inherit permissions from parent + agent config
    // 3. Run turn loop in isolated task
    // 4. Return summary + task_id
}
```

Child conversations are stored in the same `StoragePort` but with `parent_id` field. The TUI can render them as collapsible tree nodes under the parent tool call.

---

## 3. Approval System: From Oneshot to Pub/Sub Runtime

### 3.1 What the References Do

| Tool | Approval Architecture | Key Insight |
|------|----------------------|-------------|
| **Codex** | `ApprovalRequest` events with `call_id` + `approval_id` routing. Hook precedence over user. Caches `already_approved` for sandbox escalation retries. | **Approval ID routing** lets subcommands inherit parent approval without re-prompting. |
| **Kimi CLI** | `ApprovalRuntime` — pub/sub hub with `create_request()`, `wait_for_response()`, `resolve()`. `ContextVar` tracks source (foreground_turn / background_agent / subagent). Batch sweep on "Approve for session." | **Approval as a service.** Tools call `approval.request()` mid-execution; the runtime pauses the coroutine. ContextVar propagates through `asyncio.create_task()` automatically. |
| **Gemini CLI** | `MessageBus` event-driven: `TOOL_CONFIRMATION_REQUEST` → TUI renders → `TOOL_CONFIRMATION_RESPONSE`. Same core works across CLI TUI, IDE companion, and HTTP A2A. | **Event-driven over callback-driven.** Decouples core scheduler from all UIs. |
| **OpenCode** | `Deferred` (Effect-TS) + global bus. `Permission.ask()` evaluates rules → if "ask", creates Deferred, publishes event, awaits reply. | **Deferred-based async UI bridge.** Elegant bridging of terminal UI's event-driven nature with structured concurrency. |

### 3.2 Rustain's Current State

- `SecurityAdapter::request_permission()` uses oneshot channel directly
- TUI receives `AppEvent::PermissionRequest`, renders modal, sends back via `response_tx`
- SessionAllow and AlwaysAllow exist but no batch sweeping
- No approval source tracking — can't distinguish foreground vs background vs subagent
- No pub/sub — each approval is a point-to-point oneshot

### 3.3 Recommended Architecture

**Extract an `ApprovalRuntime`** (Kimi-style, adapted for Rust):

```rust
// domain/services/approval_runtime.rs
pub struct ApprovalRuntime {
    pending: Arc<RwLock<HashMap<String, ApprovalRequestRecord>>>,
    subscribers: Arc<RwLock<Vec<mpsc::UnboundedSender<ApprovalRuntimeEvent>>>>,
}

impl ApprovalRuntime {
    pub async fn create_request(&self, req: ApprovalRequest) -> String;
    pub async fn wait_for_response(&self, request_id: &str, timeout: Duration) -> Result<ApprovalResponse, ApprovalError>;
    pub async fn resolve(&self, request_id: &str, response: ApprovalResponse);
    pub async fn cancel_by_source(&self, source: ApprovalSource);
}
```

**Approval Source Tracking:**

```rust
pub enum ApprovalSource {
    ForegroundTurn { turn_id: String },
    BackgroundAgent { task_id: String, agent_id: Option<String>, subagent_type: Option<String> },
    ForegroundSubagent { agent_id: String },
}
```

Use `tokio::task_local!` (Rust's equivalent to Python's `ContextVar`) to track the current approval source through async task spawning:

```rust
tokio::task_local! {
    static CURRENT_APPROVAL_SOURCE: ApprovalSource;
}

// In BackgroundAgentRunner::run:
CURRENT_APPROVAL_SOURCE.scope(
    ApprovalSource::BackgroundAgent { task_id, agent_id, subagent_type },
    async { /* tool execution here inherits the source */ }
).await;
```

**Batch Sweep on SessionAllow:**

When user selects "Allow for this session" for `Bash(cargo test)`:
1. Add `Bash` to session-allowed set
2. Drain all queued pending requests for `Bash` and auto-approve them
3. Future `Bash` requests bypass UI entirely

---

## 4. Checkpointing & Restore: Beyond Snapshots

### 4.1 What the References Do

| Tool | Checkpoint Approach | Key Insight |
|------|--------------------|-------------|
| **Codex** | `TurnDiffTracker` per turn — `Arc<tokio::sync::Mutex<TurnDiffTracker>>` tracks file changes across multiple tool calls within a single turn. | **Unified diff view per turn.** Not just per-file snapshots, but a consolidated view of what changed in the turn. |
| **Gemini CLI** | Git snapshot in shadow repo (`~/.gemini/history/<project_hash>`) + JSON checkpoint file with conversation history + tool call args. `/restore` command checks out the snapshot and restores conversation. | **Git-based restore without polluting user's repo.** Shadow repo is separate from project repo. |
| **Kimi CLI** | Filesystem persistence for background tasks (spec/runtime/control/consumer/output). | **Resilience across process restarts.** A new CLI process can recover tasks from the previous session. |

### 4.2 Rustain's Current State

- `StoragePort::snapshot_file()` + `finalize_snapshot()` — per-file snapshots with SHA256 hashes
- `create_checkpoint()` returns a `CheckpointId`
- No git-based restore, no conversation history checkpointing, no `/restore` command

### 4.3 Recommended Architecture

**Add Git Shadow Checkpointing** (Gemini-style, but simpler):

```rust
// adapters/checkpoint_store.rs
pub struct CheckpointStore {
    shadow_repo_path: PathBuf,  // ~/.rustain/checkpoints/<project_hash>/
}

impl CheckpointStore {
    /// Before any file-modifying tool runs:
    pub async fn create_git_snapshot(&self, checkpoint_id: CheckpointId) -> Result<()>;
    
    /// Save conversation state + tool call args as JSON:
    pub async fn save_checkpoint_metadata(&self, checkpoint_id: CheckpointId, metadata: CheckpointMetadata) -> Result<()>;
    
    /// /restore command:
    pub async fn restore(&self, checkpoint_id: CheckpointId) -> Result<RestoreResult>;
}
```

**Why this matters for Plan Mode:** When a plan is approved and implementation starts, a checkpoint should be created *before* any edits. If implementation goes wrong, user can `/restore` to pre-implementation state.

---

## 5. Event Loop Decoupling: Taming the 6480-Line Beast

### 5.1 The Problem

`infrastructure/runtime/event_loop.rs` is **6,480 lines** — it handles:
- Terminal input conversion
- Message submission
- Permission handling
- Skill trust prompts
- Command execution (/new, /mode, /export)
- Retry logic
- Tab management
- Autocomplete and palette
- Feedback blocks
- Recovery prompts
- Rendering

This is a **single-responsibility violation** at architectural scale.

### 5.2 What the References Do

| Tool | Event Architecture | Key Insight |
|------|-------------------|-------------|
| **Codex** | App-Server Protocol (JSON-RPC) bridges core events to TUI. Core knows nothing about ratatui. | **Protocol boundary.** Core emits events; TUI consumes them. Clean separation enables IDE/HTTP clients. |
| **Kimi CLI** | `WireServer` (JSON-RPC over stdio) with `RootWireHub` for broadcast. | **Wire protocol as the only contract.** Background tasks, subagents, and TUI all speak the same protocol. |
| **Gemini CLI** | `MessageBus` — typed event bus. Core scheduler publishes; TUI, IDE companion, A2A server subscribe. | **Event-driven over callback-driven.** Same core works across all interfaces without modification. |
| **OpenCode** | Effect-TS `Runner` with `SynchronizedRef` for state transitions. Session loop is isolated from TUI. | **Structured concurrency.** Runner handles idle/running/shell state; TUI just renders. |

### 5.3 Recommended Architecture

**Extract three layers from the monolith:**

```
┌─────────────────────────────────────────────┐
│  TUI Layer (adapters/tui/)                  │
│  - render()                                 │
│  - convert_crossterm_event()                │
│  - widgets/                                 │
├─────────────────────────────────────────────┤
│  Wire Protocol Layer (infrastructure/wire/) │
│  - WireHub: broadcast queue for out-of-turn │
│  - WireMessage enum: all TUI↔Core messages  │
│  - JSON-RPC framing (future: IDE/HTTP)      │
├─────────────────────────────────────────────┤
│  Core Layer (infrastructure/core/)          │
│  - TurnExecutor: runs the agentic loop      │
│  - ToolScheduler: executes tool calls       │
│  - ApprovalRuntime: permission pub/sub      │
│  - BackgroundTaskManager: task lifecycle    │
└─────────────────────────────────────────────┘
```

**Immediate refactoring target:**

Split `event_loop.rs` into:

| Module | Responsibility | Lines Target |
|--------|---------------|--------------|
| `event_loop.rs` | `tokio::select!` branches only — route events to handlers | < 300 |
| `input_handler.rs` | Terminal event → `InputAction` mapping | ~400 |
| `turn_executor.rs` | `start_turn()`, retry logic, turn queue | ~500 |
| `permission_handler.rs` | Permission queue, batch sweep, feedback | ~300 |
| `command_handler.rs` | /new, /mode, /export, skill activation | ~400 |
| `render_engine.rs` | `render()` call and error handling | ~200 |

---

## 6. Implementation Roadmap

### Phase 1: Foundation (2–3 weeks)
- [ ] **ApprovalRuntime extraction** — pub/sub hub, `tokio::task_local!` source tracking
- [ ] **Event loop decoupling** — split 6480-line file into focused modules
- [ ] **ToolScheduler state machine** — `ToolCallStatus` enum, parallel execution for reads

### Phase 2: Plan Mode (2 weeks)
- [ ] **PlanManager** — plan file path resolution, hero-name slugs
- [ ] **PlanModeInjector** — dynamic reminder injection every N turns
- [ ] **ExitPlanMode tool + approval dialog** — plan content preview, Approve/Reject/Revise
- [ ] **Plan mode permission gating** — deny all Write/Bash except plan file

### Phase 3: Background Tasks (2–3 weeks)
- [ ] **BackgroundTaskManager** — filesystem persistence model
- [ ] **Bash background worker** — subprocess runner with heartbeat
- [ ] **TaskList/TaskOutput/TaskStop tools** — user-facing task management
- [ ] **Task browser TUI** — full-screen task list with output preview

### Phase 4: Subagents (3–4 weeks)
- [ ] **Child conversation model** — `parent_id` field, isolated message history
- [ ] **SubagentRunner** — foreground + background execution
- [ ] **Agent type definitions** — frontmatter extension for `allowed-tools`, `model`, `when_to_use`
- [ ] **Subagent event wrapping** — tree visualization in chat pane

### Phase 5: Checkpointing (1–2 weeks)
- [ ] **Git shadow repo** — `~/.rustain/checkpoints/<project_hash>/`
- [ ] **Checkpoint metadata** — conversation history + tool call args JSON
- [ ] **`/restore` command** — git checkout + conversation restoration

---

## 7. Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **Plan mode enforcement** | Dynamic injection + tool-time checks (Kimi) | Avoids re-describing tool schemas to LLM; simpler than agent-swapping |
| **Background task IPC** | Filesystem JSON files (Kimi) | Resilient to process restarts; no socket/pipe complexity |
| **Approval architecture** | Pub/sub runtime + task_local source (Kimi adapted) | Rust-native; propagates through `tokio::spawn` naturally |
| **Tool parallelization** | `join_all` within a turn (Gemini simplified) | All tools in a turn are independent; no dependency graph needed yet |
| **Subagent isolation** | Child conversations in same SQLite store (OpenCode) | Leverages existing `StoragePort`; minimal new infrastructure |
| **Checkpointing** | Git shadow repo (Gemini) | Battle-tested; `/restore` is a single `git checkout` |
| **Event loop refactor** | Wire protocol layer (Codex/Kimi hybrid) | Enables future IDE/HTTP clients without TUI rewrite |

---

## 8. What NOT to Adopt

| Pattern | Source | Why Skip |
|---------|--------|----------|
| **Streaming XML plan parser** | Codex | Overkill for rustain's scale. A simple regex/tag scanner on complete responses is sufficient. |
| **Guardian subagent** | Codex | Automated risk review adds latency and complexity. Rustain's permission chain + blocklist is enough for now. |
| **Sandbox escalation** | Codex | No sandbox infrastructure in rustain. Skip until container/namespace support is planned. |
| **Effect-TS Runner** | OpenCode | Rust has `tokio` and structured concurrency via `tokio::select!`. No need for an Effect runtime. |
| **Model routing by mode** | Gemini | Clever but adds provider complexity. Stick to user-configured model. |
| **A2A server wrapping** | Gemini | Future protocol, not needed for TUI-first product. |

---

## 9. Conclusion

Rustain has a **strong hexagonal foundation** but is missing the **runtime layers** that turn a chat loop into an agentic orchestrator. The four reference implementations converge on a common pattern:

> **Plan → Approve → Execute → Checkpoint → Restore**

Each tool adds a piece:
- **Codex** shows how plan mode becomes a first-class collaboration mode
- **Kimi CLI** shows how approval and tasks become pub/sub services
- **Gemini CLI** shows how policy engines and state machines bring structure
- **OpenCode** shows how agents and permissions compose into reusable modes

The recommended path is **incremental**: extract the approval runtime first (low risk, high payoff), then add plan mode workflow, then background tasks, then subagents. Each phase builds on the previous without rewriting the core.

Rustain's TUI-first, Rust-native identity is an asset — lean into `tokio`'s structured concurrency, use the type system for the tool call state machine, and keep the event loop decoupled. The result will be a tool that feels as fast as rustain currently is, but with the orchestration depth of the reference implementations.
