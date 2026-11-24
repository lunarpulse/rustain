//! Composition root for `AgentCore` — dispatches profile adapter names
//! to concrete adapter constructors. One factory per port dimension.

#[cfg(feature = "meta-search")]
pub mod catalog_observer_registry;

use std::sync::Arc;

use crate::adapters::noop::{NoOpChannel, NoOpContext, NoOpMemory, NoOpScheduler, NoOpSession};
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::workspace_registry::FileWorkspaceRegistry;
use crate::domain::errors::AdapterCompositionError;
use crate::domain::models::profile::{AdapterRef, PortDimension, ProfileSelection};
use crate::domain::models::project_context::ProjectContext;
use crate::domain::ports::{
    ChannelPort, ContextPort, MemoryPort, PersonaPort, SandboxManager, SchedulerPort, SecurityPort,
    SessionPort, StoragePort, StreamingProvider, ToolSetPort, WorkspaceRegistrarPort,
};
use crate::infrastructure::runtime::agent_core::AgentCore;

/// Dependencies required by per-port factories. Constructed once at startup
/// and snapshotted onto AppState for reload-time re-composition.
#[derive(Clone)]
pub struct ComposeContext {
    pub workspace_path: std::path::PathBuf,
    /// Story C-2 — the configuration root (`RUSTAIN_CONFIG_DIR` or
    /// `~/.config/rustain`, from `paths::config_dir()`); profiles live in
    /// `<config_dir>/profiles`, persona files in `<config_dir>/personas`.
    /// Composition never reads the environment itself.
    pub config_dir: std::path::PathBuf,
    pub project_context: ProjectContext,
    pub storage: Arc<dyn StoragePort>,
    pub skill_activator: Arc<SkillActivator>,
    /// MCP server specs resolved from workspace `.claude/mcp.json` + profile TOML.
    /// Populated by startup.rs after profile resolution (Story 9.1).
    pub mcp_servers: Vec<crate::domain::models::McpServerSpec>,
    /// Allowlisted A2A peers resolved from workspace and profile configuration.
    pub a2a_peers: Vec<crate::domain::models::A2aPeerSpec>,
    /// Whether composite adapter includes builtin tools (default true).
    /// `[tools.config] include_builtin = false` disables builtin tools
    /// so only MCP tools are available (Story 9.1 AC-4, used by 9.2).
    pub include_builtin_tools: bool,
    pub domain_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
    pub channel_turn_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::domain::models::ChannelTurnRequest>>,
    /// Story 9.4 — exposure strategy name from AppConfig.tools.exposure.
    /// Phase A: always "static-full".
    pub tool_exposure: String,
    /// Story 11.6 — Message-tier assembler strategy name from
    /// AppConfig.assembler.strategy. `"passthrough"` (default) or `"windowing"`.
    pub assembler: String,
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
    /// Story 12.4 — held-writer prevention gate for the shared memory slot.
    /// Writers (remember/store) take a shared read lock; the warm-swap path
    /// takes an exclusive write lock. This guarantees no in-flight write can be
    /// in-progress on a detached adapter — the swap blocks until every writer
    /// finishes. `tokio::sync::RwLock` (not `std`) so writers may hold it across
    /// `.await` points without blocking the runtime. Deadlock-free by lock
    /// ordering: writers never nest, and the swap holds only the write lock.
    pub memory_write_gate: Arc<tokio::sync::RwLock<()>>,
    /// Story 18.4a — the replicated Topic log, shared by the transport that
    /// fills it and the `"composite"` context adapter that reads it.
    ///
    /// ⛔ One `Arc`, threaded rather than constructed twice: two stores is a
    /// host whose agent reads a different log than the one its transport
    /// writes, and the profile-reload path re-composes the context port while
    /// the transport keeps running.
    pub peer_topic_store: Arc<crate::adapters::rap::PeerTopicStore>,
    #[cfg(feature = "meta-search")]
    pub search_config: crate::domain::models::SearchConfig,
    #[cfg(feature = "meta-search")]
    pub meta_search_engine: Option<Arc<dyn crate::domain::ports::search::MetaSearchEngine>>,
}

fn build_workspace_registrar() -> Result<Arc<dyn WorkspaceRegistrarPort>, AdapterCompositionError> {
    FileWorkspaceRegistry::new()
        .map(|registry| Arc::new(registry) as Arc<dyn WorkspaceRegistrarPort>)
        .map_err(
            |source| AdapterCompositionError::AdapterConstructionFailed {
                port: PortDimension::Session,
                name: "workspace-registry".to_string(),
                source: Box::new(source),
            },
        )
}

impl AgentCore {
    /// Compose an AgentCore from a profile selection. FATAL on any per-port error.
    pub fn compose(
        profile_name: &str,
        selection: &ProfileSelection,
        ctx: &ComposeContext,
    ) -> Result<Self, AdapterCompositionError> {
        Self::compose_inner(profile_name, selection, ctx, true)
    }

    /// Like [`compose`](Self::compose) but the memory adapter is NOT published
    /// into `ctx.memory_slot`: the caller publishes it at its commit point
    /// (`/config reload`, C2-05).
    pub fn compose_staged(
        profile_name: &str,
        selection: &ProfileSelection,
        ctx: &ComposeContext,
    ) -> Result<Self, AdapterCompositionError> {
        Self::compose_inner(profile_name, selection, ctx, false)
    }

    fn compose_inner(
        profile_name: &str,
        selection: &ProfileSelection,
        ctx: &ComposeContext,
        publish_memory: bool,
    ) -> Result<Self, AdapterCompositionError> {
        let started = std::time::Instant::now();
        tracing::info!(profile = %profile_name, "Composing AgentCore for profile");

        let persona = compose_one(PortDimension::Persona, selection, |n, c| {
            build_persona(n, c, ctx)
        })?;
        let memory = compose_one(PortDimension::Memory, selection, |n, c| {
            if publish_memory {
                build_memory(n, c, ctx)
            } else {
                build_memory_unpublished(n, c, ctx)
            }
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
        let isolation: Arc<dyn crate::domain::ports::IsolationProvider> =
            Arc::new(crate::adapters::isolation::CowIsolationProvider::default());

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
            peer_topic_store: Arc::clone(&ctx.peer_topic_store),
            agent_message_bus: AgentCore::wrap(Arc::new(
                crate::infrastructure::agent_message_bus::LocalMessageBus::new(
                    Default::default(),
                    Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
                ),
            )
                as Arc<dyn crate::domain::ports::AgentMessageBus>),
            // Story 11.0a / 11.6 — Message-tier assembler, Option-wrapped like
            // the exposure ports (no BuiltAdapter variant — Option ports are
            // bound here, not via store_for_port). The concrete strategy is
            // selected by name from `[assembler].strategy` (default
            // "passthrough"). `None` is the reserved eval/replay bypass.
            context_assembler: Self::wrap_optional(Some(build_context_assembler(ctx)?)),
            tool_exposure: Self::wrap_optional(tool_exposure),
            skill_exposure: Self::wrap_optional(skill_exposure),
            sandbox: Self::wrap(sandbox),
            isolation: Self::wrap(isolation),
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

/// The log line for an adapter name that composes but is still a stand-in.
const PLACEHOLDER_ADAPTER_WARNING: &str =
    "Using a placeholder adapter; the real adapter is not available yet";

/// Embedded persona text of the built-in `personal-assistant` profile (AD-5).
const PERSONAL_ASSISTANT_PERSONA: &str =
    include_str!("../../../profiles/personas/personal-assistant.md");

pub fn build_persona(
    name: &str,
    config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn PersonaPort>, AdapterCompositionError> {
    compose_persona(
        &AdapterRef {
            adapter: name.to_string(),
            _config: config.cloned(),
        },
        &ctx.project_context,
        &ctx.config_dir.join("personas"),
    )
}

/// Story C-2 (AD-6) — the one place a persona is composed from an
/// `AdapterRef`: pure on its arguments, reads the persona file only when
/// `config.file` is set. Used by the TUI, the daemon, `ask`, ACP and
/// `profile validate`.
pub fn compose_persona(
    adapter_ref: &AdapterRef,
    project_context: &ProjectContext,
    personas_dir: &std::path::Path,
) -> Result<Arc<dyn PersonaPort>, AdapterCompositionError> {
    use crate::adapters::persona_file::load_persona_file;
    use crate::domain::models::persona_config::PersonaFileConfig;

    let name = adapter_ref.adapter.as_str();
    if !matches!(name, "minimal" | "coding" | "personal-assistant") {
        return Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Persona,
            name: name.to_string(),
            available: vec![
                "minimal".into(),
                "coding".into(),
                "personal-assistant".into(),
            ],
        });
    }
    let failed = |source: Box<dyn std::error::Error + Send + Sync>| {
        AdapterCompositionError::AdapterConstructionFailed {
            port: PortDimension::Persona,
            name: name.to_string(),
            source,
        }
    };
    let file = PersonaFileConfig::from_config(adapter_ref._config.as_ref())
        .map_err(|e| failed(Box::new(e)))?;
    let body: Option<Arc<str>> = match file {
        Some(cfg) => Some(Arc::from(
            load_persona_file(&cfg, personas_dir)
                .map_err(|e| failed(Box::new(e)))?
                .into_string(),
        )),
        None => None,
    };
    Ok(match name {
        "minimal" => match body {
            None => Arc::new(crate::adapters::noop::NoOpPersona),
            Some(body) => Arc::new(PersonaAdapter::with_body(
                Some(body),
                ProjectContext::empty(),
            )),
        },
        "coding" => Arc::new(PersonaAdapter::with_body(body, project_context.clone())),
        _ => {
            let body = body.unwrap_or_else(|| Arc::from(PERSONAL_ASSISTANT_PERSONA.trim_end()));
            Arc::new(PersonaAdapter::with_body(
                Some(body),
                ProjectContext::empty(),
            ))
        }
    })
}

/// Story C-2 (AD-10) — the persona for `rustain ask` and ACP: the `--persona`
/// override (replacing the whole reference, `config` included) or the active
/// profile's persona, composed with an empty project context.
pub fn compose_headless_persona(
    selection: &ProfileSelection,
    cli_persona: Option<&str>,
    config_dir: &std::path::Path,
) -> Result<Arc<dyn PersonaPort>, AdapterCompositionError> {
    let adapter_ref = match cli_persona {
        Some(adapter) => AdapterRef {
            adapter: adapter.to_string(),
            _config: None,
        },
        // No resolved profile (empty selection): the same `coding` default the
        // daemon uses, which composes to the empty prompt `ask`/ACP always had.
        None => selection
            .dimensions
            .get(&PortDimension::Persona)
            .cloned()
            .unwrap_or(AdapterRef {
                adapter: "coding".to_string(),
                _config: None,
            }),
    };
    compose_persona(
        &adapter_ref,
        &ProjectContext::empty(),
        &config_dir.join("personas"),
    )
}

/// Story C-2 (C2-05): construct a memory adapter WITHOUT touching the shared
/// live slot. A reload composes everything first and publishes only at its
/// commit point, so a refused composition is never visible to a consumer of the
/// slot and never overwrites a concurrent legitimate swap.
pub fn build_memory_unpublished(
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
        "vector-search" => build_vector_search_memory(ctx, _config),
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
    Ok(adapter)
}

/// Build AND publish the memory port: the composed adapter is stored into the
/// shared slot so the `remember` builtin tool (held by ToolSetAdapter via
/// `Arc::clone` of this slot) routes writes to the live adapter — on initial
/// compose and on a warm swap (which holds `memory_write_gate`).
pub fn build_memory(
    name: &str,
    config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn MemoryPort>, AdapterCompositionError> {
    let adapter = build_memory_unpublished(name, config, ctx)?;
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

/// Story 11.3a/11.3b — the `vector-search` memory arm, compiled in. Wraps the
/// inner `project-scoped` content source with a config-selected
/// `EmbeddingProvider` (Local default, or a Remote OpenAI-compatible one — AC1)
/// + the flat cosine index, persisting to `{workspace}/.rustain/memory/index.bin`.
/// The model cache is user-global (`~/.config/rustain/models/`), a DIFFERENT
/// root. The `domain_tx` is wired into the adapter so the guided-reindex notices
/// (AC1) surface on a provider/dimension switch.
#[cfg(feature = "vector-search")]
fn build_vector_search_memory(
    ctx: &ComposeContext,
    config: Option<&toml::Value>,
) -> Arc<dyn MemoryPort> {
    use crate::adapters::vector_search::{
        EmbeddingProvider, LocalEmbeddingProvider, VectorSearchConfig, VectorSearchMemory,
        default_cache_dir,
    };
    use crate::domain::events::AppEvent;

    // Deserialize the per-adapter `[memory] config` block from the VERIFIED-wired
    // `AdapterRef._config` seam (profile.rs:78-81; proven by `adapter_ref_with_config`).
    // Absent / unparsable → defaults (provider = "local"), so offline/coding
    // profiles are unchanged (NFR9).
    let cfg: VectorSearchConfig = match config {
        Some(v) => match v.clone().try_into::<VectorSearchConfig>() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "invalid [memory] config for vector-search — falling back to defaults");
                VectorSearchConfig::default()
            }
        },
        None => VectorSearchConfig::default(),
    };

    let inner = build_project_scoped_inner(ctx);
    let provider: Arc<dyn EmbeddingProvider> = match resolve_embedding_provider(&cfg, ctx) {
        Ok(p) => p,
        Err(reason) => {
            // A remote-provider misconfig must NEVER abort startup — fall back to
            // the local model with a surfaced notice (offline-default + the AC4
            // graceful-degradation philosophy).
            if let Some(ref tx) = ctx.domain_tx {
                let event = AppEvent::SystemNotice {
                    conversation_id: None,
                    level: crate::domain::models::NoticeLevel::Warning,
                    message: format!(
                        "Memory embedding provider '{}' unavailable ({reason}); using the local model.",
                        cfg.provider
                    ),
                };
                let _ = tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 11-3b AC1 — remote-provider misconfig falls back to local with a surfaced notice (no event_bus access at compose time)
            }
            Arc::new(LocalEmbeddingProvider::new(
                default_cache_dir(),
                ctx.domain_tx.clone(),
            ))
        }
    };

    let index_path = ctx
        .workspace_path
        .join(".rustain")
        .join("memory")
        .join("index.bin");
    let mut mem = VectorSearchMemory::new(inner, provider, index_path);
    if let Some(ref tx) = ctx.domain_tx {
        mem.set_event_tx(tx.clone());
    }
    Arc::new(mem)
}

/// Story 11.3b — select the embedding provider from [`VectorSearchConfig`] (AC1).
/// `Ok` for the local default and any well-formed remote provider; `Err(reason)`
/// (a human-readable string) when a remote provider is misconfigured (unknown
/// provider string, missing `base_url`/`model`, unknown dimension, or bad
/// credentials) so the caller falls back to local with a notice. Vendor strings
/// are resolved to a `base_url` here — they never become type names
/// (architecture.md:174).
#[cfg(feature = "vector-search")]
fn resolve_embedding_provider(
    cfg: &crate::adapters::vector_search::VectorSearchConfig,
    ctx: &ComposeContext,
) -> Result<Arc<dyn crate::adapters::vector_search::EmbeddingProvider>, String> {
    use crate::adapters::vector_search::{
        LocalEmbeddingProvider, RemoteEmbeddingProvider, default_cache_dir, known_dimension,
        provider_defaults,
    };

    let provider = cfg.provider.trim();
    if provider.is_empty() || provider.eq_ignore_ascii_case("local") {
        return Ok(Arc::new(LocalEmbeddingProvider::new(
            default_cache_dir(),
            ctx.domain_tx.clone(),
        )));
    }

    let defaults =
        provider_defaults(provider).ok_or_else(|| format!("unknown provider '{provider}'"))?;

    // `base_url` makes a host swap a config change, not a code rewrite (the
    // AC-11-3b-GATE fallback requirement). `openai-compatible` has no default.
    let base_url = cfg
        .base_url
        .clone()
        .or_else(|| defaults.base_url.map(str::to_string))
        .ok_or_else(|| format!("provider '{provider}' requires an explicit base_url"))?;

    let model = cfg
        .model
        .clone()
        .or_else(|| defaults.default_model.map(str::to_string))
        .ok_or_else(|| format!("provider '{provider}' requires an explicit model"))?;

    // Never guess a dimension — a wrong one corrupts the index header.
    let dimension = cfg
        .dimension
        .or_else(|| known_dimension(&model))
        .ok_or_else(|| {
            format!(
                "dimension for model '{model}' is unknown — set `dimension` in the [memory] config"
            )
        })?;

    // Only the env var NAME is configured; the key value is read from the env.
    let api_key_env = cfg
        .api_key_env
        .clone()
        .unwrap_or_else(|| defaults.api_key_env.to_string());
    let api_key = crate::infrastructure::utils::env_var_trimmed(&api_key_env).unwrap_or_default();
    if api_key.is_empty() {
        return Err(format!(
            "API key env var '{api_key_env}' is not set or empty"
        ));
    }

    let remote = RemoteEmbeddingProvider::new(base_url, api_key, model, dimension)
        .map_err(|e| format!("failed to build remote provider: {e}"))?;
    Ok(Arc::new(remote))
}

/// Story 11.3a — the `vector-search` memory arm, NOT compiled in. Emits the
/// exact AC4 SystemNotice and falls back to keyword-only `project-scoped` search.
/// This is graceful-by-design: `vector-search` is a KNOWN adapter (just not
/// built), so routing it through `UnknownAdapter` — which is fatal at compose —
/// would violate AC4's "memory falls back to keyword-only search".
#[cfg(not(feature = "vector-search"))]
fn build_vector_search_memory(
    ctx: &ComposeContext,
    _config: Option<&toml::Value>,
) -> Arc<dyn MemoryPort> {
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
                    "{PLACEHOLDER_ADAPTER_WARNING}"
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
            adapter.hide_activate_skill_tool();
            adapter.set_skill_cache(Arc::clone(&ctx.skill_cache));
            // Wire the event bus so plan tools (propose_plan / exit_plan_mode)
            // can emit PlanProposed / PlanApprovalRequested. Without this the
            // composed adapter's event_tx stays None and plan approval cards
            // are silently dropped. `None` (headless/eval) stays valid.
            if let Some(ref tx) = ctx.domain_tx {
                adapter.set_event_tx(tx.clone());
            }
            // Story 11.1 — wire the shared memory slot so the `remember` builtin
            // tool can append notable entries via MemoryPort::store.
            adapter.set_memory(
                Arc::clone(&ctx.memory_slot),
                Arc::clone(&ctx.memory_write_gate),
            );
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
            adapter.set_skill_cache(Arc::clone(&ctx.skill_cache));
            // Wire the event bus so plan tools (propose_plan / exit_plan_mode)
            // can emit PlanProposed / PlanApprovalRequested. Without this the
            // composed adapter's event_tx stays None and plan approval cards
            // are silently dropped. `None` (headless/eval) stays valid.
            // `composite` reuses this profile, so it inherits the wiring.
            if let Some(ref tx) = ctx.domain_tx {
                adapter.set_event_tx(tx.clone());
            }
            // Story 11.1 — wire the shared memory slot for the `remember` tool.
            adapter.set_memory(
                Arc::clone(&ctx.memory_slot),
                Arc::clone(&ctx.memory_write_gate),
            );
            #[cfg(feature = "meta-search")]
            if let Some(ref engine) = ctx.meta_search_engine {
                adapter.set_meta_search_engine(Arc::clone(engine));
            }
            Ok(Arc::new(adapter))
        }
        #[cfg(feature = "mcp")]
        "composite" => {
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
    config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn ChannelPort>, AdapterCompositionError> {
    #[cfg(not(any(feature = "telegram", feature = "slack")))]
    let _ = (config, ctx);
    match name {
        "terminal" => Ok(Arc::new(NoOpChannel)),
        // Daemon-only like Telegram: outside the daemon nothing is read and
        // nothing starts. Inside it, the tokens come from the environment only
        // and are Debug-redacted downstream (best-effort zeroize, not a wipe of
        // every copy).
        #[cfg(feature = "slack")]
        "slack" => {
            if ctx.channel_turn_tx.is_none()
                && crate::domain::services::adapter_catalog::AdapterCatalog::is_daemon_only(
                    PortDimension::Channels,
                    name,
                )
            {
                return Ok(Arc::new(NoOpChannel));
            }
            use crate::domain::services::app_credentials::{SLACK_APP_TOKEN_ENV, SLACK_BOT_TOKEN_ENV};
            crate::adapters::channel::slack::compose(
                config,
                crate::infrastructure::utils::env_var_trimmed(SLACK_APP_TOKEN_ENV),
                crate::infrastructure::utils::env_var_trimmed(SLACK_BOT_TOKEN_ENV),
                ctx.channel_turn_tx.clone(),
            )
        }
        #[cfg(not(feature = "slack"))]
        "slack" => Err(AdapterCompositionError::MissingComposeContext {
            port: PortDimension::Channels,
            name: name.to_string(),
            missing_field:
                "slack feature not compiled — profile validator should have rewritten this to 'terminal'"
                    .into(),
        }),
        #[cfg(feature = "telegram")]
        "telegram" => {
            if ctx.channel_turn_tx.is_none()
                && crate::domain::services::adapter_catalog::AdapterCatalog::is_daemon_only(PortDimension::Channels, name)
            {
                return Ok(Arc::new(NoOpChannel));
            }
            use crate::adapters::channel::telegram::TelegramChannelAdapter;
            let conf = config.ok_or_else(|| AdapterCompositionError::MissingComposeContext {
                port: PortDimension::Channels,
                name: "telegram".into(),
                missing_field: "bot_token and allowed_chat_ids required in [channels.config]".into(),
            })?;
            let token = crate::infrastructure::utils::env_var_trimmed(crate::domain::services::app_credentials::TELEGRAM_BOT_TOKEN_ENV)
                .or_else(|| {
                    conf.get("bot_token")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .ok_or_else(|| AdapterCompositionError::MissingComposeContext {
                    port: PortDimension::Channels,
                    name: "telegram".into(),
                    missing_field:
                        "bot_token required for telegram adapter (set in [channels.config] or TELEGRAM_BOT_TOKEN env var)"
                            .into(),
                })?;
            let allowed_values = conf
                .get("allowed_chat_ids")
                .and_then(|v| v.as_array())
                .ok_or_else(|| AdapterCompositionError::MissingComposeContext {
                    port: PortDimension::Channels,
                    name: "telegram".into(),
                    missing_field: "allowed_chat_ids required in [channels.config]".into(),
                })?;
            let allowed_chat_ids: Vec<i64> = allowed_values
                .iter()
                .map(|v| {
                    v.as_integer()
                        .ok_or_else(|| AdapterCompositionError::MissingComposeContext {
                            port: PortDimension::Channels,
                            name: "telegram".into(),
                            missing_field: "allowed_chat_ids must contain only integers".into(),
                        })
                })
                .collect::<Result<_, _>>()?;
            let turn_tx =
                ctx.channel_turn_tx
                    .clone()
                    .ok_or_else(|| AdapterCompositionError::MissingComposeContext {
                        port: PortDimension::Channels,
                        name: "telegram".into(),
                        missing_field:
                            "channel_turn_tx not wired (daemon-only; TUI mode cannot use telegram adapter)"
                                .into(),
                    })?;
            Ok(Arc::new(TelegramChannelAdapter::new(
                token,
                allowed_chat_ids,
                turn_tx,
            )?))
        }
        #[cfg(not(feature = "telegram"))]
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
            available: vec!["terminal".into(), "telegram".into(), "slack".into()],
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
        // The daemon owns the real cron loop; other surfaces use the no-op and a notice.
        name if crate::domain::services::adapter_catalog::AdapterCatalog::is_daemon_only(
            PortDimension::Scheduler,
            name,
        ) =>
        {
            Ok(Arc::new(NoOpScheduler))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Scheduler,
            name: other.to_string(),
            available: vec!["none".into(), "cron".into()],
        }),
    }
}

pub fn build_context(
    name: &str,
    config: Option<&toml::Value>,
    ctx: &ComposeContext,
) -> Result<Arc<dyn ContextPort>, AdapterCompositionError> {
    match name {
        // Story 11.4 — the real Content-tier context-assembly adapter. Captures
        // the shared `memory_slot` (read via `load_full()` so warm profile swaps
        // are seen) + the project context. `"daily"` is an alias of `"default"`
        // (same adapter; the daily-vs-memory split is derived from entry recency,
        // not the profile name). With a `noop` memory profile (e.g. `base`) the
        // adapter degrades to an empty bundle — AC5.
        "default" | "daily" => {
            // Read adapter-local config off the per-dimension `_config` seam
            // (11.3b pattern); `warn` on a parse error rather than silently
            // dropping to defaults.
            let cfg: crate::adapters::memory_context::ContextAssemblyConfig = config
                .and_then(|v| match v.clone().try_into() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(
                            port = ?PortDimension::Context,
                            adapter = %name,
                            error = %e,
                            "context config parse failed — using defaults"
                        );
                        None
                    }
                })
                .unwrap_or_default();
            let adapter = crate::adapters::memory_context::MemoryContextAdapter::new(
                Arc::clone(&ctx.memory_slot),
                ctx.project_context.clone(),
                cfg,
            );
            Ok(Arc::new(adapter))
        }
        // Explicit opt-out — the dormant no-op (no injection at all).
        "noop" => Ok(Arc::new(NoOpContext)),
        // Story 18.4a (FR151) — local memory context **plus** peer-origin
        // context, both behind the one `ContextPort` slot.
        //
        // ⚑ This arm is the load-bearing half of FR151, and it is a **core
        // change**. Before it, `build_context` returned exactly one adapter, so
        // selecting the peer provider would have *replaced* the operator's
        // local memory context rather than adding to it — a feature that
        // silently deletes another feature. ⛔ FR151's "zero core change" clause
        // is false and is not repeated; what is true is that the change is
        // small and precedented.
        //
        // The shape is copied from the toolset dimension's own `"composite"`
        // arm above, including its recursive-inner-build: the inner adapter is
        // produced by calling this factory again, so a `default` context built
        // here and one built directly cannot drift.
        "composite" => {
            let local = build_context("default", config, ctx)?;
            let peer = Arc::new(crate::adapters::peer_context::PeerContextProvider::new(
                Arc::clone(&ctx.peer_topic_store),
            ));
            Ok(Arc::new(
                crate::adapters::composite_context_adapter::CompositeContextAdapter::new(
                    local, peer,
                ),
            ))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Context,
            name: other.to_string(),
            available: vec![
                "default".into(),
                "daily".into(),
                "noop".into(),
                "composite".into(),
            ],
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

/// Story 11.6 — build the Message-tier context assembler from
/// `[assembler].strategy`: `"passthrough"` (default, behaviour-preserving) or
/// `"windowing"` (Algorithm A+ within-session grouped windowing).
///
/// Mirrors the `[tools].exposure` precedent: only known strategy *names* are
/// accepted, and NO `GroupingConfig` threshold is threaded (FR121 / ADR-11-2
/// "zero user-visible settings"). `startup::validate_assembler_strategy` is the
/// primary gate (actionable error); this `Err` arm is defense-in-depth.
pub fn build_context_assembler(
    ctx: &ComposeContext,
) -> Result<Arc<dyn crate::domain::ports::ContextAssemblerPort>, AdapterCompositionError> {
    match ctx.assembler.trim() {
        "passthrough" => Ok(Arc::new(
            crate::infrastructure::context::StaticPassthroughAssembler,
        )),
        "windowing" => Ok(Arc::new(
            crate::infrastructure::context::WindowingAssembler::default(),
        )),
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Context,
            name: other.to_string(),
            available: vec!["passthrough".into(), "windowing".into()],
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
            name: "meta-search skill exposure requires the `meta-search` cargo feature; rebuild with `--features meta-search`".into(),
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

/// Story 12.1a — compose ONLY the memory port for the headless daemon body.
///
/// Reuses [`build_memory`] (no duplicated adapter logic — AC9 of Story 12.0 /
/// "harden the sink, not the call-sites"). 12.1a is a lifecycle skeleton with no
/// message runtime (channels = Stories 12.2/12.3/12.4), so the daemon composes
/// only the port whose graceful-shutdown / daily-reset flush seam
/// (AC-12-1a-3/5) must route through 12.0's hardened `prepare_detach` sink — NOT
/// the full `AgentCore` (a headless daemon needs no provider/terminal/sandbox
/// layer in 12.1a; building them would pull network + TUI machinery into the
/// skeleton for zero functional benefit). See Story 12.1a Completion Notes
/// §"Headless composition scope" for this verified reconciliation vs. the
/// literal Task-3 recipe.
///
/// `domain_tx` is `None`: headless, with no event-bus consumers wired in 12.1a.
/// **Contract:** no code path triggered during `build_daemon_memory` may send
/// events through `domain_tx`. If a future memory adapter emits events during
/// construction, this helper must either wire a real sender or document the
/// silent-drop. Currently safe — `build_memory` does not emit events.
pub fn build_daemon_memory(
    workspace_path: &std::path::Path,
    memory_adapter: &str,
) -> Result<Arc<dyn MemoryPort>, AdapterCompositionError> {
    use crate::adapters::sandbox::NoOpSandbox;
    let ctx = ComposeContext {
        workspace_path: workspace_path.to_path_buf(),
        config_dir: std::path::PathBuf::new(),
        project_context: ProjectContext::empty(),
        storage: Arc::new(crate::adapters::noop::NoOpStorage) as Arc<dyn StoragePort>,
        skill_activator: Arc::new(SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new(
            crate::infrastructure::skill_cache::SkillCacheConfig::default(),
        )),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: crate::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>
        )),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(NoOpMemory) as Arc<dyn MemoryPort>
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(crate::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: crate::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    };
    build_memory(memory_adapter, None, &ctx)
}

/// Story C-3 (AD-1) — what the daemon composes of the profile's MCP
/// configuration.
#[derive(Debug, Clone)]
pub struct DaemonMcpTools {
    pub mcp_servers: Vec<crate::domain::models::McpServerSpec>,
    pub include_builtin: bool,
}

/// Story C-3 (AD-1) — the parity rule. The daemon composes the resolved MCP
/// servers (and `include_builtin_tools`) exactly when the profile's OWN tools
/// adapter is `"composite"`, as the TUI does; every other adapter — including
/// the A2A upgrade of a non-composite adapter, which is decided later and
/// stays MCP-free — keeps today's daemon: no servers, builtin tools on.
pub fn daemon_mcp_tools(
    picked_tools_adapter: &str,
    resolved_mcp: Vec<crate::domain::models::McpServerSpec>,
    include_builtin: bool,
) -> DaemonMcpTools {
    if picked_tools_adapter == "composite" {
        DaemonMcpTools {
            mcp_servers: resolved_mcp,
            include_builtin,
        }
    } else {
        DaemonMcpTools {
            mcp_servers: Vec::new(),
            include_builtin: true,
        }
    }
}
/// Construct a daemon ComposeContext (Story 12.2b): like `build_daemon_memory`'s
/// minimal context but with **real** session storage and the daemon's
/// per-activation `domain_tx` wired, so `build_tools`/`build_context_assembler`
/// compose live (connection-holding) adapters that emit capability events onto
/// the daemon bus. Builtin tools only (no MCP servers) unless the factory passes
/// the profile's servers under the parity rule ([`daemon_mcp_tools`]).
#[cfg(unix)]
pub(crate) fn daemon_compose_context(
    workspace_path: &std::path::Path,
    config_dir: &std::path::Path,
    storage: Arc<dyn StoragePort>,
    domain_tx: tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
    assembler: String,
    channel_turn_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::domain::models::ChannelTurnRequest>,
    >,
    // Story 18.4a — threaded rather than constructed here. The daemon rebuilds
    // this context on first activity, so a store minted inside would be a
    // second log the transport never writes to.
    peer_topic_store: Arc<crate::adapters::rap::PeerTopicStore>,
    // Story C-3 (AD-1) — `Vec::new()`/`true` for every context that builds no
    // tools (eager persona context, channel context); the factory passes the
    // parity-rule result.
    mcp_servers: Vec<crate::domain::models::McpServerSpec>,
    include_builtin_tools: bool,
) -> ComposeContext {
    use crate::adapters::sandbox::NoOpSandbox;
    ComposeContext {
        workspace_path: workspace_path.to_path_buf(),
        config_dir: config_dir.to_path_buf(),
        project_context: ProjectContext::empty(),
        storage,
        skill_activator: Arc::new(SkillActivator::new()),
        mcp_servers,
        include_builtin_tools,
        domain_tx: Some(domain_tx),
        channel_turn_tx,
        tool_exposure: "static-full".into(),
        assembler,
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new(
            crate::infrastructure::skill_cache::SkillCacheConfig::default(),
        )),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: crate::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>
        )),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(NoOpMemory) as Arc<dyn MemoryPort>
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store,
        #[cfg(feature = "meta-search")]
        search_config: crate::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    }
}

/// 18.9b-a D1 (code review, party-mode ruling 2026-09-06, owner-approved):
/// a daemon whose resolved tools adapter is not the composite cannot host the
/// a2a provider install — the install below the toolset build is
/// downcast-guarded, so configured peers silently never surfaced. FR46 holds
/// in the daemon on every configuration that names peers, so the adapter
/// upgrades to `"composite"` (the daemon compose ctx carries zero MCP
/// servers, making the upgraded shape the exact one `coding`-profile daemons
/// already run) and the substrate widening is disclosed in the returned line.
/// Impossible configurations (non-`a2a` builds with peers) are refused before
/// composition by the AC3(a) feature guard in `startup.rs`.
#[cfg(all(unix, feature = "a2a"))]
fn resolve_daemon_tools_adapter(tools_name: &str, a2a_peers: usize) -> (String, Option<String>) {
    if a2a_peers > 0 && tools_name != "composite" {
        (
            "composite".to_string(),
            Some(format!(
                "tools adapter '{tools_name}' cannot host {a2a_peers} configured A2A peer(s); \
                 composed the composite toolset instead (builtin-full substrate, no MCP servers)"
            )),
        )
    } else {
        (tools_name.to_string(), None)
    }
}

/// Compose the daemon's runtime FACTORY, not an eager live core (Story 12.2b
/// AC1/AC1b — Q7-SETTLED).
///
/// Eagerly composes ONLY the cheap, long-lived, connection-free parts (memory via
/// [`build_daemon_memory`], real session storage, a `Normal`-mode security policy
/// — **never `Yolo`** headless per AC6, and the persona). It captures a
/// [`TurnRuntimeFactory`](crate::adapters::daemon::runtime::TurnRuntimeFactory)
/// that builds the live, connection-holding parts (provider via
/// [`init_provider_layer`](crate::infrastructure::startup::init_provider_layer),
/// tools via [`build_tools`], the Message-tier assembler via
/// [`build_context_assembler`], a deny-by-default [`ApprovalRuntime`], the
/// `ToolScheduler`, ledger/telemetry/plan-injector) on **first activity** behind
/// the core's single `OnceCell`. An idle daemon therefore holds no live provider
/// connection (NFR46). Reuses the SAME `build_*` factories startup uses — no
/// forked composition.
#[cfg(unix)]
pub fn build_daemon_core(
    workspace: &std::path::Path,
    config_dir: &std::path::Path,
    config: Arc<arc_swap::ArcSwap<crate::domain::models::AppConfig>>,
    profile_selection: ProfileSelection,
    memory_adapter: &str,
    domain_tx: tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
    channel_turn_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::domain::models::ChannelTurnRequest>,
    >,
    _node_tree: crate::infrastructure::subagent::NodeTree,
    _node_journal: Arc<crate::infrastructure::subagent::NodeJournal>,
    #[cfg(feature = "a2a")] a2a_egress: Arc<crate::adapters::a2a::egress::A2aEgress>,
    team_proposals: Arc<crate::adapters::daemon::team_proposal::TeamProposalRegistry>,
    resolved_mcp: DaemonMcpTools,
) -> Result<crate::adapters::daemon::runtime::DaemonCore, AdapterCompositionError> {
    use crate::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::security_adapter::HeadlessSecurityAdapter;

    let snapshot = config.load();
    let assembler_name = snapshot.assembler.strategy.clone();
    let raw_capacity = snapshot.runtime.event_bus.raw_capacity;

    let pick = |dim: PortDimension, default: &str| -> String {
        profile_selection
            .dimensions
            .get(&dim)
            .map(|a| a.adapter.clone())
            .unwrap_or_else(|| default.to_string())
    };
    let persona_ref = profile_selection
        .dimensions
        .get(&PortDimension::Persona)
        .cloned()
        .unwrap_or(AdapterRef {
            adapter: "coding".to_string(),
            _config: None,
        });
    // 18.9b-a D1 — see `resolve_daemon_tools_adapter`. Resolved once per boot,
    // outside the first-activity factory below, so the disclosure fires once.
    let picked_tools_name = pick(PortDimension::Tools, "builtin-only");
    // Story C-3 (AD-1) — the parity rule is applied to the profile's OWN tools
    // adapter, before the A2A upgrade below can change it.
    let configured_mcp = resolved_mcp.mcp_servers.len();
    let daemon_mcp = daemon_mcp_tools(
        &picked_tools_name,
        resolved_mcp.mcp_servers,
        resolved_mcp.include_builtin,
    );
    // Story C-3 (AD-10) — one announce per boot. A non-composite profile cannot
    // host MCP servers, so say so rather than staying silent (the TUI has the
    // same rule); the A2A upgrade notice below is unchanged.
    if !daemon_mcp.mcp_servers.is_empty() {
        let mut ids: Vec<&str> = daemon_mcp
            .mcp_servers
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        ids.sort_unstable();
        tracing::info!(
            "daemon composes {} MCP server(s) on first activity: {}",
            ids.len(),
            ids.join(", ")
        );
    } else if configured_mcp > 0 {
        tracing::info!(
            "daemon tools adapter '{}' hosts no MCP servers; {} configured MCP server(s) not composed",
            picked_tools_name,
            configured_mcp
        );
    }
    // Story C-3 (AD-3) — shared by the factory's start and `shutdown_mcp`.
    let admission = Arc::new(crate::adapters::daemon::mcp_admission::McpAdmission::default());
    #[cfg(feature = "a2a")]
    let (tools_name, daemon_tools_upgrade_notice) = resolve_daemon_tools_adapter(
        &picked_tools_name,
        a2a_egress.provider().peer_bindings().len(),
    );
    #[cfg(not(feature = "a2a"))]
    let tools_name = picked_tools_name;
    #[cfg(not(feature = "a2a"))]
    let _ = team_proposals;
    #[cfg(feature = "a2a")]
    if let Some(notice) = daemon_tools_upgrade_notice {
        tracing::warn!("daemon toolset upgraded to host A2A peers: {notice}");
    }
    let (context_name, context_config) = profile_selection
        .dimensions
        .get(&PortDimension::Context)
        .map(|adapter| (adapter.adapter.clone(), adapter._config.clone()))
        .unwrap_or_else(|| ("default".to_owned(), None));

    // ── Eager parts (cheap, connection-free) ────────────────────────────────
    let memory = build_daemon_memory(workspace, memory_adapter)?;
    let sessions_dir = crate::infrastructure::paths::sessions_dir(workspace);
    let storage: Arc<dyn StoragePort> = Arc::new(
        FileSystemStorage::with_workspace_root(sessions_dir.clone(), workspace.to_path_buf())
            .with_workspace_registrar(build_workspace_registrar()?),
    );
    // Headless security policy — permanently Normal (AC6).
    // `HeadlessSecurityAdapter` ignores `set_mode` so Yolo is structurally
    // unreachable, not just administratively avoided.
    let security: Arc<dyn SecurityPort> =
        Arc::new(HeadlessSecurityAdapter::new(workspace.to_path_buf()));
    // Story 18.4a — minted once, here, and shared by every later composition of
    // this daemon's context as well as by the verified-peer delivery front door.
    // ⛔ Not inside `daemon_compose_context`: that runs again on first activity,
    // so a store built there would be a second log the transport never writes.
    let peer_topic_store = Arc::new(crate::adapters::rap::PeerTopicStore::new());
    let eager_ctx = daemon_compose_context(
        workspace,
        config_dir,
        storage.clone(),
        domain_tx.clone(),
        assembler_name.clone(),
        channel_turn_tx.clone(),
        Arc::clone(&peer_topic_store),
        Vec::new(),
        true,
    );
    let persona = compose_persona(
        &persona_ref,
        &eager_ctx.project_context,
        &config_dir.join("personas"),
    )?;

    // ── The lazy factory (built on first activity) ──────────────────────────
    let factory = {
        let workspace = workspace.to_path_buf();
        let config = config.clone();
        let storage = storage.clone();
        let security = security.clone();
        let domain_tx = domain_tx.clone();
        let channel_turn_tx = channel_turn_tx.clone();
        let factory_topic_store = Arc::clone(&peer_topic_store);
        let factory_config_dir = config_dir.to_path_buf();
        // AD-9 — the persona file is read once, at boot; the factory reuses the
        // eager `Arc` so a file edited or deleted afterwards cannot change it.
        let factory_persona = Arc::clone(&persona);
        let factory_mcp = daemon_mcp.clone();
        let factory_admission = Arc::clone(&admission);
        #[cfg(feature = "a2a")]
        let a2a_egress = a2a_egress.clone();
        #[cfg(feature = "mcp")]
        let task_node_tree = _node_tree;
        #[cfg(feature = "mcp")]
        let task_node_journal = _node_journal;
        Box::new(
            move || -> Result<Arc<DaemonTurnRuntime>, AdapterCompositionError> {
                let ctx = daemon_compose_context(
                    &workspace,
                    &factory_config_dir,
                    storage.clone(),
                    domain_tx.clone(),
                    assembler_name.clone(),
                    channel_turn_tx.clone(),
                    Arc::clone(&factory_topic_store),
                    factory_mcp.mcp_servers.clone(),
                    factory_mcp.include_builtin,
                );
                // Live, connection-holding parts — first activity only.
                let provider: Arc<dyn StreamingProvider> = {
                    let layer = crate::infrastructure::startup::init_provider_layer(&config.load());
                    crate::infrastructure::model_discovery::spawn_headless(
                        layer.discovery_targets,
                        layer.registry.clone(),
                    );
                    layer.router as Arc<dyn StreamingProvider>
                };
                let tools = build_tools(&tools_name, None, &ctx)?;
                #[cfg(feature = "a2a")]
                if let Some(composite) = tools.as_any().downcast_ref::<
                    crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter,
                >() {
                    a2a_egress.install(composite, Some(team_proposals.clone()));
                }
                // Story 17.5a — deferred MCP Tasks injection preserves the
                // daemon's lazy provider/tool construction while connecting
                // every live MCP client to the shared durable node tree.
                #[cfg(feature = "mcp")]
                let mcp_task_runtimes: Vec<
                    Arc<crate::adapters::mcp::task_driver::McpTaskRuntime>,
                > = if let Some(composite) = tools.as_any().downcast_ref::<
                    crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter,
                >() {
                    let task_nodes: Arc<dyn crate::domain::ports::TaskNodes> =
                        Arc::new(task_node_tree.clone());
                    let supervised: Arc<dyn crate::domain::ports::SupervisedNodes> =
                        Arc::new(task_node_tree.clone());
                    let room: Arc<dyn crate::domain::ports::RoomJournal> =
                        Arc::new(crate::infrastructure::subagent::NodeRoomJournal::new(
                            task_node_journal.clone(),
                            Some(domain_tx.clone()),
                        ));
                    let task_clock: Arc<dyn crate::domain::clock::Clock> =
                        Arc::new(crate::domain::clock::SystemClock::default());
                    let task_runtime = Arc::new(
                        crate::adapters::mcp::task_driver::McpTaskRuntime::new(
                            task_nodes,
                            supervised,
                            room.clone(),
                            task_clock,
                        ),
                    );
                    // 17.5b (Task 6): wire the input-request artifact sink so
                    // a `Waiting` MCP task files a visible ticket (AC3).
                    let artifact_store: Arc<dyn crate::domain::ports::ArtifactStore> = Arc::new(
                        crate::adapters::artifact::FileSystemArtifactStore::new(
                            &ctx.workspace_path,
                        ),
                    );
                    let artifact_host = crate::domain::models::HostBinding::new(
                        "local",
                        format!("workspace:{}", ctx.workspace_path.display()),
                    );
                    let coordinator_authority = crate::domain::models::CapabilityToken::r1_root(
                        crate::domain::models::AgentId::root(),
                    );
                    let sink: Arc<dyn crate::domain::ports::ArtifactSink> = Arc::new(
                        crate::infrastructure::subagent::JournalArtifactSink::new(
                            artifact_store,
                            room,
                            coordinator_authority.id,
                            artifact_host,
                            crate::domain::models::AgentId::local_operator(),
                        ),
                    );
                    task_runtime.set_artifact_sink(sink);
                    for client in composite.mcp_clients() {
                        client.set_task_runtime(Arc::clone(&task_runtime));
                    }
                    vec![task_runtime]
                } else {
                    Vec::new()
                };
                let context_assembler = build_context_assembler(&ctx)?;
                // Story 18.4a (code-review D2): the daemon's interactive
                // attach-mode turn owns context injection, so compose the
                // bundle-tier port from the same `ctx` whose topic store the
                // verified-peer delivery handler fills.
                let context = build_context(&context_name, context_config.as_ref(), &ctx)?;
                // Deny-by-default approval (AC6): NoOp persistence so no stale
                // "always-allow" rule can undermine the unattended deny policy.
                let approval = crate::domain::services::approval_runtime::ApprovalRuntime::new(
                    raw_capacity,
                    Arc::new(crate::adapters::noop::NoOpApprovalPersistence),
                );
                let tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
                    security.clone(),
                    tools.clone(),
                    approval.clone(),
                    raw_capacity,
                );
                let fs_storage = Arc::new(
                    FileSystemStorage::with_workspace_root(
                        crate::infrastructure::paths::sessions_dir(&workspace),
                        workspace.clone(),
                    )
                    .with_workspace_registrar(build_workspace_registrar()?),
                );
                // Story C-3 (AD-3) — start the MCP servers LAST: every fallible
                // factory step above has succeeded, so an `Err` can no longer
                // leave children running behind a factory that never produced a
                // runtime. The admission makes check-closed + start one atomic
                // step against `DaemonCore::shutdown_mcp`; when it is closed
                // nothing starts and the composite is marked settled.
                if let Some(composite) = tools.as_any().downcast_ref::<
                    crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter,
                >() {
                    if !factory_admission.start_if_open(&tools, || composite.start_mcp_connections()) {
                        composite.mark_mcp_settled();
                    }
                }
                Ok(Arc::new(DaemonTurnRuntime {
                    recipient_context: Arc::new(Default::default()),
                    provider,
                    app_config: config.clone(),
                    security: security.clone(),
                    tools,
                    tool_scheduler,
                    persona: Arc::clone(&factory_persona),
                    context_assembler: Arc::new(arc_swap::ArcSwap::from_pointee(Some(
                        context_assembler,
                    ))),
                    context: Arc::new(arc_swap::ArcSwap::from_pointee(context)),
                    storage: storage.clone(),
                    fs_storage,
                    usage_ledger: Arc::new(crate::adapters::ledger::FileUsageLedger::new()),
                    telemetry: crate::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
                    plan_injector: Arc::new(
                        crate::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
                    ),
                    approval,
                    workspace: workspace.clone(),
                    #[cfg(feature = "mcp")]
                    mcp_task_runtimes,
                }))
            },
        )
    };

    Ok(DaemonCore::new(
        workspace.to_path_buf(),
        config,
        memory,
        storage,
        security,
        persona,
        peer_topic_store,
        factory,
    )
    .with_mcp_admission(admission))
}

/// Composition for CLI one-shot commands (`rustain ask`, future `rustain run`).
/// Human-present-but-headless posture: uses `SecurityAdapter` (NOT
/// `HeadlessSecurityAdapter`) so `--yolo` can flip the mode to `Yolo`.
/// Shares the `run_turn` engine with TUI and daemon — behavioral identity.
///
/// Durability tripwire: if this function ever sprouts an `if unattended` branch,
/// the seam has drifted toward the daemon — keep CLI and daemon as two flat,
/// opinionated compositions over the shared `run_turn`.
pub struct CliCore {
    pub provider: Arc<dyn StreamingProvider>,
    pub security: Arc<dyn SecurityPort>,
    pub tools: Arc<dyn ToolSetPort>,
    pub tool_scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    pub approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    pub storage: Arc<dyn StoragePort>,
    pub event_tx: tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    pub ledger: Arc<dyn crate::domain::ports::UsageLedgerPort>,
    /// Story C-2 — the persona composed once at startup (profile persona file
    /// or `--persona`).
    pub persona: Arc<dyn PersonaPort>,
}

pub struct AcpCore {
    pub provider: Arc<dyn StreamingProvider>,
    pub security: Arc<dyn SecurityPort>,
    pub tools: Arc<dyn ToolSetPort>,
    pub tool_scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    pub approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    pub storage: Arc<dyn StoragePort>,
    pub event_tx: tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    pub ledger: Arc<dyn crate::domain::ports::UsageLedgerPort>,
    pub registry: Arc<crate::adapters::provider::ProviderRegistry>,
    pub router: Arc<crate::adapters::provider::ProviderRouter>,
    pub skill_activator: Arc<SkillActivator>,
    /// Tier-1 skill catalog port (FR41). `None` = no catalog on this core.
    pub skill_exposure: Option<Arc<dyn crate::domain::ports::SkillExposurePort>>,
    /// Story C-2 — the persona composed once at startup (profile persona file
    /// or `--persona`); every ACP session core shares it.
    pub persona: Arc<dyn PersonaPort>,
}

impl From<CliCore> for AcpCore {
    fn from(core: CliCore) -> Self {
        let CliCore {
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            event_tx,
            event_rx,
            ledger,
            persona,
        } = core;
        let provider_id = provider.provider_id();
        let registry = Arc::new(crate::adapters::provider::ProviderRegistry::new());
        registry.register_arc(provider.clone());
        let router = Arc::new(crate::adapters::provider::ProviderRouter::new(provider_id));
        router.register(provider.clone());
        Self {
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            event_tx,
            event_rx,
            ledger,
            registry,
            router,
            skill_activator: Arc::new(SkillActivator::new()),
            // No skill source (empty activator above) → no catalog.
            skill_exposure: None,
            persona,
        }
    }
}

pub fn build_cli_core(
    app_config: &crate::domain::models::AppConfig,
    workspace: &std::path::Path,
    yolo: bool,
    persona: Arc<dyn PersonaPort>,
) -> Result<CliCore, AdapterCompositionError> {
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::NoOpApprovalPersistence;
    use crate::adapters::sandbox::NoOpSandbox;
    use crate::adapters::security_adapter::SecurityAdapter;

    let raw_capacity = app_config.runtime.event_bus.raw_capacity;

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

    let provider_layer = crate::infrastructure::startup::init_provider_layer(app_config);
    let provider: Arc<dyn StreamingProvider> = provider_layer.router as Arc<dyn StreamingProvider>;

    let security: Arc<dyn SecurityPort> = {
        let adapter = SecurityAdapter::new(workspace.to_path_buf());
        if yolo {
            adapter.set_mode(crate::domain::models::PermissionMode::Yolo);
        }
        Arc::new(adapter)
    };

    let sessions_dir = crate::infrastructure::paths::sessions_dir(workspace);
    let storage: Arc<dyn StoragePort> = Arc::new(
        FileSystemStorage::with_workspace_root(sessions_dir, workspace.to_path_buf())
            .with_workspace_registrar(build_workspace_registrar()?),
    );

    let ctx = ComposeContext {
        workspace_path: workspace.to_path_buf(),
        config_dir: std::path::PathBuf::new(),
        project_context: ProjectContext::empty(),
        storage: storage.clone(),
        skill_activator: Arc::new(SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: Some(event_tx.clone()),
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new(
            crate::infrastructure::skill_cache::SkillCacheConfig::default(),
        )),
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
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(crate::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: crate::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    };

    let tools = build_tools("builtin-only", None, &ctx)?;

    let approval = crate::domain::services::approval_runtime::ApprovalRuntime::new(
        raw_capacity,
        Arc::new(NoOpApprovalPersistence),
    );

    let tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval.clone(),
        raw_capacity,
    );

    let ledger: Arc<dyn crate::domain::ports::UsageLedgerPort> =
        Arc::new(crate::adapters::ledger::FileUsageLedger::new());

    Ok(CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger,
        persona,
    })
}

pub fn build_acp_core(
    app_config: &crate::domain::models::AppConfig,
    workspace: &std::path::Path,
    yolo: bool,
    mcp_servers: &[crate::domain::models::McpServerSpec],
    persona: Arc<dyn PersonaPort>,
) -> Result<AcpCore, AdapterCompositionError> {
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::NoOpApprovalPersistence;
    use crate::adapters::sandbox::NoOpSandbox;
    use crate::adapters::security_adapter::SecurityAdapter;

    let raw_capacity = app_config.runtime.event_bus.raw_capacity;
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

    let provider_layer = crate::infrastructure::startup::init_provider_layer(app_config);
    crate::infrastructure::model_discovery::spawn_headless(
        provider_layer.discovery_targets,
        provider_layer.registry.clone(),
    );
    let router = provider_layer.router;
    let registry = provider_layer.registry;
    let provider: Arc<dyn StreamingProvider> = router.clone() as Arc<dyn StreamingProvider>;

    let security: Arc<dyn SecurityPort> = {
        let adapter = SecurityAdapter::new(workspace.to_path_buf());
        if yolo {
            adapter.set_mode(crate::domain::models::PermissionMode::Yolo);
        }
        Arc::new(adapter)
    };

    let sessions_dir = crate::infrastructure::paths::sessions_dir(workspace);
    let storage: Arc<dyn StoragePort> = Arc::new(
        FileSystemStorage::with_workspace_root(sessions_dir, workspace.to_path_buf())
            .with_workspace_registrar(build_workspace_registrar()?),
    );

    let skill_registry = crate::adapters::skill_registry::SkillRegistry::discover(
        workspace,
        dirs::home_dir().as_deref(),
        &app_config.skills.disabled,
    );
    let skill_activator = Arc::new(SkillActivator::with_registry(Arc::new(
        tokio::sync::RwLock::new(skill_registry),
    )));

    let ctx = ComposeContext {
        workspace_path: workspace.to_path_buf(),
        config_dir: std::path::PathBuf::new(),
        project_context: ProjectContext::empty(),
        storage: storage.clone(),
        skill_activator: skill_activator.clone(),
        mcp_servers: mcp_servers.to_vec(),
        include_builtin_tools: true,
        domain_tx: Some(event_tx.clone()),
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new(
            crate::infrastructure::skill_cache::SkillCacheConfig::default(),
        )),
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
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(crate::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: crate::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    };

    let tools: Arc<dyn ToolSetPort> = {
        #[cfg(feature = "mcp")]
        {
            if ctx.mcp_servers.is_empty() {
                build_tools("builtin-full", None, &ctx)?
            } else {
                let builtin = build_tools("builtin-full", None, &ctx)?;
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
                let adapter =
                    crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter::new(
                        builtin,
                        mcp_clients,
                        ctx.mcp_servers.clone(),
                        ctx.include_builtin_tools,
                        ctx.domain_tx.clone(),
                        Some(ctx.skill_activator.clone()),
                        None,
                    );
                adapter.start_mcp_connections();
                Arc::new(adapter) as Arc<dyn ToolSetPort>
            }
        }
        #[cfg(not(feature = "mcp"))]
        {
            if !ctx.mcp_servers.is_empty() {
                // Fail LOUD rather than silently dropping. Accepting the
                // session while ignoring every forwarded server would let the
                // client (Zed) believe its MCP tools are available, then fail
                // every tool call at prompt time — a capability lie by
                // omission. Surface an error on session/new instead. (Only
                // reachable on a non-default build: `mcp` is on by default.)
                return Err(AdapterCompositionError::UnknownAdapter {
                    port: PortDimension::Tools,
                    name: format!(
                        "rustain was built without the `mcp` cargo feature, but \
                         session/new forwarded {} MCP stdio server(s); rebuild with \
                         `--features mcp` (on by default) to forward MCP servers",
                        ctx.mcp_servers.len()
                    ),
                    available: vec!["builtin-full (no mcp feature compiled)".into()],
                });
            }
            build_tools("builtin-full", None, &ctx)?
        }
    };
    let approval = crate::domain::services::approval_runtime::ApprovalRuntime::new(
        raw_capacity,
        Arc::new(NoOpApprovalPersistence),
    );
    let tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval.clone(),
        raw_capacity,
    );
    let ledger: Arc<dyn crate::domain::ports::UsageLedgerPort> =
        Arc::new(crate::adapters::ledger::FileUsageLedger::new());
    // FR41 — the Tier-1 skill catalog's port, from the same `ComposeContext` the
    // toolset was built from (ACP's exposure kind is fixed; see
    // DF-FR41-ACP-EXPOSURE-KIND-HARDCODED).
    let skill_exposure = build_skill_exposure(&ProfileSelection::default(), &ctx)?;

    Ok(AcpCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger,
        registry,
        router,
        skill_activator,
        skill_exposure,
        persona,
    })
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
            config_dir: PathBuf::new(),
            project_context: ProjectContext::empty(),
            storage: Arc::new(NoOpStorage) as Arc<dyn StoragePort>,
            skill_activator: Arc::new(SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            channel_turn_tx: None,
            tool_exposure: "static-full".into(),
            assembler: "passthrough".into(),
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
            memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
            peer_topic_store: Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            #[cfg(feature = "meta-search")]
            search_config: crate::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
            a2a_peers: Vec::new(),
        }
    }

    // ── Context-assembler selection (Story 11.6) ──

    #[test]
    fn test_build_context_assembler_passthrough_is_default() {
        let mut ctx = test_compose_ctx();
        ctx.assembler = "passthrough".into();
        assert!(build_context_assembler(&ctx).is_ok());
    }

    #[test]
    fn test_build_context_assembler_windowing_selects() {
        let mut ctx = test_compose_ctx();
        ctx.assembler = "windowing".into();
        assert!(build_context_assembler(&ctx).is_ok());
    }

    #[test]
    fn test_build_context_assembler_unknown_errors() {
        let mut ctx = test_compose_ctx();
        ctx.assembler = "nope".into();
        match build_context_assembler(&ctx) {
            Err(AdapterCompositionError::UnknownAdapter {
                name, available, ..
            }) => {
                assert_eq!(name, "nope");
                assert!(available.contains(&"windowing".to_string()));
            }
            Err(other) => panic!("expected UnknownAdapter, got {other:?}"),
            Ok(_) => panic!("expected UnknownAdapter error, got Ok"),
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
            _ => panic!("expected UnknownAdapter"),
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
            _ => panic!("expected UnknownAdapter"),
        }
    }

    #[test]
    fn story21_14_the_placeholder_warning_and_the_meta_search_rebuild_advice_are_pinned() {
        assert_eq!(
            PLACEHOLDER_ADAPTER_WARNING,
            "Using a placeholder adapter; the real adapter is not available yet"
        );
        #[cfg(not(feature = "meta-search"))]
        {
            let mut ctx = test_compose_ctx();
            ctx.skill_exposure = "meta-search".into();
            match build_skill_exposure(&ProfileSelection::default(), &ctx) {
                Err(AdapterCompositionError::UnknownAdapter { name, .. }) => assert_eq!(
                    name,
                    "meta-search skill exposure requires the `meta-search` cargo feature; rebuild with `--features meta-search`"
                ),
                _ => panic!("expected UnknownAdapter"),
            }
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
            _ => panic!("expected UnknownAdapter"),
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
            _ => panic!("expected UnknownAdapter"),
        }
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn test_build_tools_composite_empty_succeeds() {
        // ADR-10-5 S1: composite must compose with zero MCP servers — MCP is an
        // additive capability provider, not constitutive of the adapter. The default
        // `coding` profile relies on this (it selects composite with no MCP config).
        let ctx = test_compose_ctx();
        let result = build_tools("composite", None, &ctx);
        assert!(
            result.is_ok(),
            "composite must compose with zero MCP servers (ADR-10-5 S1)"
        );
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

    // ── 18.9b-a D1: daemon toolset upgrade predicate ──

    #[test]
    #[cfg(all(test, unix, feature = "a2a"))]
    fn d1_composite_with_peers_is_unchanged_and_silent() {
        let (name, notice) = resolve_daemon_tools_adapter("composite", 3);
        assert_eq!(name, "composite");
        assert!(notice.is_none());
    }

    #[test]
    #[cfg(all(test, unix, feature = "a2a"))]
    fn d1_non_composite_with_zero_peers_is_unchanged_and_silent() {
        let (name, notice) = resolve_daemon_tools_adapter("builtin-only", 0);
        assert_eq!(name, "builtin-only");
        assert!(
            notice.is_none(),
            "zero peers asked for nothing; silence is correct"
        );
    }

    #[test]
    #[cfg(all(test, unix, feature = "a2a"))]
    fn d1_builtin_only_with_peers_upgrades_with_disclosure() {
        let (name, notice) = resolve_daemon_tools_adapter("builtin-only", 2);
        assert_eq!(name, "composite");
        let notice = notice.expect("the upgrade must disclose the widening");
        assert!(
            notice.contains("builtin-only"),
            "names the adapter it upgraded from: {notice}"
        );
        assert!(
            notice.contains("composite"),
            "names what it composed instead: {notice}"
        );
        assert!(
            notice.contains("2"),
            "names the peer count that forced the upgrade: {notice}"
        );
    }

    #[test]
    #[cfg(all(test, unix, feature = "a2a"))]
    fn d1_builtin_full_with_peers_also_upgrades() {
        let (name, notice) = resolve_daemon_tools_adapter("builtin-full", 1);
        assert_eq!(name, "composite");
        assert!(notice.is_some());
    }

    #[test]
    #[cfg(all(test, unix, feature = "a2a", feature = "mcp"))]
    fn d1_upgraded_name_builds_the_installable_composite() {
        // Winston's identity row: the upgraded name, fed through the same
        // `build_tools` arm the daemon factory uses, yields the composite —
        // the exact construction `coding`-profile daemons run (ADR-10-5 S1
        // pins the zero-MPC compose; this pins the
        // decision → construction → installable chain the upgrade relies on).
        let (name, _notice) = resolve_daemon_tools_adapter("builtin-only", 1);
        assert_eq!(name, "composite");
        let ctx = test_compose_ctx();
        let tools = build_tools(&name, None, &ctx).expect("composite builds");
        assert!(
            tools
                .as_any()
                .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()
                .is_some(),
            "the upgraded toolset must be the composite that hosts the a2a install"
        );
    }

    // ── Channels tests ──

    #[test]
    fn test_build_channels_terminal() {
        let ctx = test_compose_ctx();
        assert!(build_channels("terminal", None, &ctx).is_ok());
    }

    #[test]
    #[cfg(not(feature = "telegram"))]
    fn test_build_channels_telegram_missing_context() {
        let ctx = test_compose_ctx();
        let result = build_channels("telegram", None, &ctx);
        match result {
            Err(AdapterCompositionError::MissingComposeContext { port, .. }) => {
                assert_eq!(port, PortDimension::Channels);
            }
            _ => panic!("expected MissingComposeContext"),
        }
    }

    #[test]
    #[cfg(not(feature = "telegram"))]
    fn test_build_channels_telegram_feature_off_message_unchanged() {
        let ctx = test_compose_ctx();
        let result = build_channels("telegram", None, &ctx);
        match result {
            Err(AdapterCompositionError::MissingComposeContext { missing_field, .. }) => {
                assert_eq!(
                    missing_field,
                    "telegram feature not compiled — profile validator should have rewritten this to 'terminal'"
                );
            }
            _ => panic!("expected MissingComposeContext"),
        }
    }

    #[test]
    #[cfg(feature = "telegram")]
    fn test_build_channels_telegram_with_feature() {
        let mut ctx = test_compose_ctx();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channel_turn_tx = Some(tx);
        let config: toml::Value = r#"
bot_token = "123:abc"
allowed_chat_ids = [123456789]
"#
        .parse()
        .unwrap();
        let channel =
            build_channels("telegram", Some(&config), &ctx).expect("daemon channel composes");
        let health = channel.health_snapshot();
        assert_eq!(health.level, crate::domain::models::HealthLevel::Degraded);
        assert_eq!(health.metric, "connecting");
    }

    #[test]
    #[cfg(feature = "telegram")]
    fn test_build_channels_telegram_requires_allowed_chat_ids_key() {
        let mut ctx = test_compose_ctx();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channel_turn_tx = Some(tx);
        let config: toml::Value = r#"
bot_token = "123:abc"
"#
        .parse()
        .unwrap();
        match build_channels("telegram", Some(&config), &ctx) {
            Err(AdapterCompositionError::MissingComposeContext { missing_field, .. }) => {
                assert_eq!(
                    missing_field,
                    "allowed_chat_ids required in [channels.config]"
                );
            }
            _ => panic!("expected MissingComposeContext"),
        }
    }

    #[test]
    #[cfg(feature = "telegram")]
    fn test_build_channels_telegram_allows_explicit_empty_allowed_chat_ids() {
        let mut ctx = test_compose_ctx();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channel_turn_tx = Some(tx);
        let config: toml::Value = r#"
bot_token = "123:abc"
allowed_chat_ids = []
"#
        .parse()
        .unwrap();
        assert!(build_channels("telegram", Some(&config), &ctx).is_ok());
    }

    #[test]
    #[cfg(not(feature = "slack"))]
    fn test_build_channels_slack_feature_off_fails_closed() {
        let ctx = test_compose_ctx();
        match build_channels("slack", None, &ctx) {
            Err(AdapterCompositionError::MissingComposeContext { missing_field, .. }) => {
                assert_eq!(
                    missing_field,
                    "slack feature not compiled — profile validator should have rewritten this to 'terminal'"
                );
            }
            _ => panic!("expected MissingComposeContext"),
        }
    }

    #[test]
    #[cfg(feature = "slack")]
    fn test_build_channels_slack_outside_the_daemon_reads_nothing_and_starts_nothing() {
        // No turn queue (a TUI or CLI session): the daemon-only notice path, even
        // for an unusable config and with no credential in the environment.
        let ctx = test_compose_ctx();
        let channel = build_channels("slack", None, &ctx).expect("no-op outside the daemon");
        assert!(channel.health_updates().is_none());
        assert_eq!(
            channel.health_snapshot().metric,
            NoOpChannel.health_snapshot().metric
        );
    }

    #[test]
    #[cfg(feature = "slack")]
    fn test_build_channels_slack_daemon_rejects_a_missing_allowlist_before_readiness() {
        let mut ctx = test_compose_ctx();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channel_turn_tx = Some(tx);
        let config: toml::Value = "allowed_user_ids = []".parse().unwrap();
        for conf in [None, Some(&config)] {
            let error = build_channels("slack", conf, &ctx)
                .err()
                .expect("explicit Slack with no usable allowlist is rejected")
                .to_string();
            assert!(!error.to_lowercase().contains("xapp"), "{error}");
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
            _ => panic!("expected UnknownAdapter"),
        }
    }

    // ── Scheduler tests ──

    #[test]
    fn test_build_scheduler_none() {
        let ctx = test_compose_ctx();
        assert!(build_scheduler("none", None, &ctx).is_ok());
    }

    #[test]
    fn test_build_scheduler_cron_falls_back_to_noop() {
        let ctx = test_compose_ctx();
        let scheduler = build_scheduler("cron", None, &ctx).expect("local scheduler composes");
        let health = scheduler.health_snapshot();
        let noop = NoOpScheduler.health_snapshot();
        assert_eq!(
            (health.level, health.metric, health.suggested_action),
            (noop.level, noop.metric, noop.suggested_action)
        );
    }

    #[test]
    fn test_build_scheduler_unknown() {
        let ctx = test_compose_ctx();
        let result = build_scheduler("bogus", None, &ctx);
        match result {
            Err(AdapterCompositionError::UnknownAdapter { port, .. }) => {
                assert_eq!(port, PortDimension::Scheduler);
            }
            _ => panic!("expected UnknownAdapter"),
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
            _ => panic!("expected UnknownAdapter"),
        }
    }

    // ── Story C-3 (AD-1): the daemon's MCP parity rule ──

    fn mcp_spec(id: &str) -> crate::domain::models::McpServerSpec {
        crate::domain::models::McpServerSpec {
            id: id.to_string(),
            transport: crate::domain::models::McpTransport::Stdio,
            command: Some("c3-fixture".to_string()),
            args: Vec::new(),
            env: Default::default(),
            url: None,
            persistent: false,
            source: crate::domain::models::McpServerSource::Workspace,
        }
    }

    #[test]
    fn daemon_mcp_tools_composite_keeps_the_resolved_servers_and_builtin_flag() {
        let got = daemon_mcp_tools("composite", vec![mcp_spec("a"), mcp_spec("b")], false);
        let ids: Vec<&str> = got.mcp_servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(!got.include_builtin);
    }

    #[test]
    fn daemon_mcp_tools_non_composite_is_todays_daemon() {
        for adapter in ["builtin-only", "builtin-full"] {
            let got = daemon_mcp_tools(adapter, vec![mcp_spec("a")], true);
            assert!(got.mcp_servers.is_empty(), "{adapter}");
            assert!(got.include_builtin, "{adapter}");
        }
        // `include_builtin = false` on a non-composite profile does not leak:
        // the daemon's non-composite toolset keeps its builtin tools.
        let got = daemon_mcp_tools("builtin-only", vec![mcp_spec("a")], false);
        assert!(got.include_builtin);
    }
    #[cfg(all(feature = "telegram", feature = "cron"))]
    #[tokio::test]
    async fn c2b_daemon_only_profile_composes_as_noops_outside_daemon() {
        use crate::domain::models::AdapterRef;
        let workspace = tempfile::Builder::new()
            .prefix("c-2b-compose-")
            .tempdir()
            .unwrap();
        let mut ctx = test_compose_ctx();
        ctx.workspace_path = workspace.path().to_path_buf();
        let mut selection = ProfileSelection::default();
        for (port, name) in [
            (PortDimension::Persona, "minimal"),
            (PortDimension::Memory, "noop"),
            (PortDimension::Session, "basic"),
            (PortDimension::Tools, "builtin-only"),
            (PortDimension::Channels, "telegram"),
            (PortDimension::Scheduler, "cron"),
            (PortDimension::Context, "default"),
        ] {
            selection.dimensions.insert(
                port,
                AdapterRef {
                    adapter: name.into(),
                    _config: None,
                },
            );
        }
        selection
            .dimensions
            .get_mut(&PortDimension::Channels)
            .unwrap()
            ._config = Some("allowed_chat_ids = [123]".parse().unwrap());
        let core =
            AgentCore::compose("c2b", &selection, &ctx).expect("daemon-only adapters become noops");
        let channel = core.channels.load().health_snapshot();
        let expected_channel = NoOpChannel.health_snapshot();
        assert_eq!(
            (channel.level, channel.metric, channel.suggested_action),
            (
                expected_channel.level,
                expected_channel.metric,
                expected_channel.suggested_action
            )
        );
        let scheduler = core.scheduler.load().health_snapshot();
        let expected_scheduler = NoOpScheduler.health_snapshot();
        assert_eq!(
            (
                scheduler.level,
                scheduler.metric,
                scheduler.suggested_action
            ),
            (
                expected_scheduler.level,
                expected_scheduler.metric,
                expected_scheduler.suggested_action
            )
        );
        use crate::domain::services::adapter_catalog::AdapterCatalog;
        assert_eq!(
            AdapterCatalog::daemon_only_notice("c2b", &selection).as_deref(),
            Some(
                "Profile 'c2b' uses daemon-only adapters (channels = telegram, scheduler = cron); they run under 'rustain daemon start'. This session uses terminal and none."
            )
        );
        selection
            .dimensions
            .get_mut(&PortDimension::Channels)
            .unwrap()
            .adapter = "terminal".into();
        assert_eq!(
            AdapterCatalog::daemon_only_notice("c2b", &selection).as_deref(),
            Some(
                "Profile 'c2b' uses daemon-only adapters (scheduler = cron); they run under 'rustain daemon start'. This session uses terminal and none."
            )
        );
        selection
            .dimensions
            .get_mut(&PortDimension::Scheduler)
            .unwrap()
            .adapter = "none".into();
        assert_eq!(AdapterCatalog::daemon_only_notice("c2b", &selection), None);
    }
}
