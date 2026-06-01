//! Composition root for `AgentCore` — dispatches profile adapter names
//! to concrete adapter constructors. One factory per port dimension.

#[cfg(feature = "meta-search")]
pub mod catalog_observer_registry;

use std::sync::Arc;

use crate::adapters::noop::{NoOpChannel, NoOpContext, NoOpMemory, NoOpScheduler, NoOpSession};
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::domain::errors::AdapterCompositionError;
use crate::domain::models::profile::{AdapterRef, PortDimension, ProfileSelection};
use crate::domain::models::project_context::ProjectContext;
use crate::domain::ports::{
    ChannelPort, ContextPort, MemoryPort, PersonaPort, SandboxManager, SchedulerPort, SessionPort,
    StoragePort, ToolSetPort,
};
use crate::infrastructure::runtime::agent_core::AgentCore;

/// Dependencies required by per-port factories. Constructed once at startup
/// and snapshotted onto AppState for reload-time re-composition.
#[derive(Clone)]
pub struct ComposeContext {
    pub workspace_path: std::path::PathBuf,
    pub project_context: ProjectContext,
    pub storage: Arc<dyn StoragePort>,
    pub skill_activator: Arc<SkillActivator>,
    /// MCP server specs resolved from workspace `.claude/mcp.json` + profile TOML.
    /// Populated by startup.rs after profile resolution (Story 9.1).
    pub mcp_servers: Vec<crate::domain::models::McpServerSpec>,
    /// Whether composite adapter includes builtin tools (default true).
    /// `[tools.config] include_builtin = false` disables builtin tools
    /// so only MCP tools are available (Story 9.1 AC-4, used by 9.2).
    pub include_builtin_tools: bool,
    pub domain_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
    /// Story 9.4 — exposure strategy name from AppConfig.tools.exposure.
    /// Phase A: always "static-full".
    pub tool_exposure: String,
    /// Story 9.6 — skill exposure strategy name from AppConfig.skill_exposure.kind.
    /// Phase A: "l1-metadata" (default) or "static-full" (opt-in).
    pub skill_exposure: String,
    /// Story 9.6 — shared two-layer skill cache (L1 LRU + L2 disk snapshot).
    /// Passed to both L1MetadataExposure and StaticFullExposure constructors,
    /// and to ToolSetAdapter for the skill_view builtin tool.
    pub skill_cache: std::sync::Arc<crate::infrastructure::skill_cache::SkillCache>,
    /// Story 9.5 — sandbox adapter name from AppConfig.sandbox.adapter.
    /// Phase A: "noop" (default on all platforms) or "landlock" (Linux + feature).
    pub sandbox_adapter: String,
    /// Story 9.5 — startup sandbox policy for `restrict_self()` parent-process
    /// call. Derived from `SandboxPolicy::from_mode(initial_mode, workspace)`.
    pub sandbox_startup_policy: crate::domain::models::sandbox::SandboxPolicy,
    /// Story 9.5 — sandbox manager ArcSwap slot, initialized to NoOpSandbox
    /// and updated to the real adapter after AgentCore::compose. Threaded
    /// through to ToolSetAdapter for Bash-tool enforcement.
    pub sandbox_slot: Arc<arc_swap::ArcSwap<Arc<dyn SandboxManager>>>,
    /// Story 9.5 — sandbox policy shared reference for Bash-tool enforcement.
    /// Mirrors the AppState.sandbox_policy field; read at spawn time.
    pub sandbox_policy: Arc<tokio::sync::RwLock<crate::domain::models::sandbox::SandboxPolicy>>,
    /// Story 11.1 — shared memory-port slot for the `remember` builtin tool.
    /// Mirrors `sandbox_slot`: created (NoOpMemory) before compose and shared
    /// with `ToolSetAdapter`. `build_memory` publishes the composed memory port
    /// into it — including on profile reload — so the `remember` tool, which
    /// reads through `ArcSwap::load_full()`, always routes to the live adapter.
    pub memory_slot: Arc<arc_swap::ArcSwap<Arc<dyn MemoryPort>>>,
    #[cfg(feature = "meta-search")]
    pub search_config: crate::domain::models::SearchConfig,
    #[cfg(feature = "meta-search")]
    pub meta_search_engine: Option<Arc<dyn crate::domain::ports::search::MetaSearchEngine>>,
}

impl AgentCore {
    /// Compose an AgentCore from a profile selection. FATAL on any per-port error.
    pub fn compose(
        profile_name: &str,
        selection: &ProfileSelection,
        ctx: &ComposeContext,
    ) -> Result<Self, AdapterCompositionError> {
        let started = std::time::Instant::now();
        tracing::info!(profile = %profile_name, "Composing AgentCore for profile");

        let persona = compose_one(PortDimension::Persona, selection, |n, c| {
            build_persona(n, c, ctx)
        })?;
        let memory = compose_one(PortDimension::Memory, selection, |n, c| {
            build_memory(n, c, ctx)
        })?;
        let session = compose_one(PortDimension::Session, selection, |n, c| {
            build_session(n, c, ctx)
        })?;
        let tools = compose_one(PortDimension::Tools, selection, |n, c| {
            build_tools(n, c, ctx)
        })?;
        let channels = compose_one(PortDimension::Channels, selection, |n, c| {
            build_channels(n, c, ctx)
        })?;
        let scheduler = compose_one(PortDimension::Scheduler, selection, |n, c| {
            build_scheduler(n, c, ctx)
        })?;
        let context = compose_one(PortDimension::Context, selection, |n, c| {
            build_context(n, c, ctx)
        })?;
        let tool_exposure = build_tool_exposure(selection, ctx)?;
        let skill_exposure = build_skill_exposure(selection, ctx)?;
        let sandbox = build_sandbox(selection, ctx)?;

        let elapsed = started.elapsed();
        tracing::info!(
            elapsed_ms = elapsed.as_millis() as u64,
            "AgentCore composition complete"
        );

        Ok(Self {
            persona: Self::wrap(persona),
            memory: Self::wrap(memory),
            session: Self::wrap(session),
            tools: Self::wrap(tools),
            channels: Self::wrap(channels),
            scheduler: Self::wrap(scheduler),
            context: Self::wrap(context),
            tool_exposure: Self::wrap_optional(tool_exposure),
            skill_exposure: Self::wrap_optional(skill_exposure),
            sandbox: Self::wrap(sandbox),
            #[cfg(feature = "meta-search")]
            merged_index: arc_swap::ArcSwap::from_pointee(
                None as Option<Arc<crate::infrastructure::search::MergedIndex>>,
            ),
        })
    }
}

fn compose_one<T: ?Sized, F>(
    port: PortDimension,
    selection: &ProfileSelection,
    build: F,
) -> Result<Arc<T>, AdapterCompositionError>
where
    F: FnOnce(&str, Option<&toml::Value>) -> Result<Arc<T>, AdapterCompositionError>,
{
    let adapter_ref = selection
        .dimensions
        .get(&port)
        .ok_or_else(|| AdapterCompositionError::MissingDimension { port })?;
    let result = build(adapter_ref.adapter.as_str(), adapter_ref._config.as_ref());
    match &result {
        Ok(_) => {
            tracing::debug!(port = ?port, adapter = %adapter_ref.adapter, "Composed port adapter")
        }
        Err(e) => {
            tracing::error!(port = ?port, adapter = %adapter_ref.adapter, error = %e, "Adapter composition failed")
        }
    }
    result
}

// ── Per-port factories ──

pub fn build_persona(
    name: &str,
    _config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn PersonaPort>, AdapterCompositionError> {
    match name {
        "minimal" => Ok(Arc::new(crate::adapters::noop::NoOpPersona)),
        "coding" => Ok(Arc::new(PersonaAdapter::new(ctx.project_context.clone()))),
        "personal-assistant" => {
            tracing::warn!(
                port = ?PortDimension::Persona,
                adapter = %name,
                "Placeholder adapter — real implementation deferred to Epic 12+"
            );
            Ok(Arc::new(crate::adapters::noop::NoOpPersona))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Persona,
            name: other.to_string(),
            available: vec![
                "minimal".into(),
                "coding".into(),
                "personal-assistant".into(),
            ],
        }),
    }
}

pub fn build_memory(
    name: &str,
    _config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn MemoryPort>, AdapterCompositionError> {
    let adapter: Arc<dyn MemoryPort> = match name {
        "noop" => Arc::new(NoOpMemory),
        // Story 11.1 — real file-backed daily-log adapter.
        "daily-log" => Arc::new(crate::adapters::daily_log_memory::DailyLogMemory::new(
            &ctx.workspace_path,
        )),
        // Story 11.2 — the real daily-log + long-term MEMORY.md composite. Wire
        // `domain_tx` into the long-term child so the >20KB size warning (AC3)
        // surfaces as a SystemNotice; `None` (headless/eval) skips it silently.
        "project-scoped" => {
            let mut composite = crate::adapters::project_scoped_memory::ProjectScopedMemory::new(
                &ctx.workspace_path,
            );
            if let Some(ref tx) = ctx.domain_tx {
                composite.set_event_tx(tx.clone());
            }
            Arc::new(composite)
        }
        // Story 11.2 — standalone curated long-term tier (for `/memory long-term`
        // overrides and isolated tests). Same size-warning wiring as the composite.
        "long-term" => {
            let mut long_term =
                crate::adapters::long_term_memory::LongTermMemory::new(&ctx.workspace_path);
            if let Some(ref tx) = ctx.domain_tx {
                long_term.set_event_tx(tx.clone());
            }
            Arc::new(long_term)
        }
        // Story 11.3a — local semantic-search memory. A KNOWN adapter whose
        // behaviour depends on the `vector-search` feature: when compiled in, the
        // real wrap-and-override adapter; when not, a graceful SystemNotice + a
        // project-scoped keyword fallback (AC4) — deliberately NOT the
        // `UnknownAdapter` hard error, which would abort startup.
        "vector-search" => build_vector_search_memory(ctx),
        other => {
            return Err(AdapterCompositionError::UnknownAdapter {
                port: PortDimension::Memory,
                name: other.to_string(),
                available: vec![
                    "noop".into(),
                    "project-scoped".into(),
                    "daily-log".into(),
                    "long-term".into(),
                    "vector-search".into(),
                ],
            });
        }
    };

    // Publish the composed memory port into the shared slot so the `remember`
    // builtin tool (held by ToolSetAdapter via Arc::clone of this slot) routes
    // writes to the live adapter — on both initial compose and profile reload.
    ctx.memory_slot.store(Arc::new(Arc::clone(&adapter)));
    Ok(adapter)
}

/// Build the `project-scoped` content source used as the vector adapter's inner
/// (Q1) — the same construction as the standalone `project-scoped` arm, so it
/// indexes daily-log + MEMORY.md and the AC4 keyword fallback is `inner.search`.
fn build_project_scoped_inner(ctx: &ComposeContext) -> Arc<dyn MemoryPort> {
    let mut inner =
        crate::adapters::project_scoped_memory::ProjectScopedMemory::new(&ctx.workspace_path);
    if let Some(ref tx) = ctx.domain_tx {
        inner.set_event_tx(tx.clone());
    }
    Arc::new(inner)
}

/// Story 11.3a — the `vector-search` memory arm, compiled in. Wraps the inner
/// `project-scoped` content source with a `LocalEmbeddingProvider` + the flat
/// cosine index, persisting to `{workspace}/.rustain/memory/index.bin`. The
/// model cache is user-global (`~/.config/rustain/models/`), a DIFFERENT root.
#[cfg(feature = "vector-search")]
fn build_vector_search_memory(ctx: &ComposeContext) -> Arc<dyn MemoryPort> {
    use crate::adapters::vector_search::{
        LocalEmbeddingProvider, VectorSearchMemory, default_cache_dir,
    };

    let inner = build_project_scoped_inner(ctx);
    let provider = LocalEmbeddingProvider::new(default_cache_dir(), ctx.domain_tx.clone());
    let index_path = ctx
        .workspace_path
        .join(".rustain")
        .join("memory")
        .join("index.bin");
    Arc::new(VectorSearchMemory::new(
        inner,
        Arc::new(provider),
        index_path,
    ))
}

/// Story 11.3a — the `vector-search` memory arm, NOT compiled in. Emits the
/// exact AC4 SystemNotice and falls back to keyword-only `project-scoped` search.
/// This is graceful-by-design: `vector-search` is a KNOWN adapter (just not
/// built), so routing it through `UnknownAdapter` — which is fatal at compose —
/// would violate AC4's "memory falls back to keyword-only search".
#[cfg(not(feature = "vector-search"))]
fn build_vector_search_memory(ctx: &ComposeContext) -> Arc<dyn MemoryPort> {
    use crate::domain::events::AppEvent;
    use crate::domain::models::NoticeLevel;

    if let Some(ref tx) = ctx.domain_tx {
        let event = AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message:
                "Adapter 'vector-search' not available. Install with: cargo install rustain --features vector-search"
                    .to_string(),
        };
        let _ = tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 11-3a AC4 — not-compiled fallback notice via composition domain_tx (no event_bus access)
    }

    build_project_scoped_inner(ctx)
}

pub fn build_session(
    name: &str,
    _config: Option<&toml::Value>,
    _ctx: &ComposeContext,
) -> Result<Arc<dyn SessionPort>, AdapterCompositionError> {
    match name {
        "basic" | "workspace" => {
            if name == "workspace" {
                tracing::warn!(
                    port = ?PortDimension::Session,
                    adapter = %name,
                    "Placeholder adapter — real implementation deferred to Epic 12+"
                );
            }
            Ok(Arc::new(NoOpSession))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Session,
            name: other.to_string(),
            available: vec!["basic".into(), "workspace".into()],
        }),
    }
}

pub fn build_tools(
    name: &str,
    _config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn ToolSetPort>, AdapterCompositionError> {
    match name {
        "builtin-only" => {
            let mut adapter = ToolSetAdapter::new(
                ctx.workspace_path.clone(),
                Arc::clone(&ctx.storage),
                Arc::clone(&ctx.sandbox_slot),
                Arc::clone(&ctx.sandbox_policy),
            );
            // Wire the event bus so plan tools (propose_plan / exit_plan_mode)
            // can emit PlanProposed / PlanApprovalRequested. Without this the
            // composed adapter's event_tx stays None and plan approval cards
            // are silently dropped. `None` (headless/eval) stays valid.
            if let Some(ref tx) = ctx.domain_tx {
                adapter.set_event_tx(tx.clone());
            }
            // Story 11.1 — wire the shared memory slot so the `remember` builtin
            // tool can append notable entries via MemoryPort::store.
            adapter.set_memory(Arc::clone(&ctx.memory_slot));
            #[cfg(feature = "meta-search")]
            if let Some(ref engine) = ctx.meta_search_engine {
                adapter.set_meta_search_engine(Arc::clone(engine));
            }
            Ok(Arc::new(adapter))
        }
        "builtin-full" => {
            let mut adapter = ToolSetAdapter::new(
                ctx.workspace_path.clone(),
                Arc::clone(&ctx.storage),
                Arc::clone(&ctx.sandbox_slot),
                Arc::clone(&ctx.sandbox_policy),
            );
            adapter.set_activator(Arc::clone(&ctx.skill_activator));
            // Wire the event bus so plan tools (propose_plan / exit_plan_mode)
            // can emit PlanProposed / PlanApprovalRequested. Without this the
            // composed adapter's event_tx stays None and plan approval cards
            // are silently dropped. `None` (headless/eval) stays valid.
            // `composite` reuses this profile, so it inherits the wiring.
            if let Some(ref tx) = ctx.domain_tx {
                adapter.set_event_tx(tx.clone());
            }
            // Story 11.1 — wire the shared memory slot for the `remember` tool.
            adapter.set_memory(Arc::clone(&ctx.memory_slot));
            #[cfg(feature = "meta-search")]
            if let Some(ref engine) = ctx.meta_search_engine {
                adapter.set_meta_search_engine(Arc::clone(engine));
            }
            Ok(Arc::new(adapter))
        }
        #[cfg(feature = "mcp")]
        "composite" => {
            if ctx.mcp_servers.is_empty() {
                return Err(AdapterCompositionError::MissingComposeContext {
                    port: PortDimension::Tools,
                    name: name.to_string(),
                    missing_field: "mcp_servers (empty Vec) — composite requires at least one server; profile should use 'builtin-full' instead".into(),
                });
            }
            let builtin = build_tools("builtin-full", None, ctx)?;
            let mcp_clients: Vec<Arc<crate::adapters::mcp::client::McpClientAdapter>> = ctx
                .mcp_servers
                .iter()
                .map(|spec| {
                    let client = crate::adapters::mcp::client::McpClientAdapter::new(
                        spec.clone(),
                        ctx.domain_tx.clone(),
                    );
                    let arc = Arc::new(client);
                    arc.set_self_weak(Arc::downgrade(&arc));
                    arc
                })
                .collect();
            let adapter = crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter::new(
                builtin,
                mcp_clients,
                ctx.mcp_servers.clone(),
                ctx.include_builtin_tools,
                ctx.domain_tx.clone(),
                Some(ctx.skill_activator.clone()),
                None, // SubagentProvider wired in main.rs for Story 10.0+
            );
            Ok(Arc::new(adapter))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Tools,
            name: other.to_string(),
            #[cfg(feature = "mcp")]
            available: vec![
                "builtin-only".into(),
                "builtin-full".into(),
                "composite".into(),
            ],
            #[cfg(not(feature = "mcp"))]
            available: vec!["builtin-only".into(), "builtin-full".into()],
        }),
    }
}

pub fn build_channels(
    name: &str,
    _config: Option<&toml::Value>,
    _ctx: &ComposeContext,
) -> Result<Arc<dyn ChannelPort>, AdapterCompositionError> {
    match name {
        "terminal" => Ok(Arc::new(NoOpChannel)),
        "telegram" => Err(AdapterCompositionError::MissingComposeContext {
            port: PortDimension::Channels,
            name: name.to_string(),
            missing_field:
                "telegram feature not compiled — profile validator should have rewritten this to 'terminal'"
                    .into(),
        }),
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Channels,
            name: other.to_string(),
            available: vec!["terminal".into(), "telegram".into()],
        }),
    }
}

pub fn build_scheduler(
    name: &str,
    _config: Option<&toml::Value>,
    _ctx: &ComposeContext,
) -> Result<Arc<dyn SchedulerPort>, AdapterCompositionError> {
    match name {
        "none" => Ok(Arc::new(NoOpScheduler)),
        "cron" => Err(AdapterCompositionError::MissingComposeContext {
            port: PortDimension::Scheduler,
            name: name.to_string(),
            missing_field:
                "cron feature not compiled — profile validator should have rewritten this to 'none'"
                    .into(),
        }),
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Scheduler,
            name: other.to_string(),
            available: vec!["none".into(), "cron".into()],
        }),
    }
}

pub fn build_context(
    name: &str,
    _config: Option<&toml::Value>,
    _ctx: &ComposeContext,
) -> Result<Arc<dyn ContextPort>, AdapterCompositionError> {
    match name {
        "default" | "daily" => {
            if name == "daily" {
                tracing::warn!(
                    port = ?PortDimension::Context,
                    adapter = %name,
                    "Placeholder adapter — real implementation deferred to Epic 12+"
                );
            }
            Ok(Arc::new(NoOpContext))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Context,
            name: other.to_string(),
            available: vec!["default".into(), "daily".into()],
        }),
    }
}

/// Story 9.4 — build the per-turn exposure strategy from active config.
///
/// Returns `Some(Arc::new(StaticFullExposure))` for normal startup (the only
/// Phase A impl) and `None` for the headless / eval-harness path per
/// ADR-09-01 v2.1 §W1.
///
/// # Coupling with `validate_tools_exposure`
///
/// `startup::validate_tools_exposure` runs BEFORE this factory and rejects
/// anything other than `"static-full"` with an actionable error citing
/// Story 9.7 + ADR-09-01 v2.2. This factory's `Err` arm is defense-in-depth
/// — it should be unreachable in production. If you add a new valid exposure
/// value (e.g. Phase B `"meta-search"`), update BOTH this match AND the
/// validation function.
pub fn build_tool_exposure(
    _selection: &ProfileSelection,
    ctx: &ComposeContext,
) -> Result<Option<Arc<dyn crate::domain::ports::ToolExposurePort>>, AdapterCompositionError> {
    // ⚠ ASYMMETRY-BY-DESIGN: Tools default = `StaticFullExposure` (per ADR-09-01
    // §Decision) while Skills default = `L1MetadataExposure` (per ADR-09-02
    // §Decision Story 9.6). Do NOT "fix" this asymmetry by aligning defaults
    // without RE-OPENING BOTH ADRs:
    //
    //   - ADR-09-01 §Decision (Tools=StaticFullExposure): Mary's "MCP ecosystem
    //     evidence is partial — Arcade single-anchor only" stance + Lunarpulse's
    //     n=1 user signal. The MetaSearch Phase B path is GATED on the 7 Phase B
    //     Prerequisites in ADR-09-02 §Phased Implementation (subsumes ADR-09-01's
    //     original 5). Default `static-full` preserves zero-behavior-change for
    //     all current users.
    //
    //   - ADR-09-02 §Decision (Skills=L1MetadataExposure): 7-signal ecosystem
    //     evidence (gemini-cli, hermes-agent, opencode, Anthropic-spec, Google,
    //     Boliv 97%-reduction production deployment, K-Dense-AI/claude-skills-mcp)
    //     + Anthropic spec mandate for progressive disclosure. Default
    //     `l1-metadata` honors spec + 3-of-4 ecosystem peers from day one.
    //
    // Each port has INDEPENDENT evidence channels and the defaults reflect those
    // channels HONESTLY. Symmetric defaults would either (a) burn 25-50k prefix
    // tokens on typical Skills catalogs with no opt-in (if both default to
    // static-full), or (b) destabilize MCP-default behavior for the n=0
    // production users on meta-search Phase A (if both default to meta-search).
    // Neither is acceptable.
    //
    // See SCP-2026-05-21-skill-exposure-strategy §4.2 for the John Round-2
    // directive that mandated this guard comment.
    //
    // ⚠ PHASE B ASYMMETRY-BY-DESIGN — DO NOT silently restore symmetry on `[search]` runtime config ⚠
    //
    // Phase B introduces a SECOND ASYMMETRIC DEFAULT pair:
    //   [search] skills = "on"  → Skills meta-search ON by default (when user opts into [skill_exposure].kind = "meta-search")
    //   [search] tools  = "off" → Tools meta-search OFF by default (user must explicitly opt in via BOTH knobs)
    //
    // The asymmetry is BY DESIGN per ADR-09-02 v2 §Audience Split + Mary Round 4
    // Phase B Prereq re-evaluation:
    //   - Skills track: 7-signal ecosystem saturation → meta-search defaults ON when feature compiled.
    //   - Tools track: PARTIAL ecosystem evidence (Arcade single-anchor) → meta-search defaults OFF
    //     even when feature compiled; user must flip BOTH `[search].tools = "on"` AND
    //     `[tools].exposure = "meta-search"` to opt in.
    //
    // This asymmetry is independent of (and STACKS on) the Phase A asymmetric defaults documented
    // above (StaticFullExposure-vs-L1MetadataExposure). DO NOT collapse either asymmetry to "fix"
    // the other without re-opening BOTH ADR-09-01 + ADR-09-02 + SCP-2026-05-23-phase-b-audience-split.
    match ctx.tool_exposure.as_str() {
        "static-full" => Ok(Some(Arc::new(
            crate::adapters::tool_exposure::StaticFullExposure::new(),
        ))),
        #[cfg(feature = "meta-search")]
        "meta-search" => {
            if let Some(engine) = &ctx.meta_search_engine {
                Ok(Some(Arc::new(
                    crate::adapters::tool_exposure::MetaSearchExposure::new(Arc::clone(engine)),
                )))
            } else {
                Err(AdapterCompositionError::MissingComposeContext {
                    port: PortDimension::Tools,
                    name: "meta-search".into(),
                    missing_field: "meta_search_engine".into(),
                })
            }
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Tools,
            name: other.to_string(),
            available: vec!["static-full".into()],
        }),
    }
}

/// Story 9.6 — build the per-turn skill exposure strategy from active config.
///
/// Returns `Some(Arc::new(L1MetadataExposure))` for the default `"l1-metadata"`,
/// `Some(Arc::new(StaticFullExposure))` for opt-in `"static-full"`, or `None`
/// for the headless / eval-harness path per ADR-09-01 v2.1 §W1 (inherited).
///
/// # Coupling with `validate_skill_exposure`
///
/// `startup::validate_skill_exposure` runs BEFORE this factory and rejects
/// anything other than `"l1-metadata"` or `"static-full"` with an actionable
/// error citing Story 9.7 + ADR-09-02. This factory's `Err` arm is
/// defense-in-depth — it should be unreachable in production.
pub fn build_skill_exposure(
    _selection: &ProfileSelection,
    ctx: &ComposeContext,
) -> Result<Option<Arc<dyn crate::domain::ports::SkillExposurePort>>, AdapterCompositionError> {
    // ⚠ ASYMMETRY-BY-DESIGN — DO NOT silently restore symmetry with build_tool_exposure ⚠
    //
    // This factory defaults to `L1MetadataExposure` (the SPEC-ALIGNED default per
    // ADR-09-02 §Decision). The sibling factory `build_tool_exposure` (see above)
    // defaults to `StaticFullExposure`. THE TWO DEFAULTS ARE INTENTIONALLY ASYMMETRIC:
    //
    //   - SKILLS default to L1Metadata because ADR-09-02 §Context documents
    //     7-signal ecosystem evidence saturation (gemini-cli, hermes-agent,
    //     opencode, Anthropic-spec, Google, Boliv, K-Dense-AI) AND the Anthropic
    //     Skills spec mandates progressive disclosure. Codex is the lone outlier.
    //
    //   - TOOLS default to StaticFull because ADR-09-01 §Decision documents
    //     PARTIAL ecosystem evidence (Arcade single-anchor) AND vendor primitives
    //     for MCP-side meta-search are still beta.
    //
    // If a future refactor wants to "fix" the asymmetry, DO NOT do so without
    // re-opening:
    //   1. ADR-09-01 §Decision (Tool-side evidence channel)
    //   2. ADR-09-02 §Decision (Skill-side evidence channel)
    // Each port has independent evidence channels per SCP-2026-05-21
    // (John Round-2 directive). Symmetric defaults would either over-eager
    // the Tools side or under-serve the Skills side.
    //
    // See also:
    //   - SCP-2026-05-21-skill-exposure-strategy.md §4.2
    //   - The sibling guard comment at build_tool_exposure above
    //   - epic-9-design-flags-2026-05-20.md Flag 4 (Tools) + Flag 5 (Skills)
    //
    // ⚠ PHASE B ASYMMETRY-BY-DESIGN — DO NOT silently restore symmetry on `[search]` runtime config ⚠
    //
    // Phase B introduces a SECOND ASYMMETRIC DEFAULT pair:
    //   [search] skills = "on"  → Skills meta-search ON by default (when user opts into [skill_exposure].kind = "meta-search")
    //   [search] tools  = "off" → Tools meta-search OFF by default (user must explicitly opt in via BOTH knobs)
    //
    // The asymmetry is BY DESIGN per ADR-09-02 v2 §Audience Split + Mary Round 4
    // Phase B Prereq re-evaluation:
    //   - Skills track: 7-signal ecosystem saturation → meta-search defaults ON when feature compiled.
    //   - Tools track: PARTIAL ecosystem evidence (Arcade single-anchor) → meta-search defaults OFF
    //     even when feature compiled; user must flip BOTH `[search].tools = "on"` AND
    //     `[tools].exposure = "meta-search"` to opt in.
    //
    // This asymmetry is independent of (and STACKS on) the Phase A asymmetric defaults documented
    // above (StaticFullExposure-vs-L1MetadataExposure). DO NOT collapse either asymmetry to "fix"
    // the other without re-opening BOTH ADR-09-01 + ADR-09-02 + SCP-2026-05-23-phase-b-audience-split.
    match ctx.skill_exposure.as_str() {
        "l1-metadata" => Ok(Some(Arc::new(
            crate::adapters::skill_exposure::L1MetadataExposure::new(
                Arc::clone(&ctx.skill_cache),
            ),
        ))),
        "static-full" => Ok(Some(Arc::new(
            crate::adapters::skill_exposure::StaticFullExposure::new(
                Arc::clone(&ctx.skill_cache),
            ),
        ))),
        #[cfg(feature = "meta-search")]
        "meta-search" => {
            if let Some(engine) = &ctx.meta_search_engine {
                Ok(Some(Arc::new(
                    crate::adapters::skill_exposure::MetaSearchExposure::new(Arc::clone(engine)),
                )))
            } else {
                Err(AdapterCompositionError::MissingComposeContext {
                    port: PortDimension::Skills,
                    name: "meta-search".into(),
                    missing_field: "meta_search_engine".into(),
                })
            }
        }
        #[cfg(not(feature = "meta-search"))]
        "meta-search" => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Skills,
            name: "meta-search skill exposure requires the `meta-search` cargo feature; see ADR-09-02 §Phase B".into(),
            available: vec!["l1-metadata".into(), "static-full".into()],
        }),
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Skills,
            name: other.to_string(),
            available: vec!["l1-metadata".into(), "static-full".into()],
        }),
    }
}

/// Story 9.5 — build the sandbox manager from active config + platform/feature.
///
/// Resolution order:
/// 1. `ctx.sandbox_adapter` (Figment-resolved from `[sandbox].adapter`)
/// 2. If `"noop"`: bind `NoOpSandbox`.
/// 3. If `"landlock"` AND `#[cfg(all(target_os = "linux", feature = "sandbox"))]`:
///     attempt `LandlockSandbox::new(&startup_policy)`. On `Ok`: bind. On
///     `Err(AbiTooOld | RulesetBuildFailed | ...)`: log `tracing::warn!` and
///     fall back to `NoOpSandbox`.
/// 4. If `"landlock"` AND not Linux OR without `sandbox` feature:
///     `Err(AdapterCompositionError::UnknownAdapter)` — defense-in-depth;
///     `startup::validate_sandbox_adapter` runs BEFORE this factory and
///     converts the same configuration error into an actionable startup
///     message.
///
/// # Coupling with `validate_sandbox_adapter`
///
/// `startup::validate_sandbox_adapter` runs BEFORE this factory and rejects
/// `"landlock"` with an actionable error pointing at the `sandbox` cargo
/// feature when the build is missing it. This factory's `Err` arm is
/// defense-in-depth.
pub fn build_sandbox(
    _selection: &ProfileSelection,
    ctx: &ComposeContext,
) -> Result<Arc<dyn SandboxManager>, AdapterCompositionError> {
    use crate::adapters::sandbox::NoOpSandbox;

    match ctx.sandbox_adapter.as_str() {
        "noop" => Ok(Arc::new(NoOpSandbox)),

        #[cfg(all(target_os = "linux", feature = "sandbox"))]
        "landlock" => {
            use crate::adapters::sandbox::LandlockSandbox;
            match LandlockSandbox::new(&ctx.sandbox_startup_policy) {
                Ok(sb) => Ok(Arc::new(sb)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Landlock sandbox construction failed; falling back to NoOpSandbox \
                         (ADR-06-04 §Negative — documented as known limitation per NFR33)"
                    );
                    Ok(Arc::new(NoOpSandbox))
                }
            }
        }

        #[cfg(not(all(target_os = "linux", feature = "sandbox")))]
        "landlock" => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Tools, // re-use; no new PortDimension Phase A
            name: "landlock requires the `sandbox` cargo feature on Linux".into(),
            available: vec!["noop".into()],
        }),

        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Tools,
            name: other.to_string(),
            available: vec![
                "noop".into(),
                #[cfg(all(target_os = "linux", feature = "sandbox"))]
                "landlock".into(),
            ],
        }),
    }
}

// ── BuiltAdapter — typed dispatch enum for per-port adapter construction ──

pub enum BuiltAdapter {
    Persona(Arc<dyn PersonaPort>),
    Memory(Arc<dyn MemoryPort>),
    Session(Arc<dyn SessionPort>),
    Tools(Arc<dyn ToolSetPort>),
    Channels(Arc<dyn ChannelPort>),
    Scheduler(Arc<dyn SchedulerPort>),
    Context(Arc<dyn ContextPort>),
}

pub fn build_for_port(
    port: PortDimension,
    adapter_ref: &AdapterRef,
    ctx: &ComposeContext,
) -> Result<BuiltAdapter, AdapterCompositionError> {
    match port {
        PortDimension::Persona => {
            build_persona(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Persona)
        }
        PortDimension::Memory => {
            build_memory(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Memory)
        }
        PortDimension::Session => {
            build_session(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Session)
        }
        PortDimension::Tools => {
            build_tools(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Tools)
        }
        PortDimension::Channels => {
            build_channels(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Channels)
        }
        PortDimension::Scheduler => {
            build_scheduler(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Scheduler)
        }
        PortDimension::Context => {
            build_context(&adapter_ref.adapter, adapter_ref._config.as_ref(), ctx)
                .map(BuiltAdapter::Context)
        }
        PortDimension::Skills => Err(AdapterCompositionError::AdapterConstructionFailed {
            port: PortDimension::Skills,
            name: "skills".into(),
            source: Box::<dyn std::error::Error + Send + Sync>::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "skill exposure is composed via build_skill_exposure, not build_for_port",
            )),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::noop::NoOpStorage;
    use crate::infrastructure::skill_cache::SkillCache;
    use std::path::PathBuf;

    fn test_compose_ctx() -> ComposeContext {
        use crate::adapters::sandbox::NoOpSandbox;
        ComposeContext {
            workspace_path: PathBuf::from("/tmp/test"),
            project_context: ProjectContext::empty(),
            storage: Arc::new(NoOpStorage::default()) as Arc<dyn StoragePort>,
            skill_activator: Arc::new(SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            tool_exposure: "static-full".into(),
            skill_exposure: "l1-metadata".into(),
            skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new_in_memory()),
            sandbox_adapter: "noop".into(),
            sandbox_startup_policy: crate::domain::models::sandbox::SandboxPolicy::Permissive,
            sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(
                Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>
            )),
            sandbox_policy: Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
            memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
                crate::adapters::noop::NoOpMemory,
            )
                as Arc<dyn MemoryPort>)),
            #[cfg(feature = "meta-search")]
            search_config: crate::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
        }
    }

    // ── Persona tests ──

    #[test]
    fn test_build_persona_coding() {
        let ctx = test_compose_ctx();
        let result = build_persona("coding", None, &ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_persona_minimal() {
        let ctx = test_compose_ctx();
        let result = build_persona("minimal", None, &ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_persona_unknown() {
        let ctx = test_compose_ctx();
        let result = build_persona("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter {
                port, available, ..
            }) => {
                assert_eq!(port, PortDimension::Persona);
                assert!(!available.is_empty());
            }
            other => panic!("expected UnknownAdapter"),
        }
    }

    // ── Memory tests ──

    #[test]
    fn test_build_memory_noop() {
        let ctx = test_compose_ctx();
        assert!(build_memory("noop", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_memory_project_scoped() {
        let ctx = test_compose_ctx();
        assert!(build_memory("project-scoped", None, &ctx).is_ok());
    }

    // Story 11.1 — daily-log composes a REAL adapter (a stored entry is
    // retrievable); project-scoped stays NoOp (store is a no-op) until 11.2.
    #[tokio::test]
    async fn test_build_memory_daily_log_is_real_and_published_to_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_compose_ctx();
        ctx.workspace_path = tmp.path().to_path_buf();

        let mem = build_memory("daily-log", None, &ctx).expect("daily-log builds");
        mem.store(crate::domain::models::MemoryEntry {
            timestamp: chrono::Local::now(),
            summary: "composed entry".into(),
            context: None,
        })
        .await
        .unwrap();
        assert_eq!(
            mem.recent(10).await.unwrap().len(),
            1,
            "daily-log persists (non-NoOp)"
        );
        // build_memory published the composed port into the shared slot.
        let slot = ctx.memory_slot.load_full();
        assert_eq!(
            slot.recent(10).await.unwrap().len(),
            1,
            "composed memory port published into memory_slot"
        );
    }

    // Story 11.2 — project-scoped composes a REAL composite: a `remember_fact`'d
    // fact AND a `store`'d entry are both retrievable via `recent` (test 13).
    #[tokio::test]
    async fn test_build_memory_project_scoped_is_real_composite() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_compose_ctx();
        ctx.workspace_path = tmp.path().to_path_buf();

        let mem = build_memory("project-scoped", None, &ctx).expect("project-scoped builds");
        mem.remember_fact(crate::domain::models::MemoryFact {
            category: "Database".into(),
            fact: "PostgreSQL 15".into(),
            detail: None,
        })
        .await
        .unwrap();
        mem.store(crate::domain::models::MemoryEntry {
            timestamp: chrono::Local::now(),
            summary: "daily decision".into(),
            context: None,
        })
        .await
        .unwrap();

        let recent = mem.recent(10).await.unwrap();
        let summaries: Vec<&str> = recent.iter().map(|e| e.summary.as_str()).collect();
        assert!(
            summaries.contains(&"PostgreSQL 15"),
            "long-term fact retrievable"
        );
        assert!(
            summaries.contains(&"daily decision"),
            "daily entry retrievable"
        );
        // Long-term first (AC6).
        assert_eq!(recent[0].summary, "PostgreSQL 15");

        // MEMORY.md was written by remember_fact (the real long-term child).
        assert!(tmp.path().join(".rustain").join("MEMORY.md").exists());
        // The composed port was published into the shared slot.
        let slot = ctx.memory_slot.load_full();
        assert!(!slot.recent(10).await.unwrap().is_empty());
    }

    // Story 11.2 — the standalone `long-term` arm builds a real LongTermMemory.
    #[tokio::test]
    async fn test_build_memory_long_term_is_real() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_compose_ctx();
        ctx.workspace_path = tmp.path().to_path_buf();

        let mem = build_memory("long-term", None, &ctx).expect("long-term builds");
        mem.remember_fact(crate::domain::models::MemoryFact {
            category: "Preferences".into(),
            fact: "prefers snake_case".into(),
            detail: None,
        })
        .await
        .unwrap();
        assert_eq!(mem.recent(10).await.unwrap().len(), 1, "long-term persists");
        // store is a no-op on the standalone long-term tier.
        mem.store(crate::domain::models::MemoryEntry {
            timestamp: chrono::Local::now(),
            summary: "ignored".into(),
            context: None,
        })
        .await
        .unwrap();
        assert_eq!(
            mem.recent(10).await.unwrap().len(),
            1,
            "store does not add to the long-term tier"
        );
    }

    // Story 11.1 — additive-with-defaults regression: NoOpMemory's defaulted
    // store/recent/search return Ok/empty without any edits to noop.rs.
    #[tokio::test]
    async fn test_noop_memory_additive_defaults() {
        use crate::adapters::noop::NoOpMemory;
        let m = NoOpMemory;
        assert!(
            m.store(crate::domain::models::MemoryEntry {
                timestamp: chrono::Local::now(),
                summary: "x".into(),
                context: None,
            })
            .await
            .is_ok()
        );
        assert!(m.recent(5).await.unwrap().is_empty());
        assert!(m.search("q", 5).await.unwrap().is_empty());
    }

    // Story 11.2 — additive-with-defaults regression (test 15): the new
    // `remember_fact` default no-op covers NoOpMemory (untouched) and
    // DailyLogMemory (inherited default — durable facts belong in MEMORY.md, not
    // the append-only daily log, so it stores nothing and creates NO MEMORY.md).
    #[tokio::test]
    async fn test_remember_fact_additive_defaults() {
        use crate::adapters::noop::NoOpMemory;
        let fact = crate::domain::models::MemoryFact {
            category: "Cat".into(),
            fact: "durable".into(),
            detail: None,
        };
        assert!(NoOpMemory.remember_fact(fact.clone()).await.is_ok());

        let tmp = tempfile::tempdir().unwrap();
        let daily = crate::adapters::daily_log_memory::DailyLogMemory::new(tmp.path());
        assert!(
            daily.remember_fact(fact).await.is_ok(),
            "DailyLogMemory inherits the remember_fact default no-op"
        );
        assert!(
            !tmp.path().join(".rustain").join("MEMORY.md").exists(),
            "daily-log's remember_fact default creates no MEMORY.md"
        );
    }

    #[test]
    fn test_build_memory_unknown() {
        let ctx = test_compose_ctx();
        let result = build_memory("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Memory);
            }
            other => panic!("expected UnknownAdapter"),
        }
    }

    // ── Session tests ──

    #[test]
    fn test_build_session_basic() {
        let ctx = test_compose_ctx();
        assert!(build_session("basic", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_session_workspace() {
        let ctx = test_compose_ctx();
        assert!(build_session("workspace", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_session_unknown() {
        let ctx = test_compose_ctx();
        let result = build_session("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Session);
            }
            other => panic!("expected UnknownAdapter"),
        }
    }

    // ── Tools tests ──

    #[test]
    fn test_build_tools_builtin_only() {
        let ctx = test_compose_ctx();
        assert!(build_tools("builtin-only", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_tools_builtin_full() {
        let ctx = test_compose_ctx();
        assert!(build_tools("builtin-full", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_tools_unknown() {
        let ctx = test_compose_ctx();
        let result = build_tools("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Tools);
            }
            other => panic!("expected UnknownAdapter"),
        }
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn test_build_tools_composite_empty_fails() {
        let ctx = test_compose_ctx();
        let result = build_tools("composite", None, &ctx);
        match result {
            Err(AdapterCompositionError::MissingComposeContext { port, .. }) => {
                assert_eq!(port, PortDimension::Tools);
            }
            other => panic!("expected MissingComposeContext for composite with empty mcp_servers"),
        }
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn test_build_tools_composite_with_servers() {
        let mut ctx = test_compose_ctx();
        ctx.mcp_servers = vec![crate::domain::models::McpServerSpec {
            id: "test-server".into(),
            transport: crate::domain::models::McpTransport::Stdio,
            command: Some("echo".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            persistent: false,
            source: crate::domain::models::McpServerSource::Workspace,
        }];
        assert!(build_tools("composite", None, &ctx).is_ok());
    }

    // ── Channels tests ──

    #[test]
    fn test_build_channels_terminal() {
        let ctx = test_compose_ctx();
        assert!(build_channels("terminal", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_channels_telegram_missing_context() {
        let ctx = test_compose_ctx();
        let result = build_channels("telegram", None, &ctx);
        match result {
            Err(AdapterCompositionError::MissingComposeContext { port, .. }) => {
                assert_eq!(port, PortDimension::Channels);
            }
            other => panic!("expected MissingComposeContext"),
        }
    }

    #[test]
    fn test_build_channels_unknown() {
        let ctx = test_compose_ctx();
        let result = build_channels("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Channels);
            }
            other => panic!("expected UnknownAdapter"),
        }
    }

    // ── Scheduler tests ──

    #[test]
    fn test_build_scheduler_none() {
        let ctx = test_compose_ctx();
        assert!(build_scheduler("none", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_scheduler_cron_missing_context() {
        let ctx = test_compose_ctx();
        let result = build_scheduler("cron", None, &ctx);
        match result {
            Err(AdapterCompositionError::MissingComposeContext { port, .. }) => {
                assert_eq!(port, PortDimension::Scheduler);
            }
            other => panic!("expected MissingComposeContext"),
        }
    }

    #[test]
    fn test_build_scheduler_unknown() {
        let ctx = test_compose_ctx();
        let result = build_scheduler("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Scheduler);
            }
            other => panic!("expected UnknownAdapter"),
        }
    }

    // ── Context tests ──

    #[test]
    fn test_build_context_default() {
        let ctx = test_compose_ctx();
        assert!(build_context("default", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_context_daily() {
        let ctx = test_compose_ctx();
        assert!(build_context("daily", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_context_unknown() {
        let ctx = test_compose_ctx();
        let result = build_context("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Context);
            }
            other => panic!("expected UnknownAdapter"),
        }
    }
}
