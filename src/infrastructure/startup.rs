use std::sync::Arc;

use anyhow::Result;

use crate::adapters::approval_persistence_toml::ApprovalPersistenceToml;
use crate::adapters::cli::commands::{Cli, Command};
use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::project_context_loader::ProjectContextLoader;
use crate::adapters::security_adapter::SecurityAdapter;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::skill_registry::SkillRegistry;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::tui::terminal;
use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;
use crate::domain::models::{PermissionMode, SandboxPolicy};
use crate::domain::ports::{
    ClipboardPort, PersonaPort, StreamingProvider, SecurityPort, StoragePort, ToolSetPort,
};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::plan_manager::PlanManager;
use crate::domain::services::plan_mode_injector::{DefaultPlanInjector, PlanModeInjector};
use crate::infrastructure::runtime::app_state::AppState;
use crate::infrastructure::runtime::event_loop;
use crate::infrastructure::{config, logging, paths, permission_rules, signals};

/// Error type for subcommand exits where output was already printed.
/// Used by `main.rs` to suppress redundant error display.
#[derive(Debug)]
pub struct SubcommandExit;

impl std::fmt::Display for SubcommandExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "subcommand exited with error")
    }
}

impl std::error::Error for SubcommandExit {}

/// Ordered startup sequence.
/// 1. Parse CLI args
/// 2. Load config
/// 3. Initialize logging
/// 4. Install panic hook
/// 5. Construct provider
/// 6. Setup terminal
/// 7. Enter event loop
pub async fn run() -> Result<()> {
    // 1. Parse CLI args — augment with rich long_version (FR109)
    let cli = {
        use clap::{CommandFactory, FromArgMatches};
        // Leak the version string to get a 'static str required by clap's API.
        // This runs once at startup; the allocation is intentionally permanent.
        let long_ver: &'static str =
            Box::leak(crate::adapters::tui::version_info::version_string().into_boxed_str());
        let cmd = Cli::command().long_version(long_ver);
        let matches = cmd.get_matches();
        Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    };

    // 2. Load config
    let app_config = config::load();

    // 3. Initialize logging (hold guard to flush on drop)
    let _log_guard = logging::init(&cli.log_level)?;
    tracing::info!("Starting rustain...");

    // 4. Install panic hook
    signals::install_panic_hook();

    // 4a. Intercept init/doctor/migrate subcommands BEFORE provider construction and terminal setup
    if let Some(Command::Init) = cli.command {
        return crate::adapters::cli::init::run_init().await;
    }
    if let Some(Command::Doctor { terminal }) = cli.command {
        // Doctor already prints its own summary via display_results().
        // Suppress anyhow error display to avoid duplicate output (DF-045).
        return crate::adapters::cli::doctor::run_doctor(terminal)
            .await
            .map_err(|e| {
                tracing::error!("Doctor subcommand failed: {e}");
                SubcommandExit.into()
            });
    }
    if let Some(Command::Migrate {
        from,
        path,
        yes,
        select,
        dry_run,
    }) = cli.command
    {
        return crate::adapters::cli::migrate::run_migrate(from, path, yes, select, dry_run)
            .await
            .map_err(|e| {
                tracing::error!("Migrate subcommand failed: {e}");
                SubcommandExit.into()
            });
    }

    // 5. Apply model override from env (before provider + event loop, so status bar sees it)
    let mut app_config = app_config;
    if let Some(model_override) =
        crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_DEFAULT_SONNET_MODEL")
    {
        tracing::info!(
            "Model override from ANTHROPIC_DEFAULT_SONNET_MODEL: {}",
            model_override
        );
        app_config.model = model_override;
    }

    // 5a. Construct provider adapter
    let provider: Arc<dyn StreamingProvider> = build_provider(&app_config)?;
    // ArcSwap hot-swap holder: wraps the same Arc so ProviderRouter (Story 7.1b)
    // can swap it atomically. Stored on AppState for future routing.
    let provider_arc_for_swap = Arc::clone(&provider);
    let provider_swap = Arc::new(arc_swap::ArcSwap::from_pointee(provider_arc_for_swap));

    // 5a.2 — ProviderRegistry: catalog of registered providers
    let provider_registry = Arc::new(crate::adapters::provider::ProviderRegistry::new());
    // TODO(S7.1b): register providers from config instead of hard-coding Anthropic
    provider_registry.register(Box::new(crate::adapters::noop::NoOpProvider));
    // Run health check; failures emit a warning notice but do not block startup (AC4)
    match provider.health_check().await {
        Ok(()) => {
            tracing::info!("Provider '{}' health check passed", provider.provider_id());
        }
        Err(e) => {
            tracing::warn!("Provider '{}' health check failed: {e}", provider.provider_id());
        }
    }

    // 5b. Construct security and toolset adapters
    let workspace_path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;

    // 6. Create AppState (owns EventBus + CancellationToken + ApprovalRuntime)
    let raw_capacity = app_config.runtime.event_bus.raw_capacity;
    let user_config = paths::config_dir()
        .unwrap_or_else(|_| workspace_path.join(".rustain"))
        .join("config.toml");
    let workspace_rules = workspace_path.join(".rustain").join("permissions.toml");
    let persistence = Arc::new(ApprovalPersistenceToml::new(
        user_config.clone(),
        workspace_rules.clone(),
    ));
    let approval_runtime = ApprovalRuntime::new(raw_capacity, persistence);
    approval_runtime.load_session().await;
    if let Ok(ruleset) = permission_rules::load_rules(&user_config, &workspace_rules) {
        let seed = ruleset.seed_session();
        approval_runtime.seed_session(seed).await;
    }
    let plans_dir = workspace_path.join(".rustain").join("plans");
    let plan_manager = Arc::new(PlanManager::new(plans_dir));
    let plan_injector = Arc::new(DefaultPlanInjector::new());

    let initial_mode = if app_config.default_plan_mode {
        PermissionMode::Plan
    } else {
        PermissionMode::Normal
    };
    let sandbox_policy = SandboxPolicy::from_mode(initial_mode, &workspace_path);

    let (app_state, domain_rx) = AppState::new(
        raw_capacity,
        approval_runtime.clone(),
        sandbox_policy,
        plan_manager.clone(),
        plan_injector.clone(),
        provider_swap,
    );
    let domain_tx = app_state.event_bus.domain_tx.clone();
    let security_adapter = SecurityAdapter::new(workspace_path.clone());
    security_adapter.set_mode(initial_mode);
    let security: Arc<dyn SecurityPort> = Arc::new(security_adapter);

    if app_config.default_plan_mode {
        let _ = plan_manager.ensure_dir().await;
        plan_injector.as_ref().reset_reentry();
    }
    // Storage must be constructed before ToolSetAdapter (Story 4-3b: tools use storage for snapshots).
    // Story 4-3b P2: pass the real workspace root so `snapshot_file` can enforce
    // path-traversal checks without falling back to the sessions_dir grandparent proxy.
    let tools_sessions_dir = paths::sessions_dir(&workspace_path);
    let tools_storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        tools_sessions_dir.clone(),
        workspace_path.clone(),
    ));
    let shared_skill_registry = Arc::new(tokio::sync::RwLock::new(SkillRegistry::new()));
    let skill_activator = Arc::new(SkillActivator::with_registry(shared_skill_registry));
    skill_activator.set_event_tx(domain_tx.clone()).await;
    let agent_activator = Arc::new(crate::adapters::agent_activation::AgentActivator::new(
        Arc::clone(&security),
    ));
    let mut tools_adapter = ToolSetAdapter::new(workspace_path.clone(), Arc::clone(&tools_storage));
    tools_adapter.set_activator(Arc::clone(&skill_activator));
    tools_adapter.set_plan_manager(plan_manager.clone());
    tools_adapter.set_event_tx(domain_tx.clone());
    let tools: Arc<dyn ToolSetPort> = Arc::new(tools_adapter);

    // 5c. Discover and load project context
    let context_loader = ProjectContextLoader::new(workspace_path.clone());
    let project_context = context_loader.discover().unwrap_or_else(|e| {
        tracing::warn!("Failed to discover project context: {}", e);
        crate::domain::models::project_context::ProjectContext::empty()
    });
    let persona_adapter = PersonaAdapter::new(project_context);

    // Emit context loading notices (Phase D: Task 7)
    if persona_adapter.has_context() {
        let paths: Vec<String> = persona_adapter
            .file_paths()
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let msg = format!(
            "Project context: {} ({} chars)",
            paths.join(", "),
            persona_adapter.total_chars(),
        );
        tracing::info!("{}", msg);
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: msg,
        });

        if persona_adapter.is_truncated() {
            let warn_msg = format!(
                "Project context truncated: some files omitted (budget: {} chars)",
                crate::domain::models::project_context::CONTEXT_BUDGET_CHARS,
            );
            tracing::warn!("{}", warn_msg);
            let _ = domain_tx.send(AppEvent::SystemNotice {
                conversation_id: None,
                level: NoticeLevel::Warning,
                message: warn_msg,
            });
        }
    }

    let persona: Arc<dyn PersonaPort> = Arc::new(persona_adapter);

    // 5d. Use the same storage adapter constructed above for session management.
    // Both tools and the event loop share one FileSystemStorage instance pointing
    // to the same sessions directory (Story 4-3b: snapshots are co-located with conversations).
    let sessions_dir = tools_sessions_dir.clone();
    // Downcast to FileSystemStorage to access ensure_dir (concrete method).
    // Mirror the workspace_root configuration from `tools_storage` above.
    // AC1: CLI --snapshot-retention takes precedence over config file value.
    let retention = cli
        .snapshot_retention
        .or(app_config.snapshot_retention_count);
    let storage = FileSystemStorage::with_workspace_root(sessions_dir, workspace_path.clone())
        .with_snapshot_retention(retention);
    if let Err(e) = storage.ensure_dir().await {
        tracing::warn!("Failed to create sessions directory: {}", e);
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: format!("Session persistence unavailable: {}", e),
        });
    }

    // DF-109 (AC3): Reconcile any rewind transactions that were interrupted by a crash.
    // Must run before session restoration so that recovered conversations are in a
    // consistent state when we attempt to load them.
    use crate::domain::ports::StoragePort as _;
    if let Err(e) = storage.reconcile_pending_txns().await {
        tracing::warn!("Failed to reconcile pending rewind transactions: {}", e);
    }

    // Session restoration: --new skips restore, --session <id> loads specific session
    // recovery_prompt: Some((title, token_count)) if crash detected
    let (restored_conversation, recovery_prompt) = if cli.new {
        tracing::info!("Starting new session (--new flag)");
        (None, None)
    } else if let Some(ref session_id) = cli.session {
        // Validate --session <id> exists BEFORE terminal setup
        match storage.load_conversation_with_exit(session_id).await {
            Ok(Some((conv, _clean_exit))) => {
                tracing::info!(
                    "Restored specific session: {} ({} messages)",
                    conv.title.as_str(),
                    conv.messages.len()
                );
                // Don't show recovery prompt for explicit --session restore
                (Some(conv), None)
            }
            Ok(None) => {
                eprintln!("Error: session '{}' not found", session_id);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error: failed to load session '{}': {}", session_id, e);
                std::process::exit(1);
            }
        }
    } else {
        // Default: load most recent session with crash detection
        match storage.list_conversations().await {
            Ok(summaries) if !summaries.is_empty() => {
                let most_recent = &summaries[0];
                match storage.load_conversation_with_exit(&most_recent.id).await {
                    Ok(Some((conv, clean_exit))) => {
                        tracing::info!(
                            "Restored session: {} ({} messages, clean_exit={})",
                            conv.title.as_str(),
                            conv.messages.len(),
                            clean_exit,
                        );
                        let recovery = if !clean_exit && !conv.messages.is_empty() {
                            // Crash detected: prepare recovery prompt info
                            let title = if conv.title.is_empty() {
                                "Untitled".to_string()
                            } else {
                                conv.title.clone()
                            };
                            let token_count = conv
                                .messages
                                .last()
                                .and_then(|m| m.token_count)
                                .unwrap_or(0);
                            Some((title, token_count))
                        } else {
                            None
                        };
                        (Some(conv), recovery)
                    }
                    Ok(None) => {
                        tracing::warn!("Session file listed but not loadable");
                        (None, None)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load last session: {}", e);
                        (None, None)
                    }
                }
            }
            _ => (None, None),
        }
    };

    let storage = Arc::new(storage);
    let storage_port: Arc<dyn StoragePort> = storage.clone();

    signals::set_shutdown_sender(app_state.event_bus.domain_tx.clone());
    signals::set_session_cancel(app_state.session_cancel.clone());
    signals::install_signal_handlers().await;

    // 5e. Construct clipboard adapter
    #[cfg(feature = "clipboard")]
    let clipboard: Arc<dyn ClipboardPort> =
        Arc::new(crate::adapters::clipboard_adapter::ArboardClipboard::new());
    #[cfg(not(feature = "clipboard"))]
    let clipboard: Arc<dyn ClipboardPort> =
        Arc::new(crate::adapters::clipboard_adapter::NoOpClipboard::new());

    // 7. Setup terminal
    let mut tui = terminal::setup()?;

    let result = event_loop::run(
        &mut tui,
        domain_rx,
        app_state,
        &app_config,
        provider,
        security,
        tools,
        persona,
        storage_port,
        storage,
        clipboard,
        workspace_path,
        restored_conversation,
        recovery_prompt,
        skill_activator,
        agent_activator,
        approval_runtime,
    )
    .await;

    // 9. Teardown terminal (always, even on error)
    if let Err(e) = terminal::teardown() {
        tracing::error!("Terminal teardown failed: {}", e);
    }
    tracing::info!("Rustain shutdown complete.");

    result
}

/// Build the provider adapter based on configuration and environment.
///
/// Auth precedence (CC-compatible): `ANTHROPIC_AUTH_TOKEN` > `ANTHROPIC_API_KEY`.
/// - `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer {token}` (gateways/proxies)
/// - `ANTHROPIC_API_KEY` → `X-Api-Key: {key}` (direct Anthropic)
fn build_provider(config: &crate::domain::models::AppConfig) -> Result<Arc<dyn StreamingProvider>> {
    #[cfg(feature = "anthropic")]
    {
        use crate::adapters::anthropic::AuthMode;

        // 1. Resolve auth: ANTHROPIC_AUTH_TOKEN > ANTHROPIC_API_KEY (CC precedence)
        let auth_token = crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_AUTH_TOKEN");
        let api_key = crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_API_KEY");

        if auth_token.is_some() && api_key.is_some() {
            tracing::warn!(
                "Both ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY are set; using ANTHROPIC_AUTH_TOKEN (Bearer auth)"
            );
        }

        let auth_mode = if let Some(token) = auth_token {
            tracing::info!("Using ANTHROPIC_AUTH_TOKEN (Bearer auth)");
            AuthMode::BearerToken(token)
        } else if let Some(key) = api_key {
            tracing::info!("Using ANTHROPIC_API_KEY (X-Api-Key auth)");
            AuthMode::ApiKey(key)
        } else {
            anyhow::bail!(
                "No API key found.\n\n\
                 Set one of:\n\
                 \n\
                 export ANTHROPIC_API_KEY=sk-ant-...       # Direct Anthropic\n\
                 export ANTHROPIC_AUTH_TOKEN=your-key       # Anthropic-compatible gateway\n\
                 \n\
                 Get your API key at: https://console.anthropic.com/"
            );
        };

        // 2. Resolve base URL (filter empty to preserve default)
        let base_url = crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_BASE_URL");
        if let Some(ref url) = base_url {
            tracing::info!("Custom base URL: {}", url);
        }

        let adapter = crate::adapters::anthropic::AnthropicAdapter::new(
            auth_mode,
            config.model.clone(),
            base_url,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create Anthropic adapter: {}", e))?;
        tracing::info!("Anthropic provider initialized (model: {})", config.model);
        Ok(Arc::new(adapter))
    }

    #[cfg(not(feature = "anthropic"))]
    {
        let _ = config;
        tracing::warn!("No provider feature enabled — using NoOp provider");
        Ok(Arc::new(crate::adapters::noop::NoOpProvider))
    }
}
