# Adapter Composition (Story 8.3)

## Composition Root

Adapter composition happens in `src/infrastructure/composition/mod.rs` after profile resolution. One factory function per port dimension dispatches adapter names to concrete adapter constructors. The factory output is wrapped in `Arc<ArcSwap<Arc<dyn PortTrait>>>` and stored on `AgentCore`.

```text
ProfileSelection → AgentCore::compose()
  ├─ build_persona(name)  → PersonaPort
  ├─ build_memory(name)   → MemoryPort
  ├─ build_session(name)  → SessionPort
  ├─ build_tools(name)    → ToolSetPort
  ├─ build_channels(name) → ChannelPort
  ├─ build_scheduler(name)→ SchedulerPort
  └─ build_context(name)  → ContextPort
```

`ComposeContext` is the dependency-injection seam:

```rust
pub struct ComposeContext {
    pub workspace_path: PathBuf,
    pub project_context: ProjectContext,
    pub storage: Arc<dyn StoragePort>,
    pub skill_activator: Arc<SkillActivator>,
}
```

`AgentCore` holds exactly 7 `Arc<ArcSwap<Arc<dyn PortTrait>>>` slots — one per port dimension. Each slot can be atomically swapped independently (Story 8.4).

## Adding a New Adapter

1. **Implement the port trait** in a new module under `src/adapters/<adapter_module>/`.
2. **Add an `AdapterDescriptor`** to `AdapterCatalog` in `src/domain/services/adapter_catalog.rs` (name + optional feature gate + fallback).
3. **Add a match arm** to the corresponding `build_<port>` factory function in `src/infrastructure/composition/mod.rs`.

After these three steps, the adapter name is validatable in profiles and composable at runtime. Dynamic runtime registration is deferred (DF-NNN).

## Placeholder Adapters

The following adapter names currently map to `NoOp` implementations pending their real implementations in Epic 12+:

| Port | Placeholder Name | Future Implementation |
|------|-----------------|----------------------|
| Persona | `personal-assistant` | Prompt-based persona specialization |
| Memory | `project-scoped`, `daily-log` | File-based / SQLite memory storage |
| Session | `workspace` | Workspace-scoped session persistence |
| Context | `daily` | Daily context rolling window |

Placeholders emit `tracing::warn!` at construction time. No user-visible notice is emitted.

The following adapter names are **feature-gated** and return a hard error when the corresponding feature is not compiled — `AdapterCatalog::fallback_for` rewrites them to the default before composition in normal operation:

| Port | Name | Requires | Fallback |
|------|------|----------|----------|
| Scheduler | `cron` | `cron` feature (not yet implemented) | `none` (NoOpScheduler) |
| Channels | `telegram` | `telegram` feature (not yet implemented) | `terminal` (NoOpChannel) |

## Adapter SDK Compatibility

- **New methods MUST carry a default impl** — existing adapter implementations compile unchanged.
- **Removing a method** or **changing a signature** is a MAJOR version bump.
- **Additive-with-default =** minor version bump.
- Publishing `rustain-adapter-sdk` as a separate crate is deferred to v1.5+ (NFR51).

The pattern is preserved for future extraction: per-port factory functions in `composition/mod.rs` are thin dispatch shims; concrete adapter constructors live in their own modules; the port traits are the stable API contract.

## Memory Footprint

Per-adapter struct sizes (informational, not a CI gate):

| Adapter | `size_of` | Notes |
|---------|----------|-------|
| `NoOpPersona` | 0 bytes | Zero-sized default |
| `NoOpMemory` | 0 bytes | Zero-sized default |
| `NoOpSession` | 0 bytes | Zero-sized default |
| `NoOpToolSet` | 0 bytes | Zero-sized default |
| `NoOpChannel` | 0 bytes | Zero-sized default |
| `NoOpScheduler` | 0 bytes | Zero-sized default |
| `NoOpContext` | 0 bytes | Zero-sized default |
| `PersonaAdapter` | ~48 bytes | Contains `ProjectContext` (24 bytes + Vec heap) |
| `ToolSetAdapter` | ~176 bytes | PathBuf, write_locks, storage Arc, optional fields |

Total `AgentCore` holder overhead: ~1 KB (7 × ~64 bytes ArcSwap structs + 7 × 8 bytes Arc pointers). The adapter heap allocations dominate.

NFR45 budget: < 5MB per loaded adapter, < 35MB total for the default `coding` profile composition.

## Reload Semantics

When a profile change triggers `/config reload`:

1. Config + profile resolver are atomically swapped (Story 8.1 + 8.2).
2. `AgentCore::compose()` is called with the new profile selection and the **snapshot** `ComposeContext` (stored on `AppState.compose_snapshot`).
3. On success: each of the 7 port slots on the existing `AgentCore` is individually swapped — other tasks holding `Arc::clone(&app_state.agent_core)` see the new adapters.
4. On failure: config + profile remain swapped; previous adapters remain active; a `ConfigReloaded { success: false, error: "..." }` notice is emitted.

The `ComposeContext` snapshot is NOT re-built at reload time — profile changes do NOT re-scan project files (that is a separate "context refresh" concern).

## Composite Tools Adapter

The `composite` adapter (Story 9.1 + 9.2) delegates tool execution to the builtin adapter while managing MCP server lifecycle (connect, reconnect, shutdown, profile-switch migration). MCP-discovered tools are projected into the tool catalog with canonical `mcp__<server>__<tool>` naming per ADR-06-08 (see [Invoking MCP tools](mcp.md#invoking-mcp-tools)). `@MCP/` autocomplete lets users discover and insert MCP tool names into the input buffer (see [Discovering tools with @MCP/](mcp.md#discovering-tools-with-mcp)).

### Configuration

```toml
[tools]
adapter = "composite"

[tools.config]
include_builtin = true    # default true; if false, only MCP tools are available

[tools.config.mcp.postgres]
transport = "stdio"
command = "mcp-server-postgres"
args = ["--connection-string", "$DATABASE_URL"]
persistent = false

[tools.config.mcp.git]
transport = "stdio"
command = "mcp-server-git"
```

### Worked Example: `coding` profile

```toml
# ~/.config/rustain/coding.toml
[tools]
adapter = "composite"

[tools.config.mcp.postgres]
command = "mcp-server-postgres"
args = ["--connection-string", "$DATABASE_URL"]

[tools.config.mcp.git]
command = "mcp-server-git"
```

### Worked Example: `personal-assistant` profile

```toml
# ~/.config/rustain/personal-assistant.toml
[tools]
adapter = "composite"

[tools.config.mcp.calendar]
command = "mcp-server-calendar"
```

### Feature Gate

The `composite` adapter requires the `mcp` cargo feature (enabled by default in v0.5). Building without it falls back to `builtin-full`.

### Lifecycle

- **Startup**: MCP servers are lazy-connected after the first frame renders
- **Reconnect**: Exponential backoff (1s→32s, max 5 attempts) on disconnect
- **Profile switch**: Warm-tier migration preserves `persistent = true` servers
- **Shutdown**: Parallel shutdown with 5s ceiling; EOF → 2s grace → SIGKILL

### Capability Registry

The `CompositeToolsetAdapter` holds an internal `CapabilityRegistry` (Story 9.3a, Flag 1) that serves as the single source of truth for "what capabilities exist right now".

**Lifecycle:** `discover` (read provider capabilities from in-memory caches) → `register` (insert into the registry) → `invoke` (execute via `CapabilityProvider::invoke`).

**Namespace bridge:** The registry uses `::` (double-colon) ids — `mcp::postgres::query`, `builtin::bash`, `skill::review`. These are distinct from the LLM-wire `mcp__<server>__<tool>` shape (Story 9.2, ADR-06-08). Bridge methods `CapabilityId::from_mcp_wire_name` / `to_mcp_wire_name` convert between the two.

**Reactive surface:** Every `register` / `deregister` / `update` emits a `CapabilityEvent` through the existing `AppEvent` bus (same `event_tx` channel as `McpConnectionStateChanged` and `McpCatalogChanged` from 9.1 + 9.2). The event loop sets `needs_redraw = true`, so the adapter-status panel automatically picks up the registry snapshot on the next render tick.

**Observer subscription:** The `CatalogObserver` trait + `CapabilityRegistry::subscribe(observer)` seam is shipped in 9.3a. The fan-out implementation (`CatalogObserverRegistry` at `src/infrastructure/composition/catalog_observer_registry.rs`) lands in Story 9.4b Phase B (per ADR-09-01 v2.2 §Phased Implementation).

### ToolDescriptor — Domain Catalog Shape (Story 9.3b)

`ToolDescriptor` is the domain catalog shape consumed by `FilteredCatalog`
(Story 9.4 Phase A), `IndexableItem` (Story 9.7 Phase B), and
`SkillExposurePort::render` (Story 9.6 Phase A).

| Type | Shape | Used by |
|------|-------|---------|
| `ToolDefinition` (`tools.rs:7`) | LLM-wire | Anthropic / OpenAI / Ollama tool schemas |
| `RegisteredCapability` (`capability_registry.rs:51`) | Registry working shape | `CapabilityRegistry::snapshot()` |
| `ToolDescriptor` (`tool_descriptor.rs`) | Domain catalog | `FilteredCatalog`, `IndexableItem`, `SkillExposurePort` |

Conversion: `From<&RegisteredCapability> for ToolDescriptor` synthesizes
`ToolAnnotations { read_only_hint: Some(parallel_safe), ..None }` (best-effort).
Richer annotations require provider-side population — currently only MCP
preserves the full rmcp `ToolAnnotations` shape; built-ins + skills default
to read_only_hint = Some(parallel_safe).

### CatalogDelta — Phase A No-op, Phase B Broadcast (Story 9.3b → Story 9.7)

`CatalogDelta { added, removed, version }` is the diff type emitted by
`CompositeToolsetAdapter::emit_catalog_delta()` whenever the merged catalog
changes (provider registered/deregistered, MCP server `notifications/tools/list_changed`).

- **Phase A (Story 9.3b):** type defined, `emit_catalog_delta` returns `Ok(())` no-op. Version counter increments and prev-catalog diffing happens internally so future Phase B wiring observes a stable monotonic version. No broadcast subscriber.
- **Phase B (Story 9.7):** `tokio::sync::broadcast::Sender<CatalogDelta>` field added; owned-task debounce per ADR-09-01 v2.2 §W3. `CatalogObserverRegistry` at `src/infrastructure/composition/catalog_observer_registry.rs` subscribes and fans out to `Vec<Arc<dyn CatalogObserver>>`.
