use std::sync::Arc;

use anyhow::Result;

use crate::adapters::approval_persistence_toml::ApprovalPersistenceToml;
use crate::adapters::cli::commands::{Cli, Command};
use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::ledger::FileUsageLedger;
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::project_context_loader::ProjectContextLoader;
use crate::adapters::security_adapter::SecurityAdapter;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::skill_registry::SkillRegistry;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::tui::terminal;
use crate::domain::errors::ProviderError;
use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;
use crate::domain::models::{PermissionMode, ProviderConfig, SandboxPolicy};
use crate::domain::ports::{
    ClipboardPort, PersonaPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
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
/// 2. Initialize logging (so config warnings are captured)
/// 3. Load config
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

    // 2. Initialize logging BEFORE config load so parse warnings are captured
    let _log_guard = logging::init(&cli.log_level)?;
    tracing::info!("Starting rustain...");

    // 3. Load config
    let app_config = config::load();

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
    #[cfg(feature = "openai")]
    if let Some(Command::UpdateCatalog { output, provider }) = cli.command {
        return crate::adapters::cli::update_catalog::run_update_catalog(output, provider)
            .await
            .map_err(|e| {
                tracing::error!("UpdateCatalog subcommand failed: {e}");
                SubcommandExit.into()
            });
    }
    #[cfg(not(feature = "openai"))]
    if let Some(Command::UpdateCatalog { .. }) = cli.command {
        anyhow::bail!(
            "update-catalog requires the 'openai' feature — rebuild with --features openai"
        );
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

    // 5a. Construct provider layer
    let ProviderLayer {
        router,
        registry: provider_registry,
        deferred_notices,
        active_id: _active_id,
        unsupported_discovery,
        discovery_targets,
    } = init_provider_layer(&app_config);
    #[cfg(not(feature = "openai"))]
    let _ = &discovery_targets; // suppress unused-variable warning on non-openai builds

    // ArcSwap hot-swap holder wraps the router (not a bare adapter)
    let provider_swap = Arc::new(arc_swap::ArcSwap::from_pointee(
        router.clone() as Arc<dyn StreamingProvider>
    ));

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

    let usage_ledger: Arc<dyn crate::domain::ports::UsageLedgerPort> =
        Arc::new(FileUsageLedger::new());

    // Story 7.5 AC7 — load BudgetState (dismissed-until) once at startup.
    let budget_state_store = Arc::new(crate::adapters::budget::BudgetStateStore::new());

    let refresh_tracker = crate::adapters::tui::refresh_tracker::RefreshTracker::new();

    let (app_state, domain_rx) = AppState::new(
        raw_capacity,
        approval_runtime.clone(),
        sandbox_policy,
        plan_manager.clone(),
        plan_injector.clone(),
        provider_swap,
        provider_registry.clone(),
        usage_ledger,
        budget_state_store,
    );
    let domain_tx = app_state.event_bus.domain_tx.clone();

    // Story 7.6 AC7 — emit startup toast for providers that don't support discovery
    for (id, kind) in &unsupported_discovery {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: format!(
                "{} doesn't support model discovery — using config.toml list",
                kind
            ),
        });
        tracing::warn!(
            "Provider '{}' (kind={}) uses a static catalog — dynamic discovery is not yet supported.",
            id,
            kind
        );
    }

    #[cfg(feature = "openai")]
    {
        // Story 7.7 AC1/AC6 — Tier-0 JSON seed from embedded models_variants.json (zero I/O)
        if let Some(seed_catalog) =
            crate::adapters::model_catalog_cache::load_embedded_seed()
        {
            for target in &discovery_targets {
                if let Some(entry) = seed_catalog.providers.get(&target.provider_id) {
                    target.adapter.set_discovered_models(entry.models.clone());
                    tracing::info!(
                        "Tier-0 seed: JSON catalog for '{}' ({} models)",
                        target.provider_id,
                        entry.models.len()
                    );
                }
            }
        } else {
            tracing::error!(
                "Failed to parse embedded models_variants.json — catalog seed unavailable"
            );
        }

        // Story 7.6 AC4/AC5 — Tier-1 disk cache seed BEFORE health check (synchronous, ≤10ms)
        let cache = crate::adapters::model_catalog_cache::ModelCatalogCache::new();
        let cached = cache.load().await;

        for target in &discovery_targets {
            if let Some(entry) = cached.providers.get(&target.provider_id) {
                target.adapter.set_discovered_models(entry.models.clone());
                tracing::info!("Tier-1 seed: cached catalog for '{}'", target.provider_id);
            }
        }
    }

    // D2: Health check — emit TUI warning notice on failure and update registry (AC4)
    // Health-check ALL registered providers and emit notices for failures.
    let all_provider_ids: Vec<String> = provider_registry.provider_ids().into_iter().collect();
    for id in &all_provider_ids {
        if let Some(adapter) = router.get_provider(id) {
            match adapter.health_check().await {
                Ok(()) => {
                    tracing::info!("Provider '{}' health check passed", id);
                    provider_registry.update_health(id, true);
                }
                Err(e) => {
                    tracing::warn!("Provider '{}' health check failed: {}", id, e);
                    provider_registry.update_health(id, false);
                    let (level, message) = match e {
                        ProviderError::ConnectionFailed(ref msg) => {
                            (NoticeLevel::Error, msg.clone())
                        }
                        _ => (
                            NoticeLevel::Warning,
                            format!("Provider '{}' unavailable: {}", id, e),
                        ),
                    };
                    let _ = domain_tx.send(AppEvent::SystemNotice {
                        conversation_id: None,
                        level,
                        message,
                    });
                }
            }
        }
    }

    // Flush deferred construction-failure notices
    for (id, e) in &deferred_notices {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: format!("Failed to construct provider '{}': {}", id, e),
        });
    }

    #[cfg(feature = "openai")]
    {
        // Story 7.6 AC4/AC5 — Tier-2 background refresh AFTER health check (non-blocking)
        let cache = crate::adapters::model_catalog_cache::ModelCatalogCache::new();
        let cached = cache.load().await;

        // Clone before the for-loop consumes it (used by periodic timer below)
        let discovery_targets_periodic = discovery_targets.clone();
        let refresh_tracker_clone = refresh_tracker.clone();
        for target in discovery_targets {
            let cache = cache.clone();
            let tracker = refresh_tracker_clone.clone();
            let domain_tx = domain_tx.clone();
            let provider_id = target.provider_id.clone();
            let adapter = target.adapter.clone();
            let model_filter = target.model_filter.clone();
            let ttl = target.cache_ttl_seconds;

            // Check freshness before spawning
            let is_fresh = cached.providers.get(&provider_id).is_some_and(|entry| {
                cache.is_fresh(entry, ttl, crate::infrastructure::clock_util::now_unix())
            });

            if is_fresh {
                tracing::debug!(
                    "catalog cache fresh for '{}'; skipping refresh",
                    provider_id
                );
                continue;
            }

            tokio::spawn(async move {
                let _guard = tracker.insert(provider_id.clone());
                match adapter.fetch_remote_models(&model_filter).await {
                    Ok(models) => {
                        if models.is_empty() {
                            tracing::warn!("Empty catalog from '{}'; not caching", provider_id);
                            let _ = domain_tx.send(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Warning,
                                message: format!(
                                    "Model catalog for '{}' returned empty — showing bundled models",
                                    provider_id
                                ),
                            });
                            return;
                        }
                        // Serialize cache writes so concurrent providers don't overwrite each other.
                        let _lock = cache.lock().await;
                        let mut catalog = cache.load().await;
                        let models_with_stale =
                            crate::adapters::model_catalog_cache::merge_with_live(
                                catalog.providers.get(&provider_id),
                                &models,
                            );
                        adapter.set_discovered_models(models_with_stale.clone());
                        catalog.providers.insert(
                            provider_id.clone(),
                            crate::adapters::model_catalog_cache::CachedProviderEntry {
                                fetched_at_unix: crate::infrastructure::clock_util::now_unix(),
                                models: models_with_stale,
                            },
                        );
                        if let Err(e) = cache.save(&catalog).await {
                            tracing::warn!("models_cache.json save failed: {}", e);
                        }
                        let _ = domain_tx.send(AppEvent::ProviderCatalogRefreshed { provider_id }); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: Story 7.6 AC8 — live refresh redraw signal
                    }
                    Err(e) => {
                        tracing::warn!(
                            "model discovery for '{}' failed: {}; using cached/bundled catalog",
                            provider_id,
                            e
                        );
                        let _ = domain_tx.send(AppEvent::SystemNotice {
                            conversation_id: None,
                            level: NoticeLevel::Warning,
                            message: format!(
                                "Model catalog refresh for '{}' failed: {} — showing bundled/cached models",
                                provider_id, e
                            ),
                        });
                    }
                }
            });
        }

        // Story 7.7 AC3 — periodic auto-refresh timer (4h intervals, UTC-aligned)
        spawn_periodic_catalog_refresh(
            cache.clone(),
            discovery_targets_periodic,
            refresh_tracker.clone(),
            domain_tx.clone(),
        );
    }

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

    // Story 16.9: construct progress channel when live_tail is enabled
    let (progress_tx, progress_rx) = if app_config.tool_progress.live_tail {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tools_adapter.set_progress_tx(Some(tx.clone())).await;
        tools_adapter
            .set_tool_progress_config(app_config.tool_progress.clone())
            .await;
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

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

    // 7. Setup terminal (mouse capture gated by config + RUSTAIN_NO_MOUSE env. Story 16.8, AC14)
    let mouse_enabled = app_config.mouse.capture
        && crate::infrastructure::utils::env_var_trimmed("RUSTAIN_NO_MOUSE")
            != Some("1".to_string());
    let mut tui = terminal::setup(mouse_enabled)?;

    // P13: AC14 first-launch hint — if mouse capture is active, inform the user.
    if mouse_enabled {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "Mouse scroll enabled. Hold Shift to select text for copy.".to_string(),
        });
    }

    let result = event_loop::run(
        &mut tui,
        domain_rx,
        app_state,
        &app_config,
        router.clone(),
        router.clone(),
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
        progress_tx,
        progress_rx,
        Some(refresh_tracker),
    )
    .await;

    // 9. Teardown terminal (always, even on error)
    if let Err(e) = terminal::teardown(mouse_enabled) {
        tracing::error!("Terminal teardown failed: {}", e);
    }
    tracing::info!("Rustain shutdown complete.");

    result
}

/// Extract the provider construction logic for testability.
/// Use named fields — do NOT revert to a tuple alias. Story 7.6 amendment.
pub struct ProviderLayer {
    pub router: Arc<crate::adapters::provider::ProviderRouter>,
    pub registry: Arc<crate::adapters::provider::ProviderRegistry>,
    pub deferred_notices: Vec<(String, ProviderError)>,
    pub active_id: String,
    /// Providers where `discover_models = true` but the kind doesn't support it (Story 7.6 AC7).
    pub unsupported_discovery: Vec<(String, String)>, // (provider_id, kind)
    #[cfg(feature = "openai")]
    pub discovery_targets: Vec<crate::adapters::model_catalog_cache::DiscoveryTarget>,
    #[cfg(not(feature = "openai"))]
    pub discovery_targets: Vec<()>,
}

pub fn init_provider_layer(app_config: &crate::domain::models::AppConfig) -> ProviderLayer {
    // Clear the openai adapter cache so stale references from previous sessions
    // (e.g., hot-reload in tests) don't leak into the new ProviderLayer.
    #[cfg(feature = "openai")]
    crate::infrastructure::provider_factory::clear_openai_adapters();

    let provider_registry = Arc::new(crate::adapters::provider::ProviderRegistry::new());
    let router = Arc::new(crate::adapters::provider::ProviderRouter::new(
        "anthropic".to_string(),
    ));
    let mut deferred_notices: Vec<(String, ProviderError)> = Vec::new();
    let mut unsupported_discovery: Vec<(String, String)> = Vec::new();

    #[cfg(feature = "openai")]
    let mut discovery_targets: Vec<crate::adapters::model_catalog_cache::DiscoveryTarget> =
        Vec::new();

    let enabled_configs: Vec<(&String, &ProviderConfig)> = app_config
        .provider
        .iter()
        .filter(|(_id, cfg)| cfg.enabled)
        .collect();

    let use_config_path = !app_config.provider.is_empty() && !enabled_configs.is_empty();

    let active_id = if use_config_path {
        let mut first_enabled_id: Option<String> = None;
        for (id, cfg) in enabled_configs {
            if id != &cfg.provider_id {
                tracing::warn!(
                    "Provider config key '{}' does not match provider_id '{}'; using key",
                    id,
                    cfg.provider_id
                );
            }

            // Build provider FIRST; only add discovery target if construction succeeds (Story 7.6 AC5).
            let _provider_built =
                match crate::infrastructure::provider_factory::build_provider_for_config(id, cfg) {
                    Ok(adapter) => {
                        let adapter_arc = Arc::clone(&adapter);
                        router.register(adapter);
                        provider_registry.register_arc(adapter_arc);
                        if first_enabled_id.is_none() {
                            first_enabled_id = Some(id.clone());
                        }
                        tracing::info!("Provider '{}' registered from config", id);
                        true
                    }
                    Err(e) => {
                        tracing::warn!("Failed to construct provider '{}': {}", id, e);
                        deferred_notices.push((id.clone(), e));
                        false
                    }
                };

            #[cfg(feature = "openai")]
            // Build typed OpenAI adapter for discovery (Story 7.6 AC5)
            if _provider_built && cfg.discover_models {
                match crate::infrastructure::provider_factory::build_openai_for_discovery(id, cfg) {
                    Ok(Some(adapter)) => {
                        discovery_targets.push(
                            crate::adapters::model_catalog_cache::DiscoveryTarget {
                                provider_id: id.clone(),
                                adapter,
                                cache_ttl_seconds: cfg.cache_ttl_seconds,
                                model_filter: cfg.model_filter.clone(),
                            },
                        );
                    }
                    Ok(None) => {
                        // Anthropic or Ollama — warn that discovery is not supported
                        tracing::warn!(
                            "Provider '{}' (kind={}) uses a static catalog — dynamic discovery is not yet supported. \
                             Edit [providers.{}] in config.toml to remove discover_models, or accept the static list.",
                            id,
                            cfg.kind.as_deref().unwrap_or(id),
                            id
                        );
                        unsupported_discovery
                            .push((id.clone(), cfg.kind.as_deref().unwrap_or(id).to_string()));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to build discovery adapter for '{}': {}", id, e);
                    }
                }
            }
        }
        first_enabled_id.unwrap_or_else(|| "anthropic".to_string())
    } else {
        if !app_config.provider.is_empty() {
            tracing::info!(
                "No enabled providers in [provider] config; using legacy ANTHROPIC env-var path"
            );
        }
        match build_anthropic_provider_from_env(app_config) {
            Ok(adapter) => {
                let adapter_arc = Arc::clone(&adapter);
                router.register(adapter);
                provider_registry.register_arc(adapter_arc);
                "anthropic".to_string()
            }
            Err(e) => {
                tracing::warn!("Legacy Anthropic fallback failed: {}", e);
                deferred_notices
                    .push(("anthropic".to_string(), ProviderError::Other(e.to_string())));
                "anthropic".to_string()
            }
        }
    };

    if let Err(e) = router.set_active(&active_id) {
        tracing::warn!("Failed to set active provider '{}': {}", active_id, e);
    }

    if provider_registry.provider_ids().is_empty() {
        tracing::warn!(
            "No providers registered — rustain will launch but all completion requests will fail. \
             Configure providers via `rustain init` or add [provider.*] sections to config."
        );
    }

    #[cfg(feature = "openai")]
    {
        ProviderLayer {
            router,
            registry: provider_registry,
            deferred_notices,
            active_id,
            unsupported_discovery,
            discovery_targets,
        }
    }
    #[cfg(not(feature = "openai"))]
    {
        ProviderLayer {
            router,
            registry: provider_registry,
            deferred_notices,
            active_id,
            unsupported_discovery,
            discovery_targets: Vec::new(),
        }
    }
}

/// Build the Anthropic provider from environment variables (legacy fallback).
///
/// Auth precedence (CC-compatible): `ANTHROPIC_AUTH_TOKEN` > `ANTHROPIC_API_KEY`.
/// - `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer {token}` (gateways/proxies)
/// - `ANTHROPIC_API_KEY` → `X-Api-Key: {key}` (direct Anthropic)
fn build_anthropic_provider_from_env(
    config: &crate::domain::models::AppConfig,
) -> Result<Arc<dyn StreamingProvider>> {
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

/// Spawn a background periodic catalog refresh timer (Story 7.7 AC3).
///
/// Fires every 4 hours aligned to UTC hour boundaries (00:00, 04:00, 08:00, ...).
/// On each tick, re-fetches `/v1/models` for every provider with `discover_models = true`.
/// Emits `ProviderCatalogRefreshed` on success, `SystemNotice` on failure.
#[cfg(feature = "openai")]
fn spawn_periodic_catalog_refresh(
    cache: crate::adapters::model_catalog_cache::ModelCatalogCache,
    discovery_targets: Vec<crate::adapters::model_catalog_cache::DiscoveryTarget>,
    refresh_tracker: std::sync::Arc<crate::adapters::tui::refresh_tracker::RefreshTracker>,
    domain_tx: tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
) {
    if discovery_targets.is_empty() {
        return;
    }

    tokio::spawn(async move {
        use chrono::Timelike;

        // Align to next UTC 4h boundary
        let now = chrono::Utc::now();
        let current_hour = now.hour();
        let next_boundary_hour = ((current_hour / 4) + 1) * 4;
        let next_boundary = if next_boundary_hour >= 24 {
            // Roll to next day at 00:00 UTC
            now.date_naive()
                .succ_opt()
                .unwrap_or(now.date_naive())
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
                .unwrap_or(now + chrono::Duration::hours(4))
        } else {
            now.date_naive()
                .and_hms_opt(next_boundary_hour, 0, 0)
                .map(|dt| dt.and_utc())
                .unwrap_or(now + chrono::Duration::hours(4))
        };

        let until_first = (next_boundary - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(3600 * 4));
        tracing::info!(
            "Periodic catalog refresh: first tick in {:.1}m (next UTC boundary {:02}:00)",
            until_first.as_secs_f64() / 60.0,
            next_boundary_hour % 24,
        );

        tokio::time::sleep(until_first).await;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600 * 4));
        // First tick fires immediately after sleep
        interval.tick().await;

        loop {
            interval.tick().await;
            tracing::info!("Periodic catalog refresh tick");

            for target in &discovery_targets {
                let provider_id = target.provider_id.clone();
                let adapter = target.adapter.clone();
                let model_filter = target.model_filter.clone();
                let cache = cache.clone();
                let domain_tx = domain_tx.clone();
                let tracker = refresh_tracker.clone();

                tokio::spawn(async move {
                    let _guard = tracker.insert(provider_id.clone());
                    match adapter.fetch_remote_models(&model_filter).await {
                        Ok(models) => {
                            if models.is_empty() {
                                tracing::warn!(
                                    "Periodic refresh: empty catalog from '{}'",
                                    provider_id
                                );
                                let _ = domain_tx.send(AppEvent::SystemNotice {
                                    conversation_id: None,
                                    level: NoticeLevel::Warning,
                                    message: format!(
                                        "Model catalog for '{}' returned empty — keeping current models",
                                        provider_id
                                    ),
                                });
                                return;
                            }
                            let _lock = cache.lock().await;
                            let mut catalog = cache.load().await;
                            let models_with_stale =
                                crate::adapters::model_catalog_cache::merge_with_live(
                                    catalog.providers.get(&provider_id),
                                    &models,
                                );
                            adapter.set_discovered_models(models_with_stale.clone());
                            catalog.providers.insert(
                                provider_id.clone(),
                                crate::adapters::model_catalog_cache::CachedProviderEntry {
                                    fetched_at_unix: crate::infrastructure::clock_util::now_unix(),
                                    models: models_with_stale,
                                },
                            );
                            if let Err(e) = cache.save(&catalog).await {
                                tracing::warn!("Periodic refresh save failed: {}", e);
                            }
                            let _ =
                                domain_tx.send(AppEvent::ProviderCatalogRefreshed { provider_id }); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: Story 7.7 AC3 — periodic refresh redraw signal
                        }
                        Err(e) => {
                            tracing::warn!("Periodic refresh for '{}' failed: {}", provider_id, e);

                            // AC3: mark existing models as stale on refresh failure
                            let current = adapter.list_models();
                            let stale_entries: Vec<
                                crate::adapters::model_catalog_cache::CachedModelEntry,
                            > = current
                                .into_iter()
                                .map(|mut m| {
                                    m.stale = true;
                                    crate::adapters::model_catalog_cache::CachedModelEntry {
                                        descriptor: m,
                                    }
                                })
                                .collect();
                            if !stale_entries.is_empty() {
                                adapter.set_discovered_models(stale_entries);
                            }

                            let _ = domain_tx.send(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Warning,
                                message: format!(
                                    "Model catalog refresh for '{}' failed: {} — showing current models",
                                    provider_id, e
                                ),
                            });
                        }
                    }
                });
            }
        }
    });
}
