//! Composition root for `AgentCore` — dispatches profile adapter names
//! to concrete adapter constructors. One factory per port dimension.

use std::sync::Arc;

use crate::adapters::noop::{NoOpChannel, NoOpContext, NoOpMemory, NoOpScheduler, NoOpSession};
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::domain::errors::AdapterCompositionError;
use crate::domain::models::profile::{AdapterRef, PortDimension, ProfileSelection};
use crate::domain::models::project_context::ProjectContext;
use crate::domain::ports::{
    ChannelPort, ContextPort, MemoryPort, PersonaPort, SchedulerPort, SessionPort, StoragePort,
    ToolSetPort,
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
    _ctx: &ComposeContext,
) -> Result<Arc<dyn MemoryPort>, AdapterCompositionError> {
    match name {
        "noop" => Ok(Arc::new(NoOpMemory)),
        "project-scoped" | "daily-log" => {
            tracing::warn!(
                port = ?PortDimension::Memory,
                adapter = %name,
                "Placeholder adapter — real implementation deferred to Epic 12+"
            );
            Ok(Arc::new(NoOpMemory))
        }
        other => Err(AdapterCompositionError::UnknownAdapter {
            port: PortDimension::Memory,
            name: other.to_string(),
            available: vec!["noop".into(), "project-scoped".into(), "daily-log".into()],
        }),
    }
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
            let adapter = ToolSetAdapter::new(ctx.workspace_path.clone(), Arc::clone(&ctx.storage));
            Ok(Arc::new(adapter))
        }
        "builtin-full" => {
            let mut adapter =
                ToolSetAdapter::new(ctx.workspace_path.clone(), Arc::clone(&ctx.storage));
            adapter.set_activator(Arc::clone(&ctx.skill_activator));
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
                    Arc::new(crate::adapters::mcp::client::McpClientAdapter::new(
                        spec.clone(),
                    ))
                })
                .collect();
            let adapter = crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter::new(
                builtin,
                mcp_clients,
                ctx.mcp_servers.clone(),
                ctx.include_builtin_tools,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::noop::NoOpStorage;
    use std::path::PathBuf;

    fn test_compose_ctx() -> ComposeContext {
        ComposeContext {
            workspace_path: PathBuf::from("/tmp/test"),
            project_context: ProjectContext::empty(),
            storage: Arc::new(NoOpStorage::default()) as Arc<dyn StoragePort>,
            skill_activator: Arc::new(SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
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
