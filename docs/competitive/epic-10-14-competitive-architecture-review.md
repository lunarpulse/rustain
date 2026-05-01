# Competitive Architecture Review — Epics 10 & 14

**Date:** 2026-04-30
**Scope:** Epic 10 (Subagent Spawning & Ownership) and Epic 14 (A2A & Team Collaboration)
**Purpose:** Document findings from competitive analysis of six AI coding agent implementations, synthesize architectural recommendations for rustain, and capture open decisions for the long-term P2P decentralized workspace orchestrator.

**Sources analyzed:**

| Project | Language | Key Files Analyzed | Subagent Model | A2A Model | Google A2A Spec? |
|---------|----------|--------------------|----------------|-----------|-------------------|
| codex-rs | Rust | `core/src/agent/control.rs`, `core/src/agent/mailbox.rs`, `protocol/src/agent_path.rs` | Hierarchical actor tree, AgentPath addressing | Mailbox + InterAgentCommunication protocol | **No** — custom protocol |
| gemini-cli | TypeScript | `packages/core/src/agents/`, `packages/a2a-server/` | Subagents-as-tools (AgentTool pattern) | `@a2a-js/sdk` v0.3.11 — full spec compliance | **Yes** — client + server |
| opencode | TypeScript (Effect-TS) | `packages/opencode/src/tool/task.ts`, `src/session/` | Session-tree with parentID, Task tool | Result-based (parent result capture only) | **No** — ACP for clients only |
| KIMI | Rust | `kimi-agent-rs/src/soul/agent.rs`, `tools/multiagent/` | LaborMarket registry, fixed/dynamic split | Wire pub/sub + SubagentEvent envelope | **No** — custom Wire protocol |
| openclaw | TypeScript | `src/agents/subagent-spawn.ts`, `subagent-announce.ts`, `session-visibility.ts` | Gateway sessions, spawnDepth tracking | Announce flow + SessionsSend tool | **No** — proprietary gateway protocol |
| rustycode | Rust | `domain/src/sub_agent.rs`, `application/src/services/sub_agent_manager.rs` | Hexagonal ports: SubAgentRegistry + MessageQueue | InterToolMessage + InMemoryMessageQueue | **No** — domain ports only |

**Participants:** Winston (Architect), Mary (Business Analyst), Amelia (Developer), Barry (Quick Flow Solo Dev)

**Rustain baseline:** Hexagonal architecture in place. Agent discovery/activation works (persona switching). `ApprovalSource` already has `ForegroundSubagent` and `BackgroundAgent` variants. `AgentCore` is a 2-line placeholder (`src/infrastructure/runtime/agent_core.rs:1-3`). No subagent runtime, no child conversations, no A2A protocol. `CapabilityProvider` trait described in CLAUDE.md but not implemented.

**Key finding (A2A protocol):** The term "A2A" in the agent ecosystem refers to the **Google/Linux Foundation Agent-to-Agent protocol v0.3.0** — an open standard (Apache-2.0, LF-governed) with a TypeScript reference SDK (`@a2a-js/sdk`). Among six competitors surveyed, **only gemini-cli implements the actual A2A spec.** The other five use custom protocols under the "A2A" name. **No Rust implementation of the A2A spec exists.** This is a first-mover opportunity for rustain to ship `rustain-a2a-protocol` as the reference Rust A2A crate. See section 0 for full analysis.

---

## 0. The Google A2A Protocol — Standard or Branding?

Before analyzing the competitors' A2A approaches, we must clarify what "A2A" actually means — because nearly every competitor uses the term, but only one implements the actual open standard.

### 0.1 The Real A2A Protocol

Google's Agent-to-Agent (A2A) protocol is **a real, open standard** — not a branding exercise. Key facts:

| Attribute | Detail |
|-----------|--------|
| **Governance** | **Linux Foundation** (not Google) |
| **License** | Apache-2.0 |
| **Spec version** | `0.3.0` (pre-1.0, actively developed) |
| **Reference SDK** | `@a2a-js/sdk` v0.3.11 (TypeScript, npm) |
| **Author** | Google (original), now LF community |
| **Transport spec** | Agent card negotiation of transport bindings |
| **Message format** | Typed union with `kind` discriminator, role-based |
| **Discovery** | `/.well-known/agent-card.json` endpoint |
| **Security** | Bearer, Basic, OAuth2, API key providers |

The protocol defines:

**AgentCard** — a self-describing capability document:
```json
{
  "name": "Agent Name",
  "description": "What this agent does",
  "url": "http://host:port/",
  "protocolVersion": "0.3.0",
  "version": "0.0.1",
  "capabilities": { "streaming": true, "pushNotifications": false, "stateTransitionHistory": true },
  "skills": [{ "id": "...", "name": "...", "description": "...", "tags": ["..."], "examples": ["..."] }],
  "additionalInterfaces": [{ "transport": "JSONRPC", "url": "..." }],
  "securitySchemes": { "bearerAuth": { "scheme": "bearer" } },
  "defaultInputModes": ["text"],
  "defaultOutputModes": ["text"]
}
```

**Task lifecycle** (state machine):
```
submitted → working → completed / failed / canceled / rejected / auth-required
                    → input-required (awaiting human input)
```

**Supported transports** (all three shipping in the reference SDK):
| Transport | Wire format | Bidirectional |
|-----------|-------------|---------------|
| **REST** | HTTP request/response | Client → Server |
| **JSON-RPC** | JSON-RPC over HTTP | Bidirectional |
| **gRPC** | Protobuf over HTTP/2 | Bidirectional |

**Streaming events:** `status-update` (task state transitions), `artifact-update` (incremental result chunks), with `final`/`lastChunk` flags for completion signaling.

**Extension system:** The spec supports protocol extensions that layer additional schemas. Gemini-cli ships a `development-tool` extension for code-generation-specific agent interaction (tool call lifecycle, confirmation flow, thought streaming).

### 0.2 Competitor Compliance Reality Check

**Only one competitor actually implements the Google/LF A2A spec:**

```
┌─────────────────────────────────────────────────────────────┐
│ Google A2A Protocol (Linux Foundation, v0.3.0)              │
│                                                             │
│ Reference SDK: @a2a-js/sdk v0.3.11                          │
│                                                             │
│ Adopters in this workspace:                                 │
│   ✅ gemini-cli — FULL (client + server, 3 transports)     │
│   ❌ codex-rs — custom InterAgentCommunication protocol     │
│   ❌ opencode — ACP (client protocol, not A2A)              │
│   ❌ KIMI — custom Wire pub/sub protocol                    │
│   ❌ openclaw — proprietary Gateway protocol                │
│   ❌ rustycode — domain ports only, no network protocol     │
│                                                             │
│ Rust implementations:                                       │
│   ❌ NONE exist — no crate implements the A2A spec in Rust  │
└─────────────────────────────────────────────────────────────┘
```

### 0.3 Relevance to Rustain

**Decision: rustain SHOULD adopt the Google/LF A2A protocol, not build its own.** Here's why:

**Arguments for adoption:**

1. **It's an open standard under the Linux Foundation.** This isn't a Google land-grab — governance has been transferred to a neutral foundation. The Apache-2.0 license removes intellectual property risk.

2. **Interoperability is the whole point.** If rustain builds a custom A2A protocol (like codex-rs, openclaw, and KIMI all did), it can only talk to other rustain instances. If rustain implements the A2A spec, it can interoperate with gemini-cli agents, any future A2A-compliant agent in any language, and A2A tooling/debugging infrastructure. For a P2P decentralized orchestrator, interoperability is non-negotiable.

3. **No Rust implementation exists — first-mover advantage.** Writing a Rust port of the A2A protocol (`rustain-a2a-protocol` crate) would make rustain the **reference Rust implementation**. This attracts ecosystem contributors and positions rustain as the standard-bearer for A2A in the Rust agent community.

4. **The spec is small enough to be portable.** The core A2A spec (AgentCard, Task, Message, streaming events) is ~2000 lines of TypeScript type definitions. A Rust port with `serde` + `prost` (for gRPC) is an estimated ~800 LOC of domain types plus ~600 LOC of transport adapters.

5. **AgentCard maps naturally to our `AgentDiscovery` port.** The A2A AgentCard *is* our `AgentCard` value object. The `/.well-known/agent-card.json` endpoint is the `AgentDiscovery::register()` adapter. We aren't inventing new concepts — we're implementing existing ones.

**Arguments against (and rebuttals):**

| Concern | Rebuttal |
|---------|----------|
| "It's version 0.3 — unstable" | The core types (Task, Message, AgentCard) have been stable across 0.2 → 0.3. The unstable parts are extensions, which are opt-in. |
| "It's designed by Google — they'll steer it" | LF governance means Google can't unilaterally change the spec. And rustain implementing it gives rustain a seat at the table. |
| "Our P2P needs are different from their client-server model" | The A2A spec is transport-agnostic. The Task lifecycle and message formats work identically over libp2p as over HTTP. The spec doesn't mandate client-server — it defines peer-to-peer agent interaction. |
| "We'll be tied to their release cycle" | No more than we're tied to any open standard. And we own our Rust implementation — we can maintain compatibility shims. |

### 0.4 Strategic Recommendation

**Adopt the Google/LF A2A protocol as rustain's A2A wire format.** This means:

1. **Phase 1 (now):** Define `AgentCard`, `Task`, `Message`, `Part`, `TaskState`, `Artifact` as domain value objects (`src/domain/models/a2a.rs`). These match the A2A spec but are protocol-versioned so the domain isn't tightly coupled to spec version churn.

2. **Phase 2 (Epic 14):** Implement `A2aTransport` port with three adapters:
   - `JsonRpcTransport` — JSON-RPC over HTTP (matches A2A spec)
   - `InProcessTransport` — using existing event bus for same-process agents
   - `GrpcTransport` — deferred to v2.5

3. **Phase 3 (P2P, v3.0):** Add `Libp2pTransport` — the same A2A messages over libp2p request/response and GossipSub. The A2A spec's transport-agnostic design makes this a pure adapter addition — zero domain changes.

4. **Bonus:** Ship a standalone `rustain-a2a-protocol` crate containing the A2A types + `serde` derives — usable by any Rust project wanting to speak A2A. This positions rustain as the go-to Rust A2A implementation.

**What we do NOT adopt from gemini-cli:**
- The `development-tool` extension (Google/Gemini-specific)
- The Express server (rustain uses a different server stack)
- The `TaskStore` abstraction (rustain has its own storage ports)
- The OAuth provider chain (deferred to when enterprise features are needed)

**What we DO adopt from the A2A spec:**
- AgentCard format (for `AgentDiscovery::register()`)
- Task state machine (for `AgentStatus` — it's a superset of our 5-state FSM)
- Message format (for `AgentMessageBus::send()` envelope)
- Streaming event model (for `AgentMessageBus::subscribe()`)
- Transport-agnostic adapter design
- `/.well-known/agent-card.json` discovery endpoint

### 0.5 Task State Machine — A2A Compatibility

The A2A Task state machine is a **superset** of rustycode's AgentStatus FSM. Here's the mapping:

```
rustain AgentStatus (Epic 10)     A2A TaskState (Epic 14)
─────────────────────────────     ──────────────────────
Spawning          ───────────►    submitted
Active             ───────────►    working
                                  input-required  ← NEW (blocked on human input)
Completed         ───────────►    completed
Failed             ───────────►    failed
Terminated         ───────────►    canceled
                                  rejected         ← NEW (authorization denied)
                                  auth-required    ← NEW (need credentials)
```

This means rustain's `AgentStatus` should be defined as the minimal subset we need now, with `#[non_exhaustive]` to allow adding A2A states later without breaking changes. The `AgentMessageBus` port's envelope always carries the full A2A state — downstream consumers (TUI, P2P peers, A2A clients) get the complete picture.

---

## 1. Epic 10 — Subagent Spawning & Ownership

### 1.1 Requirements Covered

| FR | Description | Key Architectural Concern |
|----|-------------|--------------------------|
| 10.1 | Define agent subagent profiles (YAML, Markdown) | `AgentDef` extension for subagent config — tools, model, sandbox policy |
| 10.2 | Spawn subagent from parent agent | `SubAgentRegistry` port trait with spawn lifecycle |
| 10.3 | Subagent context isolation (forked vs fresh) | `ContextPolicy` enum: `Isolated | Forked(limit) | Inherited` |
| 10.4 | Subagent panel/UI in TUI | Tree view widget, reusing `task_panel.rs` pattern |
| 10.5 | Subagent lifecycle management (status, terminate, cascade) | `AgentStatus` state machine, `SubAgentWorker` |
| 10.6 | Ownership topology (Owned/Peer/Self) | Capability-based ownership model from CLAUDE.md:110-115 |
| 10.7 | Config inheritance from parent to child | Permission rule propagation, model/model-override resolution |

### 1.2 Competitor Patterns

Three fundamental subagent spawning models emerged:

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Subagent Spawning Architectures                   │
├──────────────────┬───────────────────────┬──────────────────────────┤
│ Model 1: Session │ Model 2: Tool-as-     │ Model 3: Registry        │
│ Tree + Task Tool │ Agent (gemini-cli,    │ (KIMI, rustycode)        │
│ (codex, opencode)│ openclaw)             │                          │
│                  │                       │                          │
│ Subagent = child │ Subagent = tool call  │ Subagent = registered    │
│ session in tree  │ with isolated context │ entity in a pool         │
│                  │                       │                          │
│ Natural hierarchy│ Couples agent to tool │ Cleanest separation of   │
│ Implicit parent  │ abstraction           │ discovery from spawning  │
│ through tree     │                       │                          │
├──────────────────┴───────────────────────┴──────────────────────────┤
│ Recommendation for rustain: Model 3 foundation                      │
│ (registry + hexagonal ports) with Model 1's parent-child tree       │
│ for conversation lineage                                            │
└─────────────────────────────────────────────────────────────────────┘
```

### 1.3 Key Findings Per Competitor

#### codex-rs — AgentPath + V2 Message Model (Take both)

codex-rs implements two generations of multi-agent tools:

**V1 (stable):** `spawn_agent`, `send_input`, `resume_agent`, `close_agent`, `wait_agent`
**V2 (development):** `spawn_agent`, `send_message`, `followup_task`, `close_agent`, `wait`, `list_agents`

The V2 migration replaces raw item-sending (`send_input`) with semantically-rich messaging (`send_message`/`followup_task`). This is the correct evolution: raw conversation items leak the parent's context model into the child.

**AgentPath** (`/root/researcher/analyst`) is the standout contribution. It is:
- A hierarchical, filesystem-like address that forms a DAG
- Resolvable relative to the caller (`../sibling` or absolute `/root/child`)
- Maps directly to P2P routing keys: `peer_id/root/researcher`

```rust
// codex-rs AgentPath (simplified)
pub struct AgentPath {
    segments: Vec<String>,  // ["root", "researcher", "analyst"]
}
impl AgentPath {
    fn parent(&self) -> Option<AgentPath>;
    fn join(&self, child: &str) -> AgentPath;
    fn resolve(&self, relative: &str) -> AgentPath;
}
```

**InterAgentCommunication** protocol message (codex-rs `protocol/src/protocol.rs:718-765`):
```rust
pub struct InterAgentCommunication {
    pub author: AgentPath,
    pub recipient: AgentPath,
    pub content: String,
    pub trigger_turn: bool,
}
```

**Verdict:** Adopt AgentPath as the universal agent identifier. Adopt the V2 message semantics (send_message/followup_task) over raw item-passing. Reject the actor model — actors are stateful and hard to distribute.

#### gemini-cli — Subagents-as-Tools + CompleteTaskTool (Reject the coupling, adopt the termination protocol)

gemini-cli's `AgentTool` pattern makes every subagent a tool callable by the parent. This has advantages (model natively understands when to delegate) but a fatal flaw for rustain's future: it couples agent spawning to the tool abstraction, making agents second-class citizens.

**The CompleteTaskTool** (`packages/core/src/tools/complete-task.ts`) is worth adopting independently: every subagent must explicitly signal completion by calling `complete_task`, which validates output against the agent's `OutputConfig` schema. This replaces implicit "the agent stopped talking" detection with an explicit protocol.

**Grace period recovery:** When subagents hit limits (timeout, max turns), gemini-cli gives one final turn to submit best-so-far results. This is a pragmatic UX pattern rustain should adopt.

**Verdict:** Reject the tool-as-agent pattern. Adopt CompleteTaskTool as an explicit termination protocol. Adopt grace period recovery for limit-exceeded subagents.

#### opencode — Permission Rule Inheritance (Take this)

opencode's `task.ts:60-97` shows how permission rules propagate from parent to child:

```
Parent permissions
    → Child's Agent.Info.permission rules evaluated
    → If child lacks `todowrite` → inject deny rule
    → If child lacks `task` → inject deny rule (prevents recursive spawning)
    → experimental.primary_tools → forced allow
    → Merged ruleset stored on child session
```

This is capability delegation done correctly: the parent grants a *subset* of its permissions to the child. For rustain's P2P future, this becomes chainable capability tokens.

**Verdict:** Adopt permission rule inheritance. Extend to capability delegation for P2P.

#### KIMI — Fixed vs Dynamic Subagents + Runtime Forking (Take both)

KIMI distinguishes two agent types with different lifecycle policies:

| Type | Creation | Isolation | LaborMarket |
|------|----------|-----------|-------------|
| **Fixed** | Loaded from YAML at startup | Fresh LaborMarket (can't see siblings) | Registered in parent's `fixed_subagents` |
| **Dynamic** | Created via `CreateSubagent` tool at runtime | Shared LaborMarket (can see other dynamics) | Registered in parent's `dynamic_subagents` |

The `Runtime::copy_for_fixed_subagent()` vs `copy_for_dynamic_subagent()` distinction controls what state is shared vs isolated:

```rust
// Fixed subagent: completely fresh context
fn copy_for_fixed_subagent(&self) -> Runtime {
    Runtime {
        labor_market: Arc::new(Mutex::new(LaborMarket::new())),  // NEW empty
        denwa_renji: Arc::new(Mutex::new(DenwaRenji::new())),    // Fresh D-Mail
        approval: Arc::new(self.approval.share()),                // Shared state, separate queue
        // ... same config, llm, session, environment, skills
    }
}

// Dynamic subagent: shares parent's market
fn copy_for_dynamic_subagent(&self) -> Runtime {
    Runtime {
        labor_market: Arc::clone(&self.labor_market),  // SHARED
        // ... rest same
    }
}
```

**Verdict:** Adopt fixed/dynamic distinction. Fixed = workspace infrastructure agents (always-on). Dynamic = task-specific ephemeral agents. Map the runtime forking to rustain's `SandboxPolicy` (`src/domain/models/sandbox.rs`).

#### openclaw — Spawn Lifecycle + Subagent Registry (Take the phases, reject centralized gateway)

openclaw's `spawnSubagentDirect()` (`subagent-spawn.ts:626-1255`) defines the most comprehensive spawn lifecycle:

1. **Validation** — agentId format, spawn depth, sandbox compatibility, child count limits
2. **Session Setup** — child session key, capabilities resolution, context mode (`isolated` or `fork`), thread binding
3. **Agent Start** — embedded agent run with `buildSubagentSystemPrompt`
4. **Post-Spawn** — registry registration, lifecycle hooks, session event emission

The subagent registry (`subagent-registry.ts`) persists run records with `spawnedBy` parent links, timing metadata, and `frozenResultText` for completed subagents.

**Verdict:** Adopt the 4-phase spawn lifecycle as application-layer orchestration. Reject centralized gateway — the registry must be a swappable port.

#### rustycode — Hexagonal Ports + State Machine (Adopt as structural reference)

rustycode is the only competitor to treat subagent management as a **domain concern** with formal port traits:

```rust
// domain/src/sub_agent.rs
#[async_trait]
pub trait SubAgentRegistry: Send + Sync {
    async fn register(&self, agent: SubAgent) -> Result<()>;
    async fn unregister(&self, agent_id: SubAgentId) -> Result<()>;
    async fn get(&self, agent_id: SubAgentId) -> Result<Option<SubAgent>>;
    async fn update_status(&self, agent_id: SubAgentId, status: AgentStatus) -> Result<()>;
    async fn get_all_for_session(&self, session_id: SessionId) -> Result<Vec<SubAgent>>;
    async fn register_with_limit(&self, agent: SubAgent, max_agents: usize) -> Result<()>;
}

#[async_trait]
pub trait SubAgentMessageQueue: Send + Sync {
    async fn send(&self, message: SubAgentMessage) -> Result<()>;
    async fn receive(&self, agent_id: SubAgentId) -> Result<Vec<SubAgentMessage>>;
    async fn peek(&self, agent_id: SubAgentId) -> Result<Option<SubAgentMessage>>;
    async fn clear(&self, agent_id: SubAgentId) -> Result<()>;
}
```

**AgentStatus state machine** (5 states, exactly what rustain needs):
```
Spawning → Active → Completed / Failed / Terminated
```

The `register_with_limit()` method uses `AtomicUsize` for race-condition-free spawning with limits. Cascade termination (`terminate_sub_agent.rs:55-88`) recursively terminates child agents using depth-first traversal of the parent-child tree.

**Critique:** The `SubAgentWorker` with polling + deduplication (`sub_agent_worker.rs:105-146`) is a leaky abstraction. The domain should not know about polling loops. Replace with event-driven push via the message bus — KIMI's pub/sub pattern is the reference.

**Critique:** The `InMemoryMessageQueue` with FIFO `VecDeque` per agent is correct in-process, but the port trait is point-to-point only. A P2P system needs broadcast semantics. Extend with a `broadcast(scope: VisibilityScope, message)` method.

**Verdict:** Adopt rustycode's port traits and state machine as the structural template. Replace polling with push. Extend message queue with broadcast semantics.

### 1.4 Recommended Architecture for Epic 10

```mermaid
graph TD
    subgraph "Domain Layer (pure, no I/O)"
        AP[AgentPath<br/>hierarchical address<br/>from codex-rs]
        AS[AgentStatus<br/>Spawning → Active →<br/>Completed / Failed / Terminated<br/>from rustycode]
        SD[SubAgentDescriptor<br/>name, tools, model, sandbox<br/>fixed vs dynamic: from KIMI]
        CP[ContextPolicy<br/>Isolated | Forked(limit) | Inherited<br/>from KIMI + openclaw]
        SS[SpawnSpec<br/>parent, descriptor, context,<br/>capabilities, ownership]
    end

    subgraph "Ports (trait definitions)"
        SAR[SubAgentRegistry<br/>spawn, lookup, terminate, watch<br/>from rustycode]
        SMQ[SubAgentMessageQueue<br/>send, receive, peek,<br/>broadcast, clear<br/>extended from rustycode]
        AD[AgentDiscovery<br/>discover, register<br/><i>new — Mary's 3-port split</i><br/>from KIMI + openclaw]
    end

    subgraph "Application Layer"
        SM[SubAgentManager<br/>spawn orchestration<br/>4-phase lifecycle<br/>from openclaw]
        SW[SubAgentWorker<br/>event-driven executor<br/>NOT polling-based]
        CT[CompleteTaskTool<br/>explicit termination<br/>from gemini-cli]
    end

    subgraph "Adapters (I/O)"
        IMR[InMemoryRegistry<br/>RwLock + AtomicUsize<br/>from rustycode]
        IMB[InMemoryMessageBus<br/>broadcast per scope<br/>extended for P2P readiness]
        FSB[FilesystemContextStore<br/>isolated JSONL per subagent<br/>from KIMI]
    end

    SM -->|uses| SAR
    SM -->|uses| SMQ
    SW -->|uses| SAR
    SW -->|uses| SMQ
    SAR -->|implemented by| IMR
    SMQ -->|implemented by| IMB
    SM -->|uses| FSB
```

**New domain types:**

| Type | Location | Purpose |
|------|----------|---------|
| `AgentPath` | `src/domain/models/agent_path.rs` (new) | Hierarchical agent address (from codex-rs) |
| `SubAgentDescriptor` | `src/domain/models/agent.rs` (extend) | Agent config for spawning: tools, model, sandbox, type (fixed/dynamic) |
| `ContextPolicy` | `src/domain/models/conversation.rs` (extend) | Forking strategy: Isolated, Forked(u64 limit), Inherited |
| `SpawnSpec` | `src/domain/models/agent.rs` (extend) | Spawn parameters: parent, descriptor, context, capabilities |
| `AgentOwnership` | `src/domain/models/agent.rs` (new) | Enum: Owned, Peer, Self (from CLAUDE.md:110-115) |

**New ports:**

| Port | Location | Methods |
|------|----------|---------|
| `SubAgentRegistry` | `src/domain/ports/sub_agent_registry.rs` (new) | `spawn(SpawnSpec) → AgentHandle`, `lookup(AgentPath) → AgentHandle`, `terminate(AgentPath, cascade)`, `watch(AgentPath) → Stream<AgentStatus>` |
| `SubAgentMessageBus` | `src/domain/ports/message_bus.rs` (new) | `send(from, to, msg)`, `broadcast(from, scope, msg)`, `subscribe(scope) → Stream<Envelope>` |
| `AgentDiscovery` | `src/domain/ports/agent_discovery.rs` (new) | `discover(scope, capabilities) → Vec<AgentCard>`, `register(AgentCard)` |

**Note:** Mary's recommended 3-port split (Spawning / Communication / Discovery) is adopted. Each port has a single responsibility and can be swapped to P2P adapters independently.

### 1.5 File Map

```
src/domain/
  models/
    agent_path.rs          ← NEW: AgentPath, address resolution
    agent.rs               ← EXTEND: SubAgentDescriptor, SpawnSpec, AgentOwnership
    conversation.rs        ← EXTEND: parent_id field, ContextPolicy enum
  ports/
    sub_agent_registry.rs  ← NEW: SubAgentRegistry trait
    message_bus.rs         ← NEW: SubAgentMessageBus trait
    agent_discovery.rs     ← NEW: AgentDiscovery trait
  services/
    sub_agent_manager.rs   ← NEW: spawn orchestration (4-phase lifecycle)
    complete_task.rs       ← NEW: CompleteTaskTool domain logic

src/application/
  subagent/
    spawn/
      service.rs           ← NEW: SpawnService (orchestration)
      context_resolve.rs   ← NEW: ContextPolicy resolution
    lifecycle/
      worker.rs            ← NEW: event-driven SubAgentWorker
      termination.rs       ← NEW: cascade termination
    messaging/
      router.rs            ← NEW: AgentMessageRouter (Mediator)

src/adapters/
  subagent/
    registry/
      in_memory.rs         ← NEW: InMemorySubAgentRegistry (RwLock + AtomicUsize)
    messaging/
      in_memory.rs         ← NEW: InMemoryMessageBus (broadcast per scope)
      envelope.rs          ← NEW: AgentMessage envelope (from AgentPath, to AgentPath, content, signature)
    context/
      filesystem.rs        ← NEW: FilesystemContextStore (JSONL per subagent)

src/infrastructure/
  runtime/
    agent_core.rs          ← REPLACE: 2-line placeholder → full AgentCore implementation
```

---

## 2. Epic 14 — A2A & Team Collaboration

### 2.1 Requirements Covered

| FR | Description | Key Architectural Concern |
|----|-------------|--------------------------|
| 14.1 | A2A messaging between agents | `AgentMessageBus` port with transport-agnostic routing |
| 14.2 | Cross-workspace agent discovery | `AgentDiscovery` port with `AgentCard` registration |
| 14.3 | Team definition and membership | `Team` entity with hierarchical ownership |
| 14.4 | A2A protocol (JSON-RPC/gRPC/libp2p) | `A2aTransport` port with multi-transport adapters |
| 14.5 | Capability delegation | `CapabilityProvider` with chainable tokens |
| 14.6 | A2A configuration (`.rustain/a2a.json`) | `A2aConfig` loading from TOML |

### 2.2 Competitor Patterns

Three communication models exist, and rustain needs all three at different layers:

```mermaid
graph TD
    subgraph "Layer 1: Discovery & Events — Pub/Sub"
        G1[GossipSub<br/>Team broadcasts<br/>Agent join/leave]
        G2[from KIMI's Wire]
    end

    subgraph "Layer 2: Directed Messaging — Mailbox"
        M1[DHT-routed<br/>Point-to-point<br/>Task delegation]
        M2[from codex-rs's AgentPath + mailbox]
    end

    subgraph "Layer 3: High-bandwidth — Direct RPC"
        R1[libp2p req/resp<br/>Streaming results<br/>File transfer]
        R2[from gemini-cli's multi-transport]
    end

    G1 --> M1
    M1 --> R1
```

| Layer | Pattern | Used for | Competitor reference |
|-------|---------|----------|---------------------|
| **Discovery & Events** | Pub/Sub (GossipSub) | Agent join/leave, team formation, status broadcasts | KIMI's Wire |
| **Directed Messaging** | Mailbox (DHT-routed) | Task delegation, result delivery, capability negotiation | codex-rs's AgentPath + InterAgentCommunication |
| **High-bandwidth** | Direct RPC | Streaming generation results, file sync | gemini-cli's multi-transport SDK |

### 2.3 Key Findings Per Competitor

#### codex-rs — InterAgentCommunication Protocol (Take the protocol message, reject the transport)

codex-rs's `InterAgentCommunication` protocol message (`protocol.rs:718-765`) is the right envelope format:

```rust
pub struct InterAgentCommunication {
    pub author: AgentPath,
    pub recipient: AgentPath,
    pub other_recipients: Vec<AgentPath>,
    pub content: String,
    pub trigger_turn: bool,
}
```

It captures: who sent it, who it's for, what it contains, and whether the recipient should be woken up. This is a protocol-level concern, not a transport concern.

**The gap:** No signed envelope. For P2P, the message must be cryptographically signed. Extend to:

```rust
pub struct AgentEnvelope {
    pub header: EnvelopeHeader { author, recipient, content_hash, nonce, ttl },
    pub body: AgentMessage,
    pub signature: Ed25519Signature,  // or libp2p Noise-signed
}
```

**Verdict:** Adopt the protocol message structure. Add cryptographic envelope for P2P.

#### gemini-cli — Multi-Transport A2A SDK + The Only Spec-Compliant A2A (Adopt the protocol)

gemini-cli's `@a2a-js/sdk` v0.3.11 is the **reference implementation of the Linux Foundation's A2A protocol**. It is the only competitor in this survey that implements the actual open standard rather than a custom protocol. Three transports:

| Transport | Use case | Rustain equivalent |
|-----------|----------|-------------------|
| **REST** | Simple request/response | `JsonRpcTransport` adapter |
| **JSON-RPC** | Bidirectional streaming | `JsonRpcTransport` adapter (reuse) |
| **gRPC** | High-performance, typed contracts | `GrpcTransport` adapter |

**AgentCard** — the A2A spec's capability advertisement (matches our `AgentDiscovery` port exactly):

```typescript
// gemini-cli concept, adapted for rustain
pub struct AgentCard {
    pub agent_id: AgentPath,
    pub display_name: String,
    pub description: String,
    pub capabilities: Vec<Capability>,
    pub endpoints: Vec<Endpoint>,  // transport addresses
    pub signature: Ed25519Signature,
}
```

**Verdict:** Adopt the multi-transport abstraction. Define AgentCard as a domain value object. Implement `JsonRpcTransport` first, `GrpcTransport` second, `Libp2pTransport` third.

#### opencode — ACP for External Clients (Adopt as client-facing protocol)

opencode's ACP (Agent Client Protocol) bridges external clients (Zed editor) to internal sessions via JSON-RPC over stdio. This is the *client* protocol, orthogonal to A2A. rustain needs both:

```
ACP  → client-to-agent (external tools, editors)
A2A  → agent-to-agent (internal team collaboration)
```

**Verdict:** ACP is a separate concern. Note for future but don't conflate with Epic 14.

#### KIMI — Wire Pub/Sub + SubagentEvent Envelope (Take the event architecture)

KIMI's Wire provides a broadcast message bus with three sides (soul, UI, recorder) and a `SubagentEvent` envelope that wraps child events for the parent:

```rust
pub struct SubagentEvent {
    pub task_tool_call_id: String,  // links to parent's invocation
    pub event: Box<WireMessage>,    // the inner event
}
```

This is the event-driven backbone rustain needs. The key insight: **approval requests and tool calls from subagents are forwarded directly to the parent wire.** Content events are wrapped in the SubagentEvent envelope. This is the correct separation between control-plane and data-plane events.

**Verdict:** Adopt the Wire pub/sub model. The `SubAgentMessageBus` port's `subscribe(scope)` and `broadcast(scope, msg)` methods are the KIMI pattern, formalized.

#### openclaw — Session Visibility Model (Take this — it's the authorization layer)

openclaw's session visibility model (`session-visibility.ts:20-26`) defines four scopes:

```rust
pub enum VisibilityScope {
    Self_,   // Only the agent itself
    Tree,    // Agent + all descendants
    Agent,   // All sessions within the same agent workspace
    All,     // Cross-agent (requires a2a.enabled=true)
}
```

This maps directly to P2P trust domains:
- `Self` → local process
- `Tree` → DHT subtree (peers responsible for a key range)
- `Agent` → specific peer via DHT lookup
- `All` → GossipSub broadcast

openclaw's `agentToAgent` policy config (`tools.agentToAgent.enabled` + `allow` list with glob patterns) is the policy-driven communication that rustain needs.

**Verdict:** Adopt VisualScope as a domain enum. Adopt `agentToAgent` policy pattern.

#### openclaw — Announce Flow (Take with caution — powerful but complex)

```
child completion → descendant aggregation → steered/queued/direct delivery
```

The announce flow is a **convergence protocol**: results propagate up the spawn tree and aggregate. In P2P terms, this is a tree-based reduce operation. Elements to adopt:

1. **Descendant aggregation** — when a subagent completes, its own subagents' results are gathered
2. **Wake-on-descendants** — a parent blocked on `wait` is woken when all children settle
3. **Delivery dispatch** — steer (inject into running turn), queue (for next turn), direct (immediate)

**Critique (Barry):** The full announce flow with four delivery paths is overengineered for a CLI tool. A simpler Mediator pattern (`AgentMessageRouter`) with a single routing table achieves the same result.

**Resolution:** The routing should start simple (Mediator) and evolve to the announce flow only if needed. The `SubAgentMessageBus::broadcast(scope, msg)` port already supports both — the adapter decides delivery semantics.

### 2.4 Recommended Architecture for Epic 14

```mermaid
graph TD
    subgraph "Domain Layer"
        AP2[AgentPath<br/>peer_id/team/role]
        VS[VisibilityScope<br/>Self | Tree | Agent | All<br/>from openclaw]
        AC[AgentCard<br/>signed capability advertisement<br/>from gemini-cli]
        TM[Team<br/>members, roles, hierarchy<br/>from rustycode RBAC]
        CT2[CapabilityToken<br/>chainable delegation<br/>from opencode permission inheritance]
        AE[AgentEnvelope<br/>header + body + signature]
    end

    subgraph "Ports (trait definitions)"
        AMB[AgentMessageBus<br/>send, broadcast, subscribe<br/>from codex-rs + KIMI]
        AD2[AgentDiscovery<br/>discover, register, revoke<br/>from KIMI LaborMarket]
        AT[A2aTransport<br/>connect, send, stream, close<br/>from gemini-cli]
        CP2[CapabilityProvider<br/>delegate, validate, revoke<br/>from CLAUDE.md:62-79]
    end

    subgraph "Application Layer"
        MR[MessageRouter<br/>Mediator pattern<br/>all messages flow through here<br/>from Barry's recommendation]
        TS[TeamService<br/>CRUD, membership, hierarchy]
        CG[CapabilityGrant<br/>token issuance + validation]
    end

    subgraph "Adapters (I/O)"
        JR[JsonRpcTransport<br/>from gemini-cli]
        GT[GrpcTransport<br/>from gemini-cli]
        LP[Libp2pTransport<br/>GossipSub + Kademlia +<br/>request/response<br/>P2P phase]
        DD[DhtDiscovery<br/>Kademlia DHT<br/>from codex-rs AgentPath → DHT key]
        NS[NoiseSigner<br/>envelope signing<br/>+ verification]
    end

    AMB -->|implemented by| LP
    AD2 -->|implemented by| DD
    AT -->|implemented by| JR
    AT -->|implemented by| GT
    AT -->|implemented by| LP
    CP2 -->|uses| NS
```

**New domain types:**

| Type | Location | Purpose |
|------|----------|---------|
| `VisibilityScope` | `src/domain/models/a2a.rs` (new) | Self, Tree, Agent, All (from openclaw) |
| `AgentCard` | same file | Signed capability advertisement (from gemini-cli) |
| `AgentEnvelope` | same file | Signed message envelope (header + body + signature) |
| `Team` | `src/domain/models/team.rs` (new) | Members, roles, parent team hierarchy |
| `CapabilityToken` | `src/domain/models/capability.rs` (new) | Chainable capability grant token |

**New ports:**

| Port | Location | Methods |
|------|----------|---------|
| `AgentMessageBus` | `src/domain/ports/message_bus.rs` | `send(from, to, envelope)`, `broadcast(from, scope, envelope)`, `subscribe(scope) → Stream<Envelope>` |
| `AgentDiscovery` | `src/domain/ports/agent_discovery.rs` | `discover(scope, capabilities) → Vec<AgentCard>`, `register(card)`, `revoke(path)` |
| `A2aTransport` | `src/domain/ports/a2a_transport.rs` (new) | `connect(endpoint)`, `send(envelope)`, `stream() → Stream<Envelope>`, `close()` |
| `CapabilityProvider` | `src/domain/ports/capability_provider.rs` (new) | `delegate(to, subset) → Token`, `validate(token) → CapabilitySet`, `revoke(token)` |

**Note:** The `CapabilityProvider` trait described in CLAUDE.md:62-79 (DISCOVER → ACTIVATE → EXECUTE → RENDER → GOVERN) is a separate lifecycle from A2A messaging. The A2A ports handle communication; `CapabilityProvider` handles what an agent *can do*. They are orthogonal.

### 2.5 File Map

```
src/domain/
  models/
    a2a.rs                  ← NEW: VisibilityScope, AgentCard, AgentEnvelope
    team.rs                 ← NEW: Team, TeamMember, TeamRole
    capability.rs           ← NEW: CapabilityToken, CapabilitySet
  ports/
    message_bus.rs          ← NEW: AgentMessageBus trait (shared with Epic 10)
    agent_discovery.rs      ← NEW: AgentDiscovery trait (shared with Epic 10)
    a2a_transport.rs        ← NEW: A2aTransport trait
    capability_provider.rs  ← NEW: CapabilityProvider trait (from CLAUDE.md)
  services/
    message_router.rs       ← NEW: Mediator routing logic
    capability_grant.rs     ← NEW: token issuance, validation, revocation

src/application/
  a2a/
    config/
      loader.rs             ← NEW: .rustain/a2a.json → A2aConfig
    message/
      router.rs             ← NEW: AgentMessageRouter implementation
    team/
      service.rs            ← NEW: TeamService (CRUD + hierarchy + cycle detection)
    capability/
      delegator.rs          ← NEW: CapabilityDelegator

src/adapters/
  a2a/
    transport/
      json_rpc.rs           ← NEW: JsonRpcTransport
      grpc.rs               ← NEW: GrpcTransport (deferred)
      libp2p.rs             ← NEW: Libp2pTransport (P2P phase)
    discovery/
      local_directory.rs    ← NEW: wraps SubAgentRegistry (current phase)
      dht_directory.rs      ← NEW: Kademlia DHT (P2P phase)
    signing/
      noise_signer.rs       ← NEW: Noise-based envelope signing
      noop_signer.rs        ← NEW: noop stub for pre-P2P

src/infrastructure/
  runtime/
    event_loop.rs           ← EXTEND: wire A2A event handling into tokio::select!
    agent_core.rs           ← EXTEND: route A2A messages through AgentCore
```

---

## 3. Cross-Epic Intersections

### 3.1 Shared Ports Between Epics 10 and 14

Two ports are shared:

```
Epic 10                          Epic 14
  │                                │
  │  SubAgentRegistry              │
  │       ↓                        │
  ├──── AgentMessageBus ◄──────────┤  (shared: send, broadcast, subscribe)
  ├──── AgentDiscovery ◄───────────┤  (shared: discover, register)
  │                                │
  │  AgentOwnership                │
  │       ↓                        │
  └──── CapabilityProvider ────────┘  (spawn = delegate capability)
```

This is intentional. The `AgentMessageBus` and `AgentDiscovery` ports are shared because they are the same concern whether the agent is local or remote. The `CapabilityProvider` bridges both epics: spawning a subagent *is* delegating a capability.

### 3.2 AgentPath as Universal Identifier

Every architecture decision traces back to `AgentPath`. It is:
- The identity for spawning (`SubAgentRegistry::spawn(parent: AgentPath, ...)`)
- The address for messaging (`AgentMessageBus::send(to: AgentPath, ...)`)
- The key for discovery (`AgentDiscovery::register(AgentPath, ...)`)
- The routing key for P2P (`peer_id/team/role` → Kademlia DHT key)

Define `AgentPath` first. Everything else composes around it.

### 3.3 Epic Ordering

Epic 10 ships before Epic 14. The dependency chain:

```mermaid
graph LR
    E10_1["10.1<br/>AgentPath +<br/>SubAgentRegistry port"] --> E10_2["10.2<br/>SubAgentManager<br/>spawn orchestration"]
    E10_2 --> E10_3["10.3<br/>Context isolation<br/>+ forking"]
    E10_3 --> E10_6["10.6<br/>Ownership topology<br/>CapabilityProvider"]
    E10_6 --> E14["Epic 14<br/>A2A & Teams"]
    E14 --> E14_5["14.5<br/>Capability delegation<br/>across peers"]
```

Rationale: A2A communication requires agent identities (AgentPath) and a spawn model (registry). Build those in Epic 10 first.

---

## 4. What to Adopt, What to Reject

### 4.1 Adopt

| Source | Pattern | Reason |
|--------|---------|--------|
| **codex-rs** | `AgentPath` hierarchical addressing | Intuitive, maps to DHT keys, resolves elegantly |
| **codex-rs** | `InterAgentCommunication` protocol message | Well-structured envelope with author/recipient/trigger_turn |
| **codex-rs** | V2 message semantics (send_message/followup_task) | Semantic messages over raw item-passing |
| **gemini-cli** | Multi-transport A2A (REST/JSON-RPC/gRPC) | Transport-agnostic architecture, P2P-ready |
| **gemini-cli** | Google/LF A2A protocol spec v0.3.0 compliance | Open standard; first-mover advantage for Rust port; interoperability with A2A ecosystem |
| **gemini-cli** | `AgentCard` standard capability advertisement | Needed for P2P discovery |
| **gemini-cli** | `CompleteTaskTool` explicit termination protocol | Avoids implicit "agent stopped" detection |
| **gemini-cli** | Grace period recovery for limit-exceeded subagents | Better UX than silent truncation |
| **opencode** | Permission rule inheritance (parent → child) | Becomes capability delegation for P2P |
| **KIMI** | Fixed vs dynamic subagent distinction | Workspace infra vs task-specific ephemeral |
| **KIMI** | Wire pub/sub event bus + `SubagentEvent` envelope | Event-driven backbone for P2P |
| **KIMI** | `LaborMarket` registry pattern | Service locator for agent discovery |
| **openclaw** | `VisibilityScope` (Self/Tree/Agent/All) | Maps directly to P2P trust scopes |
| **openclaw** | 4-phase spawn lifecycle (validate → setup → start → post-spawn) | Clean orchestration |
| **openclaw** | `agentToAgent` policy config (enabled + allow list) | Policy-driven communication |
| **openclaw** | Descendant-aware result aggregation | Tree-based result convergence |
| **rustycode** | `SubAgentRegistry` + `SubAgentMessageQueue` port traits | Hexagonal architecture reference |
| **rustycode** | `AgentStatus` 5-state machine (Spawning → Active → Completed/Failed/Terminated) | Clean lifecycle FSM |
| **rustycode** | `register_with_limit()` with atomic operations | Race-condition-free spawning |
| **rustycode** | Cascade termination (DFS of parent-child tree) | Proper cleanup of agent sub-trees |
| **rustycode** | `InterToolMessage` separation from agent comm | Correct separation of concerns |
| **rustycode** | RBAC + Team-based ownership | Foundation for P2P capability-based security |

### 4.2 Reject

| Source | Pattern | Reason |
|--------|---------|--------|
| **codex-rs** | Actor model with stateful agents | Hard to distribute; prefer stateless handlers |
| **codex-rs** | No network protocol at all | Won't survive the transition to P2P |
| **gemini-cli** | Subagents-as-tools (`AgentTool` pattern) | Couples agent spawning to tool abstraction |
| **gemini-cli** | Recursion prohibition (agents can't call agents) | Blocks transitive delegation; use capability-based gating instead |
| **gemini-cli** | `development-tool` A2A extension | Google/Gemini-specific; rustain can define its own extensions if needed |
| **opencode** | Parent-only result capture | Command-and-control; no bidirectional A2A |
| **opencode** | No subagent event bus | Couples lifecycle to tool result channel |
| **KIMI** | `spawn_blocking` synchronous execution | Blocks event loop; everything must be async for P2P |
| **KIMI** | Filesystem-based context isolation (JSONL files) | Doesn't survive network boundaries |
| **openclaw** | Centralized Gateway architecture | Single point of failure for P2P |
| **openclaw** | `spawnDepth` as reactive limit | Arbitrary limit instead of capability model |
| **openclaw** | Full announce flow with 4 delivery paths | Overengineered for a CLI tool; start with Mediator |
| **rustycode** | Polling-based `SubAgentWorker` | Leaky abstraction; replace with event-driven push |
| **rustycode** | `InMemory*` adapters without P2P path | Correct for now but the port must not assume local |

---

## 5. Open Decisions

### 5.1 Port Granularity: 3-Port Split vs Single Port

**Question:** Should the domain define three separate port traits (Mary's proposal) or one unified `AgentCore` trait (Amelia's proposal)?

**Mary's 3-port split:**
```rust
pub trait SubAgentSpawner { ... }    // spawning lifecycle
pub trait AgentCommunication { ... }  // messaging
pub trait AgentDiscovery { ... }      // lookup
```

**Amelia's single port:**
```rust
pub trait AgentCore {
    async fn spawn(...) -> Result<AgentPath>;
    async fn send(...) -> Result<()>;
    async fn subscribe(...) -> Result<Stream<SubagentEvent>>;
    async fn terminate(...) -> Result<()>;
    fn status(...) -> AgentStatus;
}
```

| Criterion | 3-Port Split (Mary) | Single Port (Amelia) |
|-----------|---------------------|---------------------|
| **SOLID / SRP** | Each port has one responsibility | 5 responsibilities in one trait |
| **P2P readiness** | Discovery can swap to DHT independently of messaging | Must swap all 5 adapters at once |
| **Testability** | Mock each concern independently | Mock one trait |
| **Crate dependencies** | Adapters have minimal crate dependencies | libp2p pulled in by any adapter |
| **Implementation simplicity** | More wiring at composition root | Simpler wiring |

**Recommendation (Winston + Mary):** 3-port split. In a P2P system, Discovery, Communication, and Spawning have different trust models, latency profiles, and availability characteristics. They must be independently swappable. The composition root wires them together — that's its job.

### 5.2 Announce Flow vs Mediator Pattern

**Question:** For propagating subagent results, should rustain adopt openclaw's announce flow (multi-path delivery dispatch) or a simpler Mediator pattern?

| Option | Complexity | Flexibility | P2P Future |
|--------|-----------|-------------|------------|
| **Announce flow** (openclaw) | High (4 delivery paths, dispatch logic) | Handles complex patterns (steering into running turns) | Direct P2P mapping (tree-based convergence) |
| **Mediator** (Barry's proposal) | Low (single `AgentMessageRouter`) | Simple routing table | Needs extension for streaming/tree aggregation |

**Recommendation:** Start with Mediator. The `AgentMessageBus` port's `broadcast(scope, msg)` already supports hierarchical event propagation. Extend to announce flow patterns only if needed and driven by actual usage data. The port abstraction means this is a *deployment decision*, not an *architecture decision*.

### 5.3 Cryptographic Envelope: Now or Later?

**Question:** Should `AgentEnvelope` include cryptographic signing in v2.0 (pre-P2P)?

| Option | v2.0 Cost | P2P Migration Cost |
|--------|-----------|-------------------|
| **Signed from day one** | ~200 LOC for ed25519 signing + verification | Zero — already signed |
| **Unsigned until P2P** | 0 LOC | Must retroactively add signatures to all message paths |

**Recommendation (Winston):** Signed from day one. The cost is low (Ed25519 is well-supported in Rust). The benefit is that every A2A message path already carries a signature, so security is not a "bolt-on later" concern. Use `noop_signer.rs` in tests (always-valid signature) and `NoiseSigner` in production.

### 5.4 Team Hierarchy vs Flat Teams

**Question:** Should teams support hierarchical nesting (sub-teams), or be flat?

| Option | Complexity | Use Case |
|--------|-----------|----------|
| **Hierarchical** (rustycode pattern) | Cycle detection, recursive permission resolution | Mirrors org structure (`engineering/ml-team/`) |
| **Flat** | Simpler API | Sufficient for small teams |

**Recommendation (Mary):** Hierarchical with a configurable depth limit. Team hierarchy mirrors AgentPath hierarchy, and cycle detection is a solved problem (rustycode already implements it). The cost is one DFS in `TeamManager::add_sub_team()` — negligible.

---

## 6. Consensus Matrix

| Topic | Winston | Mary | Amelia | Barry | Consensus |
|-------|---------|------|--------|-------|-----------|
| AgentPath as universal address | Yes | Yes | Yes | Yes | **Unanimous** |
| rustycode as structural template | Yes | Yes | Yes | Yes | **Unanimous** |
| Adopt Google/LF A2A protocol spec | Yes | Yes | Yes | Yes | **Unanimous** |
| 3-port split (Spawn/Comm/Discovery) | Yes | Yes | No (single port) | Partial | **Majority (3-port)** |
| KIMI's pub/sub event architecture | Yes | Yes | Yes | Yes | **Unanimous** |
| gemini-cli's multi-transport A2A | Yes | Yes | Yes | Yes | **Unanimous** |
| openclaw's VisibilityScope | Yes | Yes | Yes | Yes | **Unanimous** |
| Reject subagents-as-tools | Yes | Yes | Yes | Yes | **Unanimous** |
| Reject synchronous blocking | Yes | Yes | Yes | Yes | **Unanimous** |
| Reject polling-based worker | Yes | Yes | Yes | Yes | **Unanimous** |
| Reject gemini-cli recursion prohibition | Yes | Yes | — | No (capability-gate instead) | **Majority (reject)** |
| Start with Mediator over announce flow | — | Undecided | Yes | Yes | **Majority (Mediator)** |
| Signed envelopes from day one | Yes | — | — | — | Proposal (undiscussed) |
| Hierarchical teams | — | Yes | — | — | Proposal (undiscussed) |
| Fixed vs dynamic subagent split | Yes | Yes | Yes | Yes | **Unanimous** |
| Permission rule inheritance (parent → child) | Yes | Yes | Yes | Yes | **Unanimous** |
| Ship standalone `rustain-a2a-protocol` crate | Yes | Yes | — | Yes | **Majority** |

### Key Disagreement: Single `AgentCore` Port vs 3-Port Split

**Amelia** proposes a 5-method unified `AgentCore` trait for simplicity. Five methods, one mock, straightforward wiring.

**Winston & Mary** propose three separate port traits. The argument: in a P2P system, Discovery (Kademlia DHT) and Communication (GossipSub + request/response) are different libp2p subsystems with different failure modes, latency profiles, and security models. Coupling them into one trait means swapping from in-memory to P2P requires touch points across all 5 methods instead of 3 independent adapter swaps.

**Resolution (party mode consensus):** 3-port split. Rustain's existing architecture already has 14 port traits — consistency favors the split. The composability benefit (Discovery can go P2P while Communication stays in-memory during migration) outweighs the minor wiring cost.

---

## 7. The P2P Migration Path

```
Phase 0 (Epic 10 prologue, v2.0):
  Ship rustain-a2a-protocol crate (standalone)
  — AgentCard, Task, Message, Part, TaskState, Artifact types
  — serde Serialize/Deserialize
  — protocol version enum for future-proofing
  Becomes the reference Rust A2A implementation

Phase 1 (Epic 10, v2.0):
  InMemorySubAgentRegistry
  InMemoryMessageBus (broadcast channels)
  LocalAgentDirectory (wraps SubAgentRegistry)
  InProcessTransport (A2A messages over existing event bus)

Phase 2 (Epic 14, v2.0):
  JsonRpcTransport adapter (A2A spec transport)
  GrpcTransport adapter (A2A spec transport, deferred)
  AgentCard registration in local directory
  AgentEnvelope with noop signatures (pre-P2P)

Phase 3 (v2.5):
  DhtDirectory   ← swap for LocalAgentDirectory
  GossipBus      ← swap for InMemoryMessageBus
  Libp2pTransport ← new adapter (A2A messages over libp2p)
  NoiseSigner    ← swap for NoopSigner

Phase 4 (v3.0, P2P launch):
  Capability-based security (chainable tokens)
  DID identities (AgentCard becomes a DID document extension)
  CRDT-shared agent state
  NAT traversal / relay
  A2A protocol extensions for P2P-specific features
```

Each phase is an adapter swap behind unchanged domain traits. The application layer sees no difference between local and remote agents — that's the hexagonal architecture superpower.

---

## 8. Scope Estimate

| Metric | Epic 10 | Epic 14 | `rustain-a2a-protocol` crate | Total |
|--------|---------|---------|------------------------------|-------|
| New domain types | 5 | 5 | 8 (standalone) | 18 |
| New port traits | 3 | 4 | 0 (pure types, no ports) | 7 |
| New application services | 3 | 3 | 0 | 6 |
| New adapter files | 4 | 6 | 0 | 10 |
| Extended existing files | 3 | 2 | 0 | 5 |
| Estimated new LOC | ~1200 | ~1000 | ~600 (standalone crate) | ~2800 |

The `rustain-a2a-protocol` crate is a standalone library with zero dependencies beyond `serde`, `serde_json`, and `prost` (for gRPC support). It is versioned independently of rustain to serve as the community Rust A2A implementation. rustain's `rustain-core` depends on it.

Both epics stay within rustain's existing module structure. No new crates for epics — the standalone crate is a bonus deliverable, not part of epic scope.

---

## Appendix A — Rustain's Current Ports (Reference)

The following ports exist in `src/domain/ports/` as of 2026-04-30:

| Port | File | Status |
|------|------|--------|
| `ProviderPort` | `provider.rs` | Active |
| `ToolSetPort` | `toolset.rs` | Active |
| `ApprovalPersistencePort` | `approval_persistence.rs` | Active |
| `ChannelPort` | `channel.rs` | Stub (Epic 12) |
| `ClipboardPort` | `clipboard.rs` | Active |
| `ContextPort` | `context.rs` | Active |
| `EventEmitterPort` | `event_emitter.rs` | Active |
| `MemoryPort` | `memory.rs` | Stub (Epic 11) |
| `PersonaPort` | `persona.rs` | Active |
| `SchedulerPort` | `scheduler.rs` | Stub (Epic 11) |
| `SecurityPort` | `security.rs` | Active |
| `SessionPort` | `session_port.rs` | Active |
| `StoragePort` | `storage.rs` | Active |

**Proposed additions for Epics 10 & 14:**

| New Port | File | Epic |
|----------|------|------|
| `SubAgentRegistryPort` | `sub_agent_registry.rs` | 10 |
| `AgentMessageBusPort` | `message_bus.rs` | 10, 14 (shared) |
| `AgentDiscoveryPort` | `agent_discovery.rs` | 10, 14 (shared) |
| `A2aTransportPort` | `a2a_transport.rs` | 14 |
| `CapabilityProviderPort` | `capability_provider.rs` | 14 |

## Appendix B — Anti-Pattern Reference

These patterns were identified and explicitly rejected. They are documented here so future design discussions don't accidentally resurrect them.

| Anti-pattern | Source | Why rejected |
|---|---|---|
| Subagents exposed as tools | gemini-cli | Couples agent lifecycle to tool abstraction; agents are peers, not tool wrappers |
| Blocking synchronous subagent execution | KIMI | Blocks the tokio event loop; P2P spawning is inherently async |
| Centralized gateway / message broker | openclaw | Single point of failure; P2P requires decentralized routing |
| Recursion prohibition (agents can't call agents) | gemini-cli | Blocks transitive delegation at the protocol level |
| Parent-only result communication | opencode | Command-and-control; P2P requires multi-directional messaging |
| Polling-based background workers | rustycode | Leaky abstraction; event-driven push is the correct pattern |
| `spawnDepth` as numeric limit | openclaw | Arbitrary cap instead of capability-based delegation |
| Filesystem-only context isolation | KIMI | Doesn't cross network boundaries |
| Actor model with mutable state | codex-rs | Stateful actors are hard to distribute; prefer stateless handlers |

## Appendix C — Competitor Architecture Comparison Matrix

### Subagent Spawning

| | codex-rs | gemini-cli | opencode | KIMI | openclaw | rustycode |
|---|---|---|---|---|---|---|
| **Spawning model** | Actor tree + mailbox | Tool-as-Agent (AgentTool) | Session tree + Task tool | LaborMarket registry | Gateway sessions | Hexagonal ports |
| **Agent identity** | AgentPath (hierarchical) | String name | SessionID (UUID) | String name | SessionKey (agent:name:subagent:UUID) | SubAgentId (UUID) |
| **Parent tracking** | ThreadSpawn edge in state DB | AgentLoopContext.parentSessionId | parentID on Session | None (flat registry) | spawnedBy on session store | parent_id on SubAgent |
| **Isolation** | Forked rollout + depth limit | Isolated Tool/Prompt/Resource registries | Permission rule inheritance | Separate JSONL context file + forked Runtime | Fork transcript or fresh session | Not explicitly isolated |
| **Limits** | agent_max_threads + agent_max_depth | maxTurns (30) + maxTimeMinutes (10) | None explicit | None explicit | maxSpawnDepth (3) + maxChildrenPerAgent (20) | max_agents_per_session (10) |
| **Termination** | close_agent + shutdown_agent_tree DFS | CompleteTaskTool or timeout/error | Session removal (recursive) | Subagent KimiSoul drop | Cascade kill + frozenResultText | Cascade termination (DFS) |
| **Background execution** | tokio::spawn completion watcher | LocalAgentExecutor isolated run | Subtask part in runLoop | spawn_blocking in Task tool | Embedded Pi agent run | SubAgentWorker polling loop |

### A2A Communication

| | codex-rs | gemini-cli | opencode | KIMI | openclaw | rustycode |
|---|---|---|---|---|---|---|
| **Communication model** | Mailbox (inbox per agent) | Direct RPC (REST/JSON-RPC/gRPC) | Parent result capture | Pub/sub Wire + SubagentEvent envelope | SessionsSend tool + announce flow | Message queue + InterToolMessage |
| **Message protocol** | InterAgentCommunication (author, recipient, trigger_turn) | A2A SDK messages | Task result text only | WireMessage enum (40+ variants) | Gateway request/response frames | SubAgentMessage (id, from, to, content) |
| **Transport** | In-process async_channel only | REST, JSON-RPC, gRPC | In-process only | In-process Wire channel | WebSocket (Gateway) | In-process FIFO queue |
| **Discovery** | AgentRegistry (in-memory) | AgentCard URL + AgentCardResolver | Agent config files | LaborMarket HashMap | Session store lookup | InMemorySubAgentRegistry |
| **Visibility/Access control** | None (any agent can message any) | Policy engine (ASK_USER for remote) | Permission system (allow/deny/ask) | None (pub/sub is open) | VisibilityScope (self/tree/agent/all) + agentToAgent policy | RBAC PermissionChecker |
| **Streaming** | Event stream per session | A2A SSE streaming | Tool result text only | WireMessage streaming | Gateway event stream | Not supported |
| **A2A spec compliance** | No — custom protocol | **Yes — full (client+server)** | No — ACP only | No — custom Wire | No — proprietary gateway | No — domain ports only |
| **P2P readiness** | None | High (multi-transport, client/server) | None (ACP is client protocol) | Medium (pub/sub maps to GossipSub) | Medium (WebSocket maps to libp2p) | Low (InMemory only) |
| **Cross-process** | No | Yes (with A2A server) | No | No | Yes (with Gateway) | No |
